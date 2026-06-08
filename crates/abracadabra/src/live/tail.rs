//! Background thread that tails a log file and publishes parsed events.
//!
//! Used by the Live tab to drive its animation surface without
//! touching the snapshot `State` that the historical-analysis tabs
//! read from. The tail produces into a small bounded `LiveBuffer`
//! that the renderer snapshot-reads on each frame; the existing
//! tabs are entirely unaffected by anything in this module.
//!
//! Lifecycle is owned by [`TailHandle`]. Construct via [`spawn`],
//! drop to stop. The `Drop` impl signals shutdown and joins so the
//! thread is guaranteed to exit before `TailHandle` is gone.
//!
//! Lock discipline: the buffer mutex is held only across `push` /
//! `snapshot_recent`, never across file I/O. The tail thread reads
//! into a heap buffer, parses, then briefly locks to publish.
//!
//! Rotation is not yet handled. The thread keeps a single `File`
//! handle for the path's open fd; if the validator rotates the log
//! the tail will quietly read EOF forever. LIVE-3.1 will reopen by
//! path when the file's inode changes.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::parser::{self, Event, Parsed};
use crate::source::LogSource;

/// Maximum events kept in [`LiveBuffer::recent`]. Older events drop
/// off the front as new ones arrive, so memory is bounded regardless
/// of how long the tail runs.
pub const RECENT_CAPACITY: usize = 1024;

/// Sleep between size-poll cycles inside the tail loop. Short enough
/// that newly-appended lines surface within a frame at 5-10 Hz render
/// rates; long enough that idle tails do not spin.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Read chunk size from the file. Sized to amortise syscalls without
/// blocking long on individual reads.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Snapshot data the Live tab reads to render. Producer is the tail
/// thread; consumer is the TUI render loop.
#[derive(Debug, Default)]
pub struct LiveBuffer {
    /// Most recently parsed events, oldest at the front. Length capped
    /// at [`RECENT_CAPACITY`]; pushes pop the front when full.
    pub recent: VecDeque<Event>,
    /// Total events parsed since the tail started. Strictly monotonic
    /// for the lifetime of one tail thread.
    pub total_events: u64,
    /// Total raw lines read (including ignored, continuation, and
    /// parse-error lines). Useful for "is the tail keeping up?".
    pub total_lines: u64,
    /// Last error from the tail thread (open / read / seek). Cleared
    /// on the next successful read. Rendered as a status line on the
    /// Live tab; non-fatal — the thread keeps trying.
    pub last_error: Option<String>,
    /// Wall-clock instant of the most recent successful append from
    /// the file. `None` until the first non-empty read.
    pub last_read_at: Option<Instant>,
}

impl LiveBuffer {
    fn push_event(&mut self, ev: Event) {
        if self.recent.len() == RECENT_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back(ev);
        self.total_events = self.total_events.saturating_add(1);
    }
}

/// Owner of a running tail thread. Cloneable buffer handle for
/// readers; `Drop` stops the thread.
pub struct TailHandle {
    /// Shared buffer the tail thread publishes into. Clone freely
    /// for read access; the Live tab takes a snapshot per frame.
    pub buffer: Arc<Mutex<LiveBuffer>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Held only for the journal variant so we can kill it on drop.
    child: Option<Arc<Mutex<Child>>>,
}

impl TailHandle {
    /// Signal shutdown and join the thread. Idempotent — calling
    /// repeatedly is safe (subsequent calls are a no-op).
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Kill the journalctl subprocess so the tail thread unblocks
        // from its blocking read and exits promptly.
        if let Some(child) = &self.child {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(child) = self.child.take() {
            if let Ok(mut c) = child.lock() {
                let _ = c.wait();
            }
        }
    }
}

