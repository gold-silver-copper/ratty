//! Close blocking platform consoles on a prepared worker, including error paths.

use std::io;
use std::sync::mpsc;

type Handle = Box<dyn portable_pty::MasterPty + Send>;
type Slave = Box<dyn portable_pty::SlavePty + Send>;

struct Handles {
    master: Handle,
    slave: Option<Slave>,
}

pub(crate) struct Closer(mpsc::Sender<Handles>);

impl Closer {
    pub(crate) fn prepare() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Handles>();
        std::thread::Builder::new()
            .name("ratty-pty-close".into())
            .spawn(move || {
                // Dropping ConPTY calls ClosePseudoConsole, which can wait on
                // output consumption. It must never run on the UI thread.
                if let Ok(handles) = receiver.recv() {
                    drop(handles);
                }
            })?;
        Ok(Self(sender))
    }

    pub(crate) fn own(self, master: Handle, slave: Option<Slave>) -> Master {
        Master {
            handles: Some(Handles { master, slave }),
            closer: self,
        }
    }
}

pub(crate) struct Master {
    handles: Option<Handles>,
    closer: Closer,
}

impl Master {
    pub(crate) fn handle(&self) -> &(dyn portable_pty::MasterPty + Send) {
        &*self.handles.as_ref().expect("open master").master
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub(crate) fn spawn(
        &mut self,
        command: portable_pty::CommandBuilder,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        let handles = self.handles.as_mut().expect("open master");
        let child = handles
            .slave
            .as_ref()
            .expect("unconsumed slave")
            .spawn_command(command)?;
        // The master still holds the platform console, so this cannot perform
        // its final close. On any failure, both handles go to the close worker.
        handles.slave.take();
        Ok(child)
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        if let Some(handles) = self.handles.take() {
            // Exactly one transfer; the worker's first operation receives it.
            // This send is unbounded but the type can submit only one item.
            assert!(
                self.closer.0.send(handles).is_ok(),
                "prepared PTY closer must be waiting"
            );
        }
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        // Close the slave before the final console owner, on this worker.
        self.slave.take();
    }
}
