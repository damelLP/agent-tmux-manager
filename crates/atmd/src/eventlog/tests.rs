use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn ts() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn segment(root: &Path, stream: Stream, first: u64, contents: &[u8]) -> PathBuf {
    let dir = root.join("log").join(match stream {
        Stream::Events => "events",
        Stream::Raw => "raw",
    });
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{first:08}.jsonl"));
    fs::write(&path, contents).unwrap();
    path
}

fn record(seq: u64) -> Vec<u8> {
    format!("{{\"seq\":{seq},\"ts\":\"2023-11-14T22:13:20Z\",\"kind\":\"test\",\"body\":{{}}}}\n")
        .into_bytes()
}

fn reopen(root: &Path, stream: Stream) -> EventLog {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match EventLog::open(root, stream) {
            Ok(log) => return log,
            Err(EventLogError::Locked(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("reopen: {e}"),
        }
    }
}

#[tokio::test]
async fn append_reopen_and_read_after() {
    let dir = tempfile::tempdir().unwrap();
    // open is deliberately called inside Tokio: its std startup receive must not panic.
    let log = EventLog::open(dir.path(), Stream::Events).unwrap();
    let mut live = log.subscribe();
    for seq in 1..=3 {
        assert_eq!(
            log.append(ts(), "test", serde_json::json!({"n":seq}))
                .await
                .unwrap(),
            seq
        );
        let envelope = live.recv().await.unwrap();
        assert_eq!(envelope.seq, seq);
        assert_eq!(envelope.ts, ts());
        assert_eq!(envelope.body, serde_json::json!({"n":seq}));
    }
    assert!(live.try_recv().is_err());
    let rows: Vec<_> = read_from(dir.path(), Stream::Events, 1)
        .map(|r| r.unwrap().seq)
        .collect();
    assert_eq!(rows, [2, 3]);
    drop(log);
    let log = reopen(dir.path(), Stream::Events);
    assert_eq!(
        log.append(ts(), "next", serde_json::Value::Null)
            .await
            .unwrap(),
        4
    );
    let raw = EventLog::open(dir.path(), Stream::Raw).unwrap();
    assert_eq!(
        raw.append(ts(), "raw", serde_json::Value::Null)
            .await
            .unwrap(),
        1
    );
}

#[test]
fn exclusive_stream_lock_lasts_through_clones() {
    let dir = tempfile::tempdir().unwrap();
    let first = EventLog::open(dir.path(), Stream::Events).unwrap();
    let clone = first.clone();
    drop(first);
    assert!(matches!(
        EventLog::open(dir.path(), Stream::Events),
        Err(EventLogError::Locked(_))
    ));
    drop(clone);
    drop(reopen(dir.path(), Stream::Events));
}

#[tokio::test]
async fn repairs_only_unterminated_newest_tail() {
    for tail in [
        b"{\"seq\":2".as_slice(),
        b"\xff\xfe",
        &record(2)[..record(2).len() - 1],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = record(1);
        bytes.extend_from_slice(tail);
        let path = segment(dir.path(), Stream::Events, 1, &bytes);
        let log = EventLog::open(dir.path(), Stream::Events).unwrap();
        assert_eq!(fs::read(&path).unwrap(), record(1));
        assert_eq!(
            log.append(ts(), "test", serde_json::json!({}))
                .await
                .unwrap(),
            2
        );
    }
}

#[test]
fn newest_corruption_reports_path_and_line() {
    for bad in [b"not json\n".to_vec(), record(0), record(1)] {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = record(1);
        bytes.extend(bad);
        bytes.extend(record(3));
        let path = segment(dir.path(), Stream::Events, 1, &bytes);
        let error = EventLog::open(dir.path(), Stream::Events).err().unwrap();
        assert!(matches!(error, EventLogError::Corrupt { line: 2, .. }));
        assert!(error.to_string().contains(&path.display().to_string()));
    }
}

#[test]
fn open_does_not_scan_older_segments_but_replay_validates_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = segment(
        dir.path(),
        Stream::Events,
        1,
        &[record(1), b"bad\n".to_vec()].concat(),
    );
    segment(dir.path(), Stream::Events, 3, &record(3));
    let _log = EventLog::open(dir.path(), Stream::Events).unwrap();
    let mut replay = read_from(dir.path(), Stream::Events, 0);
    assert_eq!(replay.next().unwrap().unwrap().seq, 1);
    let error = replay.next().unwrap().unwrap_err();
    assert!(matches!(error, EventLogError::Corrupt { line: 2, .. }));
    assert!(error.to_string().contains(&path.display().to_string()));
    assert!(replay.next().is_none());
}

#[test]
fn replay_rejects_cross_segment_regression_and_old_torn_tail() {
    for old in [
        [record(1), record(3)].concat(),
        [record(1), b"partial".to_vec()].concat(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        segment(dir.path(), Stream::Events, 1, &old);
        segment(dir.path(), Stream::Events, 3, &record(3));
        let mut replay = read_from(dir.path(), Stream::Events, 0);
        assert_eq!(replay.next().unwrap().unwrap().seq, 1);
        let remaining: Vec<_> = replay.collect();
        assert!(remaining.last().unwrap().is_err());
    }
}

#[tokio::test]
async fn empty_high_water_segment_and_sequence_exhaustion() {
    let dir = tempfile::tempdir().unwrap();
    segment(dir.path(), Stream::Events, u64::MAX, b"");
    let log = EventLog::open(dir.path(), Stream::Events).unwrap();
    assert_eq!(
        log.append(ts(), "last", serde_json::Value::Null)
            .await
            .unwrap(),
        u64::MAX
    );
    assert!(matches!(
        log.append(ts(), "overflow", serde_json::Value::Null).await,
        Err(EventLogError::SequenceExhausted)
    ));
    drop(log);
    let log = reopen(dir.path(), Stream::Events);
    assert!(matches!(
        log.append(ts(), "overflow", serde_json::Value::Null).await,
        Err(EventLogError::SequenceExhausted)
    ));
}

#[test]
fn missing_replay_is_empty_but_invalid_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_from(dir.path(), Stream::Events, 0).next().is_none());
    fs::write(dir.path().join("log"), "not a directory").unwrap();
    assert!(read_from(dir.path(), Stream::Events, 0)
        .next()
        .unwrap()
        .is_err());
}
