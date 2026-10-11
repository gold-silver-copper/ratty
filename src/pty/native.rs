//! fuxix's typed nonblocking master and consuming slave launcher.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fuxix::poll::{Events, PollFd};

use super::{Child, Dimensions, Master, Opened};

pub(super) fn spawn(
    size: Dimensions,
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    launcher: &Path,
) -> anyhow::Result<Opened> {
    let size_chars =
        fuxix::terminal::Size::new(size.rows, size.cols).context("invalid PTY size")?;
    let (master, slave) = fuxix::pty::open(size_chars).context("failed to open fuxix PTY")?;
    resize(&master, size)?;
    let master = Arc::new(master);
    let reader = Box::new(Io(master.clone()));
    let writer = Box::new(Io(master.clone()));
    // spawn consumes the slave and waits for the launcher's exec handshake.
    // No fallible setup remains after a child has been successfully created.
    let (child, pid) = slave
        .spawn(launcher, argv, |command| {
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            if !env.contains_key("TERM") {
                command.env("TERM", "xterm-256color");
            }
            command.envs(env);
        })
        .context("failed to launch PTY command")?;
    Ok(Opened {
        master: Master::Native(master.clone()),
        reader,
        writer,
        child: Child::Native { child, pid, master },
    })
}

pub(super) fn resize(master: &fuxix::pty::Master, size: Dimensions) -> anyhow::Result<()> {
    // fuxix 0.2.0's set_window_size writes zero pixels. This safe ioctl
    // adapter preserves all four dimensions without changing handle ownership.
    rustix::termios::tcsetwinsize(
        master,
        rustix::termios::Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        },
    )?;
    Ok(())
}

struct Io(Arc<fuxix::pty::Master>);

impl Read for Io {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match fuxix::io::read(&*self.0, bytes) {
            Err(fuxix::Errno::AGAIN) => {
                let mut fds = [PollFd::new(&*self.0, Events::IN)];
                fuxix::poll::poll(&mut fds, Some(Duration::from_millis(20)))?;
                fuxix::io::read(&*self.0, bytes).map_err(Into::into)
            }
            result => result.map_err(Into::into),
        }
    }
}

impl Write for Io {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        fuxix::io::write(&*self.0, bytes).map_err(Into::into)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_resize_preserves_character_and_pixel_dimensions() {
        let size = fuxix::terminal::Size::new(24, 80).expect("size");
        let (master, _slave) = fuxix::pty::open(size).expect("PTY");
        resize(
            &master,
            Dimensions {
                rows: 31,
                cols: 97,
                pixel_width: 1200,
                pixel_height: 700,
            },
        )
        .expect("resize");
        let actual = rustix::termios::tcgetwinsize(&master).expect("kernel dimensions");
        assert_eq!(
            (
                actual.ws_row,
                actual.ws_col,
                actual.ws_xpixel,
                actual.ws_ypixel
            ),
            (31, 97, 1200, 700)
        );
    }
}
