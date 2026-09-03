//! The outgoing log buffer, the chunking flusher and the replay history.
//!
//! A [`LogStream`] takes the bytes a run prints, hands them to the farm in
//! chunks under a strictly increasing sequence number, and keeps two bounded
//! views of the same output: the tail a completion carries and the history an
//! attaching client replays before it follows the live lines. The flush cadence
//! arrives through the [`Ticker`] trait, so a test drives every flush itself.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::protocol::client::{FarmClient, FarmError};
use crate::protocol::types::{
    HISTORY_BYTES, HISTORY_LINES, LOG_BUFFER_BYTES, LOG_CLOSE_TIMEOUT, LOG_FLUSH_INTERVAL,
    LOG_TAIL_BYTES, LOG_TAIL_LINES, MAX_LOG_CHUNK, RunId, Seq,
};

const SUBSCRIBER_CAPACITY: usize = 1024;

/// A source of flush cadence.
///
/// The daemon uses [`IntervalTicker`]; a test supplies an implementation it
/// releases one tick at a time.
pub trait Ticker: Send + Sync {
    /// Returns a future that resolves when the next flush is due.
    fn tick(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The [`Ticker`] the daemon runs with.
#[derive(Debug, Clone, Copy)]
pub struct IntervalTicker;

impl Ticker for IntervalTicker {
    fn tick(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(LOG_FLUSH_INTERVAL))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Latch {
    Open,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Closing,
}

fn clamp(line: &str, max_bytes: usize) -> &str {
    if line.len() <= max_bytes {
        return line;
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
}

impl Ring {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Ring {
            lines: VecDeque::new(),
            bytes: 0,
            max_lines,
            max_bytes,
        }
    }

    fn push(&mut self, line: &str) {
        let line = clamp(line, self.max_bytes);
        self.lines.push_back(line.to_string());
        self.bytes += line.len();
        loop {
            if self.lines.len() <= self.max_lines && self.bytes <= self.max_bytes {
                return;
            }
            let Some(dropped) = self.lines.pop_front() else {
                return;
            };
            self.bytes -= dropped.len();
        }
    }

    fn snapshot(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.lines.len());
        for line in &self.lines {
            lines.push(line.clone());
        }
        lines
    }

    fn joined(&self) -> String {
        let mut joined = String::with_capacity(self.bytes + self.lines.len());
        for line in &self.lines {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(line);
        }
        joined
    }
}

struct Buffers {
    outgoing: VecDeque<u8>,
    history: Ring,
    tail: Ring,
}

/// The log pipeline of one run.
pub struct LogStream {
    buffers: Arc<Mutex<Buffers>>,
    lines: broadcast::Sender<String>,
    gone: watch::Sender<Latch>,
    phase: watch::Sender<Phase>,
    flusher: Mutex<Option<JoinHandle<()>>>,
}

impl LogStream {
    /// Returns a stream that flushes `run_id`'s output to `client` on `ticker`.
    ///
    /// The flusher task starts here and runs until [`LogStream::close`].
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    #[must_use]
    pub fn new(client: Arc<FarmClient>, run_id: RunId, ticker: Arc<dyn Ticker>) -> Self {
        let buffers = Arc::new(Mutex::new(Buffers {
            outgoing: VecDeque::new(),
            history: Ring::new(HISTORY_LINES, HISTORY_BYTES),
            tail: Ring::new(LOG_TAIL_LINES, LOG_TAIL_BYTES),
        }));
        let (lines, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        let (gone, _) = watch::channel(Latch::Open);
        let (phase, closing) = watch::channel(Phase::Running);
        let flusher = tokio::spawn(flush_loop(
            client,
            run_id,
            Arc::clone(&buffers),
            ticker,
            gone.clone(),
            closing,
        ));
        LogStream {
            buffers,
            lines,
            gone,
            phase,
            flusher: Mutex::new(Some(flusher)),
        }
    }

    /// Appends `bytes` to the buffer the flusher sends to the farm.
    ///
    /// Past [`LOG_BUFFER_BYTES`] the oldest bytes are dropped.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the buffer lock panicked.
    pub fn write(&self, bytes: &[u8]) {
        let mut buffers = self.buffers.lock().unwrap();
        buffers.outgoing.extend(bytes);
        let over = buffers.outgoing.len().saturating_sub(LOG_BUFFER_BYTES);
        if over > 0 {
            buffers.outgoing.drain(..over);
        }
    }

    /// Records `line` in the history and the tail and sends it to subscribers.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the buffer lock panicked.
    pub fn push_line(&self, line: String) {
        let mut buffers = self.buffers.lock().unwrap();
        buffers.history.push(&line);
        buffers.tail.push(&line);
        let _ = self.lines.send(line);
        drop(buffers);
    }

    /// Returns the replay history and a receiver of the lines that follow it.
    ///
    /// The two are taken under one lock, so no line is lost or seen twice
    /// between the replay and the live stream.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the buffer lock panicked.
    #[must_use]
    pub fn subscribe(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        let buffers = self.buffers.lock().unwrap();
        let receiver = self.lines.subscribe();
        let replay = buffers.history.snapshot();
        drop(buffers);
        (replay, receiver)
    }

    /// Returns the trailing output a completion carries.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the buffer lock panicked.
    #[must_use]
    pub fn tail(&self) -> String {
        let buffers = self.buffers.lock().unwrap();
        let tail = buffers.tail.joined();
        drop(buffers);
        tail
    }

    /// Resolves once the farm answered a chunk with `410`.
    pub async fn gone(&self) {
        let mut latch = self.gone.subscribe();
        let _ = latch
            .wait_for(|latch| match latch {
                Latch::Gone => true,
                Latch::Open => false,
            })
            .await;
    }

    /// Flushes what is buffered and stops the flusher task.
    ///
    /// [`LOG_CLOSE_TIMEOUT`] is the total budget, whether the flusher was idle
    /// or already sending when the close was asked for; a flusher that outlasts
    /// it is dropped with its buffer.
    ///
    /// # Panics
    ///
    /// Panics when another holder of the flusher lock panicked.
    pub async fn close(&self) {
        self.phase.send_replace(Phase::Closing);
        let flusher = {
            let mut flusher = self.flusher.lock().unwrap();
            flusher.take()
        };
        let Some(mut flusher) = flusher else {
            return;
        };
        let stopped = tokio::time::timeout(LOG_CLOSE_TIMEOUT, &mut flusher).await;
        match stopped {
            Ok(_joined) => {}
            Err(_elapsed) => flusher.abort(),
        }
    }
}

async fn flush_loop(
    client: Arc<FarmClient>,
    run_id: RunId,
    buffers: Arc<Mutex<Buffers>>,
    ticker: Arc<dyn Ticker>,
    gone: watch::Sender<Latch>,
    mut closing: watch::Receiver<Phase>,
) {
    let mut seq = Seq::FIRST;
    loop {
        let phase = tokio::select! {
            () = ticker.tick() => Phase::Running,
            _ = closing.changed() => Phase::Closing,
        };
        match phase {
            Phase::Running => {
                seq = flush(&client, &run_id, &buffers, &gone, seq).await;
            }
            Phase::Closing => {
                let _remainder = flush(&client, &run_id, &buffers, &gone, seq).await;
                return;
            }
        }
    }
}

async fn flush(
    client: &FarmClient,
    run_id: &RunId,
    buffers: &Mutex<Buffers>,
    gone: &watch::Sender<Latch>,
    seq: Seq,
) -> Seq {
    let mut seq = seq;
    loop {
        match *gone.borrow() {
            Latch::Gone => return seq,
            Latch::Open => {}
        }
        let chunk = take_chunk(buffers);
        if chunk.is_empty() {
            return seq;
        }
        let sent = client.append_log(run_id, seq, &chunk).await;
        seq = seq.increment();
        match sent {
            Ok(()) => {}
            Err(FarmError::Gone) => {
                gone.send_replace(Latch::Gone);
                return seq;
            }
            Err(FarmError::VersionMismatch { message: _ }) => {}
            Err(FarmError::BadRequest(_)) => {}
            Err(FarmError::Rejected(_, _)) => {}
            Err(FarmError::Transport(_)) => {}
            Err(FarmError::Decode(_)) => {}
        }
    }
}

fn take_chunk(buffers: &Mutex<Buffers>) -> Vec<u8> {
    let mut buffers = buffers.lock().unwrap();
    let wanted = std::cmp::min(buffers.outgoing.len(), MAX_LOG_CHUNK);
    let mut chunk = Vec::with_capacity(wanted);
    for byte in buffers.outgoing.drain(..wanted) {
        chunk.push(byte);
    }
    drop(buffers);
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_drops_the_oldest_line_past_its_line_bound() {
        let mut ring = Ring::new(3, 1024);
        for index in 0..5 {
            ring.push(&format!("line {index}"));
        }
        assert_eq!(
            ring.snapshot(),
            vec![
                "line 2".to_string(),
                "line 3".to_string(),
                "line 4".to_string()
            ]
        );
    }

    #[test]
    fn a_ring_drops_the_oldest_line_past_its_byte_bound() {
        let mut ring = Ring::new(100, 10);
        ring.push("aaaaa");
        ring.push("bbbbb");
        ring.push("ccccc");
        assert_eq!(
            ring.snapshot(),
            vec!["bbbbb".to_string(), "ccccc".to_string()]
        );
    }

    #[test]
    fn a_line_over_the_byte_bound_is_kept_truncated_instead_of_emptying_the_ring() {
        let mut ring = Ring::new(100, 10);
        ring.push("aaaaa");
        ring.push(&"b".repeat(64));
        assert_eq!(ring.snapshot(), vec!["b".repeat(10)]);
        assert_eq!(ring.joined(), "b".repeat(10));
    }

    #[test]
    fn a_truncated_line_ends_on_a_character_boundary() {
        let mut ring = Ring::new(100, 4);
        ring.push("aé\u{fffd}");
        assert_eq!(ring.snapshot(), vec!["aé".to_string()]);
    }

    #[test]
    fn a_ring_joins_its_lines_with_newlines() {
        let mut ring = Ring::new(10, 1024);
        ring.push("one");
        ring.push("two");
        assert_eq!(ring.joined(), "one\ntwo");
    }

    #[test]
    fn an_empty_ring_joins_to_nothing() {
        let ring = Ring::new(10, 1024);
        assert_eq!(ring.joined(), "");
        assert!(ring.snapshot().is_empty());
    }
}
