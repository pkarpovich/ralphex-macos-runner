//! The log pipeline against a scripted fake farm.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ralphex_macos_runner::logstream::{LogStream, Terminator};
use ralphex_macos_runner::protocol::client::{FarmClient, FarmError};
use ralphex_macos_runner::protocol::types::{
    HISTORY_LINES, LOG_BUFFER_BYTES, LOG_TAIL_LINES, MAX_LOG_CHUNK, RunId, Seq,
};
use support::fake_farm::{FakeFarm, Reply};
use support::{TestSleeper, TickHandle, manual_ticker};

fn run_id() -> RunId {
    RunId("local-1".to_string())
}

fn client(farm: &FakeFarm) -> Arc<FarmClient> {
    Arc::new(FarmClient::new(farm.url(), "secret-token", Arc::new(TestSleeper::new())).unwrap())
}

fn stream(farm: &FakeFarm) -> (LogStream, TickHandle) {
    let (ticker, handle) = manual_ticker();
    let stream = LogStream::new(client(farm), run_id(), ticker);
    (stream, handle)
}

fn sequences(farm: &FakeFarm) -> Vec<u64> {
    let mut seen = Vec::new();
    for request in farm.requests_ending("/log") {
        let Some(seq) = request.seq() else {
            continue;
        };
        seen.push(seq);
    }
    seen
}

fn sizes(farm: &FakeFarm) -> Vec<usize> {
    let mut seen = Vec::new();
    for request in farm.requests_ending("/log") {
        seen.push(request.body.len());
    }
    seen
}

#[tokio::test]
async fn two_hundred_kilobytes_arrive_as_four_numbered_chunks() {
    let farm = FakeFarm::start().await;
    let (stream, handle) = stream(&farm);

    let bytes = vec![b'x'; 200 * 1024];
    stream.write(&bytes);
    handle.drive().await;

    assert_eq!(sequences(&farm), vec![1, 2, 3, 4]);
    assert_eq!(
        sizes(&farm),
        vec![MAX_LOG_CHUNK, MAX_LOG_CHUNK, MAX_LOG_CHUNK, 8192]
    );
    stream.close().await;
}

#[tokio::test]
async fn a_refused_chunk_is_dropped_and_its_sequence_is_spent() {
    let farm = FakeFarm::start().await;
    farm.push_log(Reply::Accepted);
    farm.push_log(Reply::Status(400, "bad chunk".to_string()));
    farm.push_log(Reply::Accepted);
    let (stream, handle) = stream(&farm);

    let first = vec![b'a'; MAX_LOG_CHUNK];
    let second = vec![b'b'; MAX_LOG_CHUNK];
    let third = vec![b'c'; MAX_LOG_CHUNK];
    stream.write(&first);
    stream.write(&second);
    stream.write(&third);
    handle.drive().await;

    assert_eq!(sequences(&farm), vec![1, 2, 3]);
    let requests = farm.requests_ending("/log");
    assert_eq!(requests[0].body[0], b'a');
    assert_eq!(requests[1].body[0], b'b');
    assert_eq!(requests[2].body[0], b'c');
    assert_eq!(requests[2].seq(), Some(3));
    stream.close().await;
}

#[tokio::test]
async fn the_farm_refuses_a_chunk_numbered_zero() {
    let farm = FakeFarm::start().await;
    let client = client(&farm);

    let error = client
        .append_log(&run_id(), Seq(0), b"hello")
        .await
        .unwrap_err();

    assert_eq!(error, FarmError::BadRequest("bad seq 0".to_string()));
}

#[tokio::test]
async fn a_gone_stream_stops_posting_and_resolves_its_latch() {
    let farm = FakeFarm::start().await;
    farm.always_log(Reply::Status(410, String::new()));
    let (stream, handle) = stream(&farm);

    stream.write(b"first\n");
    handle.drive().await;
    stream.write(b"second\n");
    handle.drive().await;

    assert_eq!(farm.requests_ending("/log").len(), 1);
    tokio::time::timeout(Duration::from_secs(5), stream.gone())
        .await
        .unwrap();
    stream.close().await;
}

#[tokio::test]
async fn the_tail_keeps_the_last_hundred_lines() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    for index in 0..150 {
        stream.push_line(format!("line {index}").as_bytes(), Terminator::Newline);
    }

    let tail = stream.tail();
    let lines: Vec<&str> = tail.split('\n').collect();
    assert_eq!(lines.len(), LOG_TAIL_LINES);
    assert_eq!(lines[0], "line 50");
    assert_eq!(lines[LOG_TAIL_LINES - 1], "line 149");
    stream.close().await;
}

