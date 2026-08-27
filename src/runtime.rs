//! PTY runtime and terminal state.

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::Context;
use bevy::platform::cell::SyncCell;
use bevy::prelude::Resource;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::{Crosswords, CrosswordsSize};
use rio_vt::event::WindowId;
use rio_vt::performer::handler::Processor;

use crate::config::AppConfig;
use crate::vt::{TerminalEventSink, VtTerminal};

type PtyDimensions = (u16, u16, u16, u16);
type ParserDimensions = (u16, u16);

/// Command-line runtime overrides.
#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    /// Command and arguments to execute instead of the configured shell.
    pub command: Option<Vec<String>>,
    /// Working directory used for the spawned PTY command.
    pub working_dir: Option<PathBuf>,
}

/// Running PTY and parser state.
///
/// The `!Sync` PTY handles (the output channel receiver and the master) live
/// in [`SyncCell`]s so the runtime qualifies as a regular [`Resource`] and
/// systems using it are not pinned to the main thread.
#[derive(Resource)]
pub struct TerminalRuntime {
    /// PTY output channel.
    rx: SyncCell<Receiver<Vec<u8>>>,
    /// PTY input writer.
    pub writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    /// PTY master handle.
    master: SyncCell<Option<Box<dyn MasterPty + Send>>>,
    /// Child process handle.
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// PTY reader thread.
    reader_thread: Option<JoinHandle<()>>,
    /// Terminal grid and VT state.
    pub term: VtTerminal,
    /// VT state machine feeding [`Self::term`].
    processor: Processor,
    /// Reply queue shared with the terminal's event listener.
    sink: TerminalEventSink,
    /// Indicates PTY shutdown.
    pub pty_disconnected: bool,
    shutdown_started: bool,
    /// Last dimensions successfully applied to the PTY.
    last_pty_size: PtyDimensions,
    /// Last column and row dimensions applied to the VT parser.
    last_parser_size: ParserDimensions,
    /// Desired dimensions retained until both the PTY and parser accept them.
    pending_resize: Option<PtyDimensions>,
}

/// Returns the default shell for the current platform.
///
/// On Windows this prefers Git for Windows' `bash.exe` when it can be found
/// (most users running terminal apps on Windows want a POSIX shell so the
/// Ratatui demos behave the same as on Linux/macOS), then `%COMSPEC%` (the
/// resolved command processor), and finally `cmd.exe`. On other platforms
/// it falls back to `/bin/sh`.
fn default_shell() -> String {
    #[cfg(windows)]
    {
        if let Some(bash) = find_git_bash() {
            return bash;
        }
        env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        "/bin/sh".to_string()
    }
}

/// Looks for a Git for Windows `bash.exe` in the well-known install
/// locations, then on `PATH`. Returns the first match.
///
/// `usr/bin/bash.exe` is the MSYS shell bundled with Git for Windows;
/// `bin/bash.exe` is the shim used by the Git Bash launcher. Either works
/// as a PTY shell.
#[cfg(windows)]
fn find_git_bash() -> Option<String> {
    use std::path::PathBuf;

    // Flat candidate table keeps every probe path on one footing: each entry
    // is `(env_var, subpath_under_that_directory)`. New install layouts (Git
    // via Scoop, Chocolatey, custom installers) only need another row here.
    const CANDIDATES: &[(&str, &str)] = &[
        ("ProgramW6432", "Git/bin/bash.exe"),
        ("ProgramW6432", "Git/usr/bin/bash.exe"),
        ("ProgramFiles", "Git/bin/bash.exe"),
        ("ProgramFiles", "Git/usr/bin/bash.exe"),
        ("ProgramFiles(x86)", "Git/bin/bash.exe"),
        ("ProgramFiles(x86)", "Git/usr/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/usr/bin/bash.exe"),
    ];

    for (env_var, sub) in CANDIDATES {
        let Ok(base) = env::var(env_var) else {
            continue;
        };
        let candidate = PathBuf::from(base).join(sub);
        if candidate.is_file() {
            return candidate.into_os_string().into_string().ok();
        }
    }

    // Final fallback: walk PATH so custom installs (Scoop shims, etc.) work.
    if let Ok(path) = env::var("PATH") {
        for entry in env::split_paths(&path) {
            let candidate = entry.join("bash.exe");
            if candidate.is_file() {
                return candidate.into_os_string().into_string().ok();
            }
        }
    }

    None
}

