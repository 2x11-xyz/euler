use super::{decode_message, IncomingMessage, ProtocolError};
use std::io::{BufReader, Read, Write};
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub(super) struct IoThread {
    done: Receiver<()>,
    handle: JoinHandle<()>,
}

pub(super) fn finish_io_thread(thread: &mut Option<IoThread>, deadline: Instant) {
    let Some(thread) = thread.take() else {
        return;
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    match thread.done.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.handle.join();
        }
        Err(RecvTimeoutError::Timeout) => {
            // The process group was already terminated. On platforms without
            // a process-group primitive, a child that deliberately keeps an
            // inherited pipe open cannot hold the caller past this deadline.
            // Dropping a JoinHandle detaches only that blocked I/O helper.
        }
    }
}

pub(super) fn spawn_stdin_writer(
    mut stdin: ChildStdin,
    receiver: Receiver<Vec<u8>>,
    failed: Arc<AtomicBool>,
) -> IoThread {
    spawn_io_thread(move || {
        for bytes in receiver {
            if stdin
                .write_all(&bytes)
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush())
                .is_err()
            {
                failed.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}

pub(super) fn spawn_stdout_reader(
    stdout: ChildStdout,
    sender: SyncSender<Result<IncomingMessage, ProtocolError>>,
    limit_reached: Arc<AtomicBool>,
    max_message_bytes: usize,
    max_protocol_messages: usize,
    max_protocol_bytes: usize,
) -> IoThread {
    spawn_io_thread(move || {
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::with_capacity(1024);
        let mut byte = [0_u8; 1];
        let mut messages_seen = 0_usize;
        let mut bytes_seen = 0_usize;
        loop {
            match stdout.read(&mut byte) {
                Ok(0) => return,
                Ok(_) if byte[0] == b'\n' => {
                    messages_seen = messages_seen.saturating_add(1);
                    bytes_seen = bytes_seen.saturating_add(line.len().saturating_add(1));
                    if messages_seen > max_protocol_messages || bytes_seen > max_protocol_bytes {
                        limit_reached.store(true, Ordering::Relaxed);
                        return;
                    }
                    let message = decode_message(&line);
                    if sender.try_send(message).is_err() {
                        limit_reached.store(true, Ordering::Relaxed);
                        return;
                    }
                    line.clear();
                }
                Ok(_) => {
                    if line.len() == max_message_bytes {
                        limit_reached.store(true, Ordering::Relaxed);
                        return;
                    }
                    line.push(byte[0]);
                }
                Err(_) => return,
            }
        }
    })
}

pub(super) fn spawn_stderr_drain(
    stderr: ChildStderr,
    limit_reached: Arc<AtomicBool>,
    max_stderr_bytes: usize,
) -> IoThread {
    spawn_io_thread(move || {
        let mut stderr = stderr;
        let mut bytes_seen = 0_usize;
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    bytes_seen = bytes_seen.saturating_add(read);
                    if bytes_seen > max_stderr_bytes {
                        // Raw stderr is never retained. Crossing the host's
                        // byte ceiling still ends the invocation: otherwise a
                        // malicious or buggy peer could stream forever while
                        // the host merely kept draining its pipe.
                        limit_reached.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    })
}

fn spawn_io_thread(task: impl FnOnce() + Send + 'static) -> IoThread {
    let (done_sender, done) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        task();
        let _ = done_sender.send(());
    });
    IoThread { done, handle }
}
