//! Bounded, ordered PTY input. Only the worker performs blocking I/O.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// Four maximum control requests fit, including a packet blocked in write().
// The packet count separately bounds overhead from tiny key/mouse reports.
const MAX_PENDING_BYTES: usize = 1024 * 1024;
const MAX_PACKETS: usize = 256;

#[derive(Default)]
struct State {
    pending: AtomicUsize,
    written: AtomicU64,
    stopped: AtomicBool,
    error: Mutex<Option<String>>,
}

struct Packet {
    bytes: Vec<u8>,
    state: Arc<State>,
}

impl Drop for Packet {
    fn drop(&mut self) {
        self.state
            .pending
            .fetch_sub(self.bytes.len(), Ordering::AcqRel);
    }
}

pub(crate) struct InputQueue {
    tx: SyncSender<Packet>,
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
}

impl InputQueue {
    pub(crate) fn spawn(mut writer: Box<dyn Write + Send>) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Packet>(MAX_PACKETS);
        let state = Arc::new(State::default());
        let worker_state = state.clone();
        let worker = thread::Builder::new()
            .name("ratty-pty-input".into())
            .spawn(move || {
                let state = worker_state;
                while !state.stopped.load(Ordering::Acquire) {
                    let packet = match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(packet) => packet,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Err(error) = write_packet(&mut *writer, &packet.bytes, &state) {
                        state.stopped.store(true, Ordering::Release);
                        *state.error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(error.to_string());
                        break;
                    }
                }
                // Dropping the receiver also releases the budget of queued packets.
                state.stopped.store(true, Ordering::Release);
            })?;
        Ok(Self {
            tx,
            state,
            worker: Some(worker),
        })
    }

    /// Acceptance is atomic for a packet. No PTY write, flush, or blocking
    /// channel send occurs on the caller's thread.
    pub(crate) fn enqueue(&self, bytes: &[u8]) -> io::Result<usize> {
        if self.state.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                self.error().unwrap_or_else(|| "PTY input is closed".into()),
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if bytes.len() > MAX_PENDING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY input packet exceeds 1 MiB",
            ));
        }
        let mut pending = self.state.pending.load(Ordering::Acquire);
        loop {
            let total = pending
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_PENDING_BYTES)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full")
                })?;
            match self.state.pending.compare_exchange_weak(
                pending,
                total,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => pending = actual,
            }
        }
        let packet = Packet {
            bytes: bytes.to_vec(),
            state: self.state.clone(),
        };
        match self.tx.try_send(packet) {
            Ok(()) => Ok(bytes.len()),
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "PTY input queue is full",
            )),
            Err(TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY input worker disconnected",
            )),
        }
    }

    pub(crate) fn pending_bytes(&self) -> usize {
        self.state.pending.load(Ordering::Acquire)
    }
    pub(crate) fn written_bytes(&self) -> u64 {
        self.state.written.load(Ordering::Acquire)
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.state
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn shutdown(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        // The OS writer may currently be blocked. Its owning runtime kills and
        // reaps the child; never wait for that I/O on Bevy's update thread.
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for InputQueue {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn write_packet(writer: &mut dyn Write, bytes: &[u8], state: &State) -> io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        if state.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        match writer.write(remaining) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "PTY writer made no progress",
                ));
            }
            Ok(written) => {
                state.written.fetch_add(written as u64, Ordering::AcqRel);
                remaining = &remaining[written..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(error),
        }
    }
    loop {
        if state.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        match writer.flush() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct PartialWriter {
        out: Vec<u8>,
        interrupt: bool,
        fail_after: usize,
    }
    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.out.len() >= self.fail_after {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let count = bytes.len().min(2).min(self.fail_after - self.out.len());
            self.out.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_and_interrupted_writes_preserve_order_and_count_actual_bytes() {
        let state = State::default();
        let mut writer = PartialWriter {
            out: Vec::new(),
            interrupt: true,
            fail_after: usize::MAX,
        };
        write_packet(&mut writer, b"keyboard", &state).expect("first packet");
        write_packet(&mut writer, b"reply", &state).expect("second packet");
        assert_eq!(writer.out, b"keyboardreply");
        assert_eq!(state.written.load(Ordering::Acquire), 13);
        writer.fail_after = 16;
        assert!(write_packet(&mut writer, b"agent", &state).is_err());
        assert_eq!(writer.out, b"keyboardreplyage");
        assert_eq!(state.written.load(Ordering::Acquire), 16);
    }

    struct BlockedWriter {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        done: mpsc::Sender<Vec<u8>>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.started.send(()).expect("test receiver");
            self.release.recv().expect("test release");
            self.done.send(bytes.to_vec()).expect("test receiver");
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn backpressured_writer_does_not_block_enqueue_or_shutdown() {
        let (started, start_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (done, done_rx) = mpsc::channel();
        let mut queue = InputQueue::spawn(Box::new(BlockedWriter {
            started,
            release: release_rx,
            done,
        }))
        .expect("worker");
        let bytes = vec![42; MAX_PENDING_BYTES];
        assert_eq!(queue.enqueue(&bytes).expect("accepted"), MAX_PENDING_BYTES);
        start_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer is blocked now");
        let start = Instant::now();
        assert_eq!(
            queue.enqueue(b"x").expect_err("bounded budget").kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(queue.pending_bytes(), MAX_PENDING_BYTES);
        assert_eq!(queue.written_bytes(), 0);
        queue.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            queue.enqueue(b"x").expect_err("closed").kind(),
            io::ErrorKind::BrokenPipe
        );
        release.send(()).expect("unblock test writer");
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("written"),
            bytes
        );
    }

    #[test]
    fn worker_failure_is_observable_and_rejects_subsequent_input() {
        let queue = InputQueue::spawn(Box::new(PartialWriter {
            out: Vec::new(),
            interrupt: false,
            fail_after: 2,
        }))
        .expect("worker");
        queue.enqueue(b"abcdef").expect("accepted before failure");
        let deadline = Instant::now() + Duration::from_secs(2);
        while queue.error().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(queue.error().is_some());
        assert_eq!(queue.written_bytes(), 2);
        assert!(queue.enqueue(b"later").is_err());
    }

    #[test]
    fn zero_writes_and_flush_failures_are_errors() {
        struct Writer(bool);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(if self.0 { bytes.len() } else { 0 })
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let state = State::default();
        assert_eq!(
            write_packet(&mut Writer(false), b"abc", &state)
                .expect_err("zero write")
                .kind(),
            io::ErrorKind::WriteZero
        );
        assert_eq!(state.written.load(Ordering::Acquire), 0);
        assert_eq!(
            write_packet(&mut Writer(true), b"abc", &state)
                .expect_err("flush failure")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(state.written.load(Ordering::Acquire), 3);
    }
}
