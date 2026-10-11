//! Platform boundary for PTY ownership, launching, I/O and pixel geometry.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod native;

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
    Portable(Box<dyn portable_pty::MasterPty + Send>),
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
            Self::Portable(master) => master.resize(portable_pty::PtySize {
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
        let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })?;
        // Do every fallible handle setup before creating the child.
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
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
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        Ok(Opened {
            master: Master::Portable(pair.master),
            reader,
            writer,
            child: Child::Portable(child),
        })
    }
}