impl TerminalRuntime {
    /// Spawns the shell PTY runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be created or the shell cannot be spawned.
    pub fn spawn(config: &AppConfig, options: &RuntimeOptions) -> anyhow::Result<Self> {
        let cols = config.terminal.default_cols;
        let rows = config.terminal.default_rows;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to create PTY pair")?;

        let mut cmd = if let Some(command) = &options.command {
            let mut command = command.iter();
            let program = command
                .next()
                .context("command override must contain at least one argument")?;
            let mut cmd = CommandBuilder::new(program);
            cmd.args(command);
            cmd
        } else {
            let shell = config
                .shell
                .program
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .or_else(|| env::var("SHELL").ok())
                .unwrap_or_else(default_shell);
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(&config.shell.args);
            cmd
        };

        if let Some(working_dir) = &options.working_dir {
            cmd.cwd(working_dir);
        }
        if !config.env.contains_key("TERM") {
            cmd.env("TERM", "xterm-256color");
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to create PTY writer")?;

        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
        let reader_thread = thread::spawn(move || {
            let mut buf = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(size) => {
                        if tx.send(buf[..size].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let sink = TerminalEventSink::default();
        let term = Crosswords::new(
            CrosswordsSize::new(usize::from(cols.max(1)), usize::from(rows.max(1))),
            CursorShape::Block,
            sink.clone(),
            // Route and window ids are Rio's multiplexer bookkeeping; ratty
            // drives a single terminal, so both are zero.
            WindowId::from(0),
            0,
            config.terminal.scrollback,
        );

        Ok(Self {
            rx: SyncCell::new(rx),
            writer: Arc::new(Mutex::new(Some(writer))),
            master: SyncCell::new(Some(pair.master)),
            child: Some(child),
            reader_thread: Some(reader_thread),
            term,
            processor: Processor::default(),
            sink,
            pty_disconnected: false,
            shutdown_started: false,
            last_pty_size: (cols, rows, 0, 0),
            last_parser_size: (cols, rows),
            pending_resize: None,
        })
    }

    /// Feeds bytes from the PTY into the VT state machine.
    pub fn process(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Drains the replies rio-vt has queued for write-back to the PTY.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        self.sink.take_replies()
    }

    /// Receives pending PTY output without blocking.
    pub fn try_recv(&mut self) -> Result<Vec<u8>, TryRecvError> {
        self.rx.get().try_recv()
    }

    /// Writes input bytes to the PTY.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        if let Ok(mut writer) = self.writer.lock()
            && let Some(writer) = writer.as_mut()
        {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Resizes the PTY and parser screen.
    ///
    /// The PTY cache is committed only after the operating-system resize
    /// succeeds. This keeps an identical request retryable after a transient
    /// failure and prevents the parser from adopting geometry the child did
    /// not receive.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system fails to resize the PTY.
    pub fn resize(&mut self, cols: u16, rows: u16, pw: u16, ph: u16) -> anyhow::Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }

        self.pending_resize = Some((cols, rows, pw, ph));
        self.retry_pending_resize()
    }

    /// Retries the most recently requested resize, if one remains pending.
    pub(crate) fn retry_pending_resize(&mut self) -> anyhow::Result<()> {
        let Some(pty_size @ (cols, rows, pw, ph)) = self.pending_resize else {
            return Ok(());
        };

        if self.last_pty_size != pty_size {
            if let Some(master) = self.master.get().as_ref() {
                master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: pw,
                        pixel_height: ph,
                    })
                    .context("failed to resize PTY")?;
            }
            self.last_pty_size = pty_size;
        }

        let parser_size = (cols, rows);
        if self.last_parser_size != parser_size {
            // rio-vt reflows content and resets the scrolling region natively,
            // so the grid resize is the whole operation — no snapshot and
            // replay.
            self.term
                .resize(CrosswordsSize::new(usize::from(cols), usize::from(rows)));
            self.last_parser_size = parser_size;
        }

        self.pending_resize = None;
        Ok(())
    }

    /// Returns the active kitty keyboard enhancement flags.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        crate::vt::kitty_keyboard_flags(&self.term)
    }

    /// Returns the active xterm `modifyOtherKeys` level.
    pub fn modify_other_keys(&self) -> Option<u8> {
        self.term.modify_other_keys()
    }

    /// Shuts down the PTY runtime without blocking the Bevy main thread indefinitely.
    pub fn shutdown(&mut self) {
        if self.shutdown_started {
            return;
        }
        self.shutdown_started = true;
        self.pty_disconnected = true;

        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }

        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        self.child.take();
        self.master.get().take();

        if self
            .reader_thread
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
            && let Some(reader_thread) = self.reader_thread.take()
        {
            let _ = reader_thread.join();
        }
    }
}

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FailOnceMaster {
        attempts: Arc<AtomicUsize>,
        applied_size: Arc<Mutex<Option<PtyDimensions>>>,
    }

    impl MasterPty for FailOnceMaster {
        fn resize(&self, size: PtySize) -> anyhow::Result<()> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("injected resize failure");
            }
            *self.applied_size.lock().expect("resize state lock") =
                Some((size.cols, size.rows, size.pixel_width, size.pixel_height));
            Ok(())
        }

        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
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

    fn test_runtime(master: Box<dyn MasterPty + Send>) -> TerminalRuntime {
        let (_tx, rx) = mpsc::channel();
        let sink = TerminalEventSink::default();
        let term = Crosswords::new(
            CrosswordsSize::new(80, 24),
            CursorShape::Block,
            sink.clone(),
            WindowId::from(0),
            0,
            100,
        );

        TerminalRuntime {
            rx: SyncCell::new(rx),
            writer: Arc::new(Mutex::new(None)),
            master: SyncCell::new(Some(master)),
            child: None,
            reader_thread: None,
            term,
            processor: Processor::default(),
            sink,
            pty_disconnected: false,
            shutdown_started: false,
            last_pty_size: (80, 24, 0, 0),
            last_parser_size: (80, 24),
            pending_resize: None,
        }
    }

    #[test]
    fn scheduled_retry_applies_a_retained_resize_without_advancing_the_parser_early() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let applied_size = Arc::new(Mutex::new(None));
        let master = FailOnceMaster {
            attempts: attempts.clone(),
            applied_size: applied_size.clone(),
        };
        let mut runtime = test_runtime(Box::new(master));

        let first = runtime.resize(100, 30, 800, 600);
        assert!(first.is_err());
        assert_eq!(runtime.last_pty_size, (80, 24, 0, 0));
        assert_eq!(runtime.last_parser_size, (80, 24));
        assert_eq!(runtime.term.columns(), 80);
        assert_eq!(runtime.term.screen_lines(), 24);
        assert_eq!(runtime.pending_resize, Some((100, 30, 800, 600)));

        let mut app = bevy::prelude::App::new();
        app.insert_resource(runtime).add_systems(
            bevy::prelude::Update,
            crate::systems::retry_pending_terminal_resize,
        );
        app.update();

        let mut runtime = app.world_mut().resource_mut::<TerminalRuntime>();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.last_pty_size, (100, 30, 800, 600));
        assert_eq!(runtime.last_parser_size, (100, 30));
        assert_eq!(runtime.pending_resize, None);
        assert_eq!(runtime.term.columns(), 100);
        assert_eq!(runtime.term.screen_lines(), 30);
        assert_eq!(
            *applied_size.lock().expect("resize state lock"),
            Some((100, 30, 800, 600))
        );

        runtime
            .resize(100, 30, 900, 600)
            .expect("pixel-only resize should reach the PTY");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(runtime.last_pty_size, (100, 30, 900, 600));
        assert_eq!(runtime.last_parser_size, (100, 30));
    }
}