#[tokio::test]
async fn a_late_subscriber_replays_the_history_then_follows_the_live_lines() {
    let farm = FakeFarm::start().await;
    let (stream, handle) = stream(&farm);

    for index in 0..50 {
        let line = format!("line {index}");
        stream.push_line(line.as_bytes(), Terminator::Newline);
    }
    handle.drive().await;
    handle.drive().await;
    handle.drive().await;

    let (replay, mut live) = stream.subscribe();
    assert_eq!(replay.len(), 50);
    assert_eq!(replay[0], "line 0");
    assert_eq!(replay[49], "line 49");

    stream.push_line(b"line 50", Terminator::Newline);
    assert_eq!(live.recv().await.unwrap(), "line 50");
    stream.close().await;
}

#[tokio::test]
async fn two_subscribers_both_receive_a_line() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    let (_, mut first) = stream.subscribe();
    let (_, mut second) = stream.subscribe();
    stream.push_line(b"shared", Terminator::Newline);

    assert_eq!(first.recv().await.unwrap(), "shared");
    assert_eq!(second.recv().await.unwrap(), "shared");
    stream.close().await;
}

#[tokio::test]
async fn the_history_ring_drops_the_oldest_line_past_its_bound() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    for index in 0..(HISTORY_LINES + 5) {
        stream.push_line(format!("line {index}").as_bytes(), Terminator::Newline);
    }

    let (replay, _live) = stream.subscribe();
    assert_eq!(replay.len(), HISTORY_LINES);
    assert_eq!(replay[0], "line 5");
    assert_eq!(
        replay[HISTORY_LINES - 1],
        format!("line {}", HISTORY_LINES + 4)
    );
    stream.close().await;
}

#[tokio::test]
async fn closing_flushes_the_remainder() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    stream.write(b"the last words\n");
    stream.close().await;

    let requests = farm.requests_ending("/log");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].seq(), Some(1));
    assert_eq!(requests[0].text(), "the last words\n");
}

#[tokio::test]
async fn a_closed_stream_can_be_closed_again() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    stream.write(b"once\n");
    stream.close().await;
    stream.close().await;

    assert_eq!(farm.requests_ending("/log").len(), 1);
}

#[tokio::test]
async fn a_buffer_past_its_bound_drops_its_oldest_bytes() {
    let farm = FakeFarm::start().await;
    let (stream, handle) = stream(&farm);

    let mut written = vec![b'o'; MAX_LOG_CHUNK];
    written.extend(vec![b'n'; LOG_BUFFER_BYTES]);
    stream.write(&written);
    handle.drive().await;

    let mut delivered = Vec::new();
    for request in farm.requests_ending("/log") {
        delivered.extend(request.body);
    }
    assert_eq!(delivered.len(), LOG_BUFFER_BYTES);
    assert!(
        !delivered.contains(&b'o'),
        "the oldest bytes outlived the bound"
    );
    stream.close().await;
}

#[tokio::test]
async fn a_burst_larger_than_the_buffer_reaches_the_farm_whole_without_a_tick() {
    let farm = FakeFarm::start().await;
    let (stream, _handle) = stream(&farm);

    let pieces = 5 * 1024 * 1024 / MAX_LOG_CHUNK;
    let mut written = Vec::new();
    for piece in 0..pieces {
        let byte = b'a' + u8::try_from(piece % 26).unwrap();
        let bytes = vec![byte; MAX_LOG_CHUNK];
        stream.write(&bytes);
        written.extend(bytes);
        tokio::task::yield_now().await;
    }
    stream.close().await;

    let mut delivered = Vec::new();
    for request in farm.requests_ending("/log") {
        delivered.extend(request.body);
    }
    assert_eq!(
        delivered.len(),
        written.len(),
        "the buffer dropped bytes the ticker never came to flush"
    );
    assert_eq!(
        delivered, written,
        "the chunks reached the farm out of order"
    );
    assert_eq!(
        sequences(&farm),
        (1..=u64::try_from(pieces).unwrap()).collect::<Vec<u64>>()
    );
}

#[tokio::test]
async fn a_chunk_the_farm_loses_is_retried_under_the_same_sequence() {
    let farm = FakeFarm::start().await;
    farm.push_log(Reply::Status(500, "boom".to_string()));
    farm.push_log(Reply::Accepted);
    let (stream, handle) = stream(&farm);

    stream.write(b"retried\n");
    handle.drive().await;

    assert_eq!(sequences(&farm), vec![1, 1]);
    stream.close().await;
}
