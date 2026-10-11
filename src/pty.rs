//! Platform boundary for PTY ownership, launching, I/O and pixel geometry.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod native;
#[cfg(any(
    test,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
pub(crate) mod portable;

#[derive(Clone, Copy)]
pub(crate) struct Dimensions {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

pub(crate) enum Master {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    Native(std::sync::Arc<fuxix::pty::Master>),
    #[cfg(any(
        test,
        not(any(target_os = "linux", target_os = "android", target_os = "macos"))
    ))]
    Portable(portable::Master),
}

impl Master {
    pub(crate) fn resize(&self, size: Dimensions) -> anyhow::Result<()> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            Self::Native(master) => native::resize(master, size),
            #[cfg(any(
                test,
                not(any(target_os = "linux", target_os = "android", target_os = "macos"))
            ))]
            Self::Portable(master) => master.handle().resize(portable_pty::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            }),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Child {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    Native {
        child: std::process::Child,
        pid: fuxix::process::Pid,
        master: std::sync::Arc<fuxix::pty::Master>,
    },
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    Portable(Box<dyn portable_pty::Child + Send + Sync>),
}

impl Child {
    pub(crate) fn id(&self) -> Option<u32> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            Self::Native { child, .. } => Some(child.id()),
            #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
            Self::Portable(child) => child.process_id(),
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<u32>> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            Self::Native { child, .. } => {
                use std::os::unix::process::ExitStatusExt;
                child.try_wait().map(|status| {
                    status.map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)) as u32)
                })
            }
            #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
            Self::Portable(child) => child.try_wait().map(|status| status.map(|s| s.exit_code())),
        }
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            Self::Native { child, pid, master } => {
                // The session leader remains our unreaped child, so its PID
                // cannot be recycled. Stop a separate foreground job as well.
                if let Some(group) = fuxix::terminal::foreground_group(&**master) {
                    let _ = fuxix::process::kill_group(group, fuxix::process::Signal::Kill);
                }
                let _ = fuxix::process::kill_group(*pid, fuxix::process::Signal::Kill);
                child.kill()
            }
            #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
            Self::Portable(child) => child.kill(),
        }
    }
}

pub(crate) struct Opened {
    pub master: Master,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Child,
}

pub(crate) fn spawn(
    size: Dimensions,
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    launcher: &Path,
) -> anyhow::Result<Opened> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        native::spawn(size, argv, env, cwd, launcher)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        use anyhow::Context;
        let _ = launcher;
        let closer = portable::Closer::prepare()?;
        let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })?;
        // Do every fallible handle setup before creating the child.
        let mut master = closer.own(pair.master, Some(pair.slave));
        let reader = master.handle().try_clone_reader()?;
        let writer = master.handle().take_writer()?;
        let (program, args) = argv.split_first().context("empty PTY command")?;
        let mut command = portable_pty::CommandBuilder::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        if !env.contains_key("TERM") {
            command.env("TERM", "xterm-256color");
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let child = master.spawn(command)?;
        Ok(Opened {
            master: Master::Portable(master),
            reader,
            writer,
            child: Child::Portable(child),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct BlockingMaster {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Drop for BlockingMaster {
        fn drop(&mut self) {
            self.entered.send(()).expect("entered");
            self.release.recv().expect("release slow OS close");
        }
    }
    impl portable_pty::MasterPty for BlockingMaster {
        fn resize(&self, _: portable_pty::PtySize) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<portable_pty::PtySize> {
            Ok(Default::default())
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
            Ok(Box::new(io::empty()))
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            Ok(Box::new(io::sink()))
        }
        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<i32> {
            None
        }
        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
    }

    #[test]
    fn a_slow_portable_master_close_never_blocks_the_callers_drop() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let master = Master::Portable(portable::Closer::prepare().expect("closer").own(
            Box::new(BlockingMaster {
                entered: entered_tx,
                release: release_rx,
            }),
            None,
        ));
        let caller = std::thread::spawn(move || {
            drop(master);
            done_tx.send(()).expect("done");
        });
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("OS close entered");
        let caller_returned = done_rx.recv_timeout(Duration::from_millis(50)).is_ok();
        release_tx.send(()).expect("unblock cleanup");
        caller.join().expect("caller");
        assert!(caller_returned, "UI drop waited for the OS console close");
    }
}