impl Drop for TailHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn the tail thread for `source`.
///
/// For a file source: opens the file, seeks to end (so we follow
/// appends, not history), and reads new bytes as they arrive.
/// For a journal source: spawns `journalctl -u <unit> -f -o cat -n 0`
/// and streams its stdout. `-n 0` means start from now — history was
/// already processed by the initial `runner::run` pass.
/// Parsed events are pushed into the returned buffer; errors land on
/// `LiveBuffer.last_error` without panicking.
pub fn spawn(source: LogSource) -> TailHandle {
    let buffer = Arc::new(Mutex::new(LiveBuffer::default()));
    let shutdown = Arc::new(AtomicBool::new(false));

    match source {
        LogSource::File(path) => {
            let thread = {
                let buffer = Arc::clone(&buffer);
                let shutdown = Arc::clone(&shutdown);
                thread::Builder::new()
                    .name("abracadabra-tail".to_owned())
                    .spawn(move || file_tail_loop(path, buffer, shutdown))
                    .ok()
            };
            TailHandle {
                buffer,
                shutdown,
                thread,
                child: None,
            }
        }
        LogSource::Journal { unit, .. } => {
            let child_proc = Command::new("journalctl")
                .args(["-u", &unit, "-f", "-o", "cat", "-n", "0"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            match child_proc {
                Err(e) => {
                    publish_error(&buffer, format!("spawn journalctl: {e}"));
                    TailHandle {
                        buffer,
                        shutdown,
                        thread: None,
                        child: None,
                    }
                }
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let child = Arc::new(Mutex::new(child));
                    let thread = stdout.and_then(|out| {
                        let buffer = Arc::clone(&buffer);
                        let shutdown = Arc::clone(&shutdown);
                        thread::Builder::new()
                            .name("abracadabra-tail".to_owned())
                            .spawn(move || journal_tail_loop(out, buffer, shutdown))
                            .ok()
                    });
                    TailHandle {
                        buffer,
                        shutdown,
                        thread,
                        child: Some(child),
                    }
                }
            }
        }
    }
}

/// Inner tail loop for a plain file.
fn file_tail_loop(path: PathBuf, buffer: Arc<Mutex<LiveBuffer>>, shutdown: Arc<AtomicBool>) {
    let mut file = match open_at_end(&path) {
        Ok(f) => f,
        Err(e) => {
            publish_error(&buffer, format!("open {}: {e}", path.display()));
            return;
        }
    };

    // Bytes read but not yet terminated by a newline carry to the
    // next read so a line split across reads still parses.
    let mut carry = Vec::<u8>::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];

    while !shutdown.load(Ordering::Relaxed) {
        match file.read(&mut chunk) {
            Ok(0) => {
                // EOF on a tailed file just means "no new data yet";
                // poll again after a short sleep.
                thread::sleep(POLL_INTERVAL);
            }
            Ok(n) => {
                carry.extend_from_slice(&chunk[..n]);
                drain_complete_lines(&mut carry, &buffer);
                clear_error(&buffer);
            }
            Err(e) => {
                publish_error(&buffer, format!("read {}: {e}", path.display()));
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Inner tail loop for a journalctl stdout stream.
fn journal_tail_loop<R: Read>(
    mut reader: R,
    buffer: Arc<Mutex<LiveBuffer>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut carry = Vec::<u8>::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];

    while !shutdown.load(Ordering::Relaxed) {
        match reader.read(&mut chunk) {
            Ok(0) => {
                // journalctl -f should not hit EOF; if it does (process
                // killed externally), bail out.
                break;
            }
            Ok(n) => {
                carry.extend_from_slice(&chunk[..n]);
                drain_complete_lines(&mut carry, &buffer);
                clear_error(&buffer);
            }
            Err(e) => {
                publish_error(&buffer, format!("read journalctl: {e}"));
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Open `path` and seek to its current end so the tail follows
/// appends only. Returns the positioned file handle.
fn open_at_end(path: &PathBuf) -> std::io::Result<File> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    Ok(f)
}

/// Scan `carry` for newline-terminated lines, parse each, push events
/// to the buffer, and shrink `carry` to whatever partial trailing
/// fragment remains (no terminating newline yet).
fn drain_complete_lines(carry: &mut Vec<u8>, buffer: &Arc<Mutex<LiveBuffer>>) {
    let mut start = 0usize;
    let mut new_events: Vec<Event> = Vec::new();
    let mut line_count: u64 = 0;

    while let Some(rel) = carry[start..].iter().position(|b| *b == b'\n') {
        let end = start + rel;
        let line_bytes = &carry[start..end];
        line_count += 1;
        if let Ok(line) = std::str::from_utf8(line_bytes) {
            if let Ok(Parsed::Event(ev)) = parser::parse(line) {
                new_events.push(ev);
            }
        }
        // Skip the newline byte.
        start = end + 1;
    }

    if start > 0 {
        carry.drain(..start);
    }

    if line_count > 0 || !new_events.is_empty() {
        if let Ok(mut buf) = buffer.lock() {
            for ev in new_events {
                buf.push_event(ev);
            }
            buf.total_lines = buf.total_lines.saturating_add(line_count);
            buf.last_read_at = Some(Instant::now());
        }
    }
}

fn publish_error(buffer: &Arc<Mutex<LiveBuffer>>, msg: String) {
    if let Ok(mut buf) = buffer.lock() {
        buf.last_error = Some(msg);
    }
}

fn clear_error(buffer: &Arc<Mutex<LiveBuffer>>) {
    if let Ok(mut buf) = buffer.lock() {
        if buf.last_error.is_some() {
            buf.last_error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn unique_tmp(name: &str) -> PathBuf {
        let pid = std::process::id();
        std::env::temp_dir().join(format!("abr-tail-{pid}-{name}.log"))
    }

    /// `push_event` respects the capacity cap by dropping the oldest.
    #[test]
    fn live_buffer_caps_at_capacity() {
        let mut b = LiveBuffer::default();
        let dummy = || Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind: crate::parser::EventKind::Block {
                slot: 0,
                hash: "h".into(),
                parent_slot: 0,
                parent_hash: "p".into(),
            },
        };
        for _ in 0..(RECENT_CAPACITY + 50) {
            b.push_event(dummy());
        }
        assert_eq!(b.recent.len(), RECENT_CAPACITY);
        assert_eq!(b.total_events, (RECENT_CAPACITY + 50) as u64);
    }

    /// Spawn the tail, append a few lines, observe events surface.
    #[test]
    fn spawn_and_observe_appended_events() {
        let path = unique_tmp("observe");
        std::fs::write(&path, b"seed\n").unwrap();

        let handle = spawn(LogSource::File(path.clone()));

        // Brief settle so the tail seeks to end before the writer races.
        thread::sleep(Duration::from_millis(50));

        let appendline = b"[2026-05-28T16:00:06.987212241Z INFO  agave_votor::event_handler] PUBKEY: First shred 12345\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for _ in 0..3 {
            f.write_all(appendline).unwrap();
        }
        f.sync_all().unwrap();

        // Poll up to 2s for events to land.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events_seen = 0u64;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if let Ok(buf) = handle.buffer.lock() {
                events_seen = buf.total_events;
                if events_seen >= 3 {
                    break;
                }
            }
        }

        drop(handle);
        std::fs::remove_file(&path).ok();
        assert!(
            events_seen >= 3,
            "expected ≥3 events after appending 3 valid lines, got {events_seen}"
        );
    }

    /// Tailing a non-existent path publishes an error and exits cleanly.
    #[test]
    fn missing_path_publishes_error_and_exits() {
        let path = unique_tmp("missing-do-not-create");
        std::fs::remove_file(&path).ok();

        let handle = spawn(LogSource::File(path));
        thread::sleep(Duration::from_millis(100));
        let err = handle.buffer.lock().ok().and_then(|b| b.last_error.clone());
        drop(handle);
        assert!(err.is_some(), "expected open error to be published");
    }

    /// Shutdown via Drop completes promptly even if the file is idle.
    #[test]
    fn drop_stops_thread_within_a_few_polls() {
        let path = unique_tmp("idle-drop");
        std::fs::write(&path, b"seed\n").unwrap();
        let handle = spawn(LogSource::File(path.clone()));
        let start = Instant::now();
        drop(handle);
        let elapsed = start.elapsed();
        std::fs::remove_file(&path).ok();
        // Two POLL_INTERVALs of slack is generous.
        assert!(
            elapsed < POLL_INTERVAL * 5,
            "drop took {elapsed:?} (> 5 * POLL_INTERVAL)"
        );
    }
}
