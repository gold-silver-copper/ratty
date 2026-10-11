//! The child has one owner, which reaps it even when the UI stops polling.

use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::pty::Child;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Running,
    HungUp,
    Stopping,
    Exited(u32),
}

pub(crate) struct Process {
    state: Arc<Mutex<Phase>>,
    sender: Option<mpsc::Sender<Child>>,
    worker: Option<JoinHandle<()>>,
    pid: Option<u32>,
}

impl Process {
    // Start the reaper before creating a child. A thread-creation failure can
    // then never leave an unreaped process, including on runtime setup errors.
    pub(crate) fn prepare() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Child>();
        let state = Arc::new(Mutex::new(Phase::Running));
        let shared = state.clone();
        let worker = thread::Builder::new()
            .name("ratty-child".into())
            .spawn(move || {
                let Ok(mut child) = receiver.recv() else {
                    return;
                };
                let mut last_error = None;
                loop {
                    // try_wait also reaps. Poll before signalling so a child
                    // that already exited retains its real exit status.
                    match child.try_wait() {
                        Ok(Some(code)) => {
                            *shared.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Exited(code);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let message = error.to_string();
                            if last_error.as_ref() != Some(&message) {
                                bevy::log::warn!("failed to reap PTY child: {message}");
                                last_error = Some(message);
                            }
                        }
                    }
                    let stopping =
                        *shared.lock().unwrap_or_else(|e| e.into_inner()) == Phase::Stopping;
                    if stopping && let Err(error) = child.kill() {
                        let message = error.to_string();
                        if last_error.as_ref() != Some(&message) {
                            bevy::log::warn!("failed to stop PTY child: {message}");
                            last_error = Some(message);
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            })?;
        Ok(Self {
            state,
            sender: Some(sender),
            worker: Some(worker),
            pid: None,
        })
    }

    pub(crate) fn adopt(&mut self, child: Child) {
        self.pid = child.id();
        // The worker's first operation waits on this channel; it cannot exit
        // while this sole sender exists and no child has been submitted.
        self.sender
            .take()
            .expect("one child per supervisor")
            .send(child)
            .expect("prepared supervisor is waiting for its child");
    }

    pub(crate) fn phase(&self) -> Phase {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) fn hung_up(&mut self) {
        let mut phase = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Running {
            *phase = Phase::HungUp;
        }
    }

    pub(crate) fn stop(&mut self) {
        let mut phase = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*phase, Phase::Exited(_)) {
            *phase = Phase::Stopping;
        }
        drop(phase);
        self.sender.take();
        // Give the supervisor time to kill/reap before the executable exits.
        // Never join a potentially uninterruptible OS child on the UI thread.
        let deadline = Instant::now() + Duration::from_millis(100);
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}
