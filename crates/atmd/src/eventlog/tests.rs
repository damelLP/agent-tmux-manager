use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Controlled receiver isolates the handle's admission/cancellation contract;
// persistence and publication use real files in the other tests.
fn controlled_log() -> (EventLog, mpsc::Receiver<Append>) {
    let (requests, receiver) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1024);
    (
        EventLog {
            requests,
            slot: Arc::new(Semaphore::new(1)),
            events,
        },
        receiver,
    )
}

#[tokio::test]
async fn expired_deadline_does_not_hand_over() {
    let (log, mut receiver) = controlled_log();
    let result = log
        .append_with_timeout(ts(), "late", serde_json::Value::Null, Duration::ZERO)
        .await;
    assert!(matches!(result, Err(EventLogError::TimeoutBeforeHandover)));
    assert!(receiver.try_recv().is_err(), "expired append was submitted");
}

#[tokio::test]
async fn timeout_after_handover_keeps_the_inflight_slot() {
    let (log, mut receiver) = controlled_log();
    let short = Duration::from_millis(20);
    assert!(matches!(
        log.append_with_timeout(ts(), "first", serde_json::Value::Null, short)
            .await,
        Err(EventLogError::TimeoutAfterHandover)
    ));
    let first = receiver.recv().await.unwrap();
    assert!(matches!(
        log.append_with_timeout(ts(), "second", serde_json::Value::Null, short)
            .await,
        Err(EventLogError::TimeoutBeforeHandover)
    ));
    assert!(receiver.try_recv().is_err());
    drop(first);
    let (result, _) = tokio::join!(
        log.append_with_timeout(
            ts(),
            "third",
            serde_json::Value::Null,
            Duration::from_secs(1)
        ),
        async {
            receiver.recv().await.unwrap().reply.send(Ok(1)).unwrap();
        },
    );
    assert_eq!(result.unwrap(), 1);
}

#[tokio::test]
async fn cancellation_and_last_handle_drop_do_not_release_an_active_write() {
    let (log, mut receiver) = controlled_log();
    let clone = log.clone();
    let caller =
        tokio::spawn(async move { clone.append(ts(), "first", serde_json::Value::Null).await });
    let active = receiver.recv().await.unwrap();
    caller.abort();
    let _ = caller.await;
    assert!(matches!(
        log.append_with_timeout(
            ts(),
            "second",
            serde_json::Value::Null,
            Duration::from_millis(20)
        )
        .await,
        Err(EventLogError::TimeoutBeforeHandover)
    ));
    drop(log);
    assert!(
        receiver.recv().await.is_none(),
        "last handle retains sender"
    );
    assert!(
        active.reply.send(Ok(1)).is_err(),
        "cancelled caller still listening"
    );
}

#[tokio::test]
async fn gone_writer_is_closed_not_a_timeout() {
    let (log, receiver) = controlled_log();
    drop(receiver);
    assert!(matches!(
        log.append(ts(), "gone", serde_json::Value::Null).await,
        Err(EventLogError::Closed)
    ));
}

#[tokio::test]
async fn concurrent_clones_publish_in_disk_order_and_lag_can_replay() {
    let dir = tempfile::tempdir().unwrap();
    let log = EventLog::open(dir.path(), Stream::Raw).unwrap();
    let mut live = log.subscribe();
    let mut tasks = Vec::new();
    for n in 0..20 {
        let log = log.clone();
        tasks.push(tokio::spawn(async move {
            log.append(ts(), "concurrent", serde_json::json!(n))
                .await
                .unwrap()
        }));
    }
    let mut seqs = Vec::new();
    for task in tasks {
        seqs.push(task.await.unwrap());
    }
    seqs.sort();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
    for row in read_from(dir.path(), Stream::Raw, 0) {
        assert_eq!(*live.recv().await.unwrap(), row.unwrap());
    }
    assert!(live.try_recv().is_err());
    // No per-record raw sync; this crosses the actual public channel capacity.
    for _ in 0..1025 {
        log.append(ts(), "lag", serde_json::Value::Null)
            .await
            .unwrap();
    }
    assert!(matches!(
        live.recv().await,
        Err(broadcast::error::RecvError::Lagged(1))
    ));
    assert_eq!(live.recv().await.unwrap().seq, 22);
    let recovered: Vec<_> = read_from(dir.path(), Stream::Raw, 20)
        .map(|row| row.unwrap().seq)
        .collect();
    assert_eq!(recovered, (21..=1045).collect::<Vec<_>>());
}

#[derive(Clone)]
struct TraceBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for TraceBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn repair_warns_with_path_line_and_dropped_byte_count() {
    // Isolate tracing's process-wide callsite-interest cache from concurrent
    // tests installing/dropping different thread-local subscribers.
    if std::env::var_os("ATM_TEST_REPAIR_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "eventlog::tests::repair_warns_with_path_line_and_dropped_byte_count",
            ])
            .env("ATM_TEST_REPAIR_CHILD", "1")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("repair child timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = segment(
        dir.path(),
        Stream::Events,
        1,
        &[record(1), b"broken".to_vec()].concat(),
    );
    let output = TraceBuffer(Arc::new(std::sync::Mutex::new(Vec::new())));
    let capture = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || capture.clone())
        .finish();
    let log = tracing::subscriber::with_default(subscriber, || {
        EventLog::open(dir.path(), Stream::Events).unwrap()
    });
    let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("WARN") && text.contains("Dropped incomplete event log tail"),
        "{text}"
    );
    assert!(text.contains(&path.display().to_string()), "{text}");
    assert!(
        text.contains("line=2") && text.contains("dropped_bytes=6"),
        "{text}"
    );
    drop(log);
}

#[tokio::test]
async fn public_rotation_keeps_oversized_records_whole() {
    let dir = tempfile::tempdir().unwrap();
    let log = EventLog::open(dir.path(), Stream::Raw).unwrap();
    let body = serde_json::json!("x".repeat(16 * 1024 * 1024));
    assert_eq!(log.append(ts(), "large", body.clone()).await.unwrap(), 1);
    assert_eq!(
        log.append(ts(), "small", serde_json::Value::Null)
            .await
            .unwrap(),
        2
    );
    let files = reader::segments(&Stream::Raw.directory(dir.path())).unwrap();
    assert_eq!(files.iter().map(|f| f.first).collect::<Vec<_>>(), [1, 2]);
    let rows: Vec<_> = read_from(dir.path(), Stream::Raw, 0)
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].body, body);
    assert_eq!(
        read_from(dir.path(), Stream::Raw, 1)
            .next()
            .unwrap()
            .unwrap()
            .seq,
        2
    );
}

#[tokio::test]
async fn rotation_io_failure_stops_writer_without_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = record(1);
    bytes.pop();
    bytes.resize(16 * 1024 * 1024, b' ');
    bytes.push(b'\n');
    segment(dir.path(), Stream::Raw, 1, &bytes);
    let log = EventLog::open(dir.path(), Stream::Raw).unwrap();
    let mut live = log.subscribe();
    // Plant only after open so failure occurs on the actual writer thread.
    fs::create_dir(dir.path().join("log/raw/00000002.jsonl")).unwrap();
    assert!(matches!(
        log.append(ts(), "fail", serde_json::Value::Null).await,
        Err(EventLogError::Io { .. })
    ));
    assert!(live.try_recv().is_err(), "published a failed append");
    assert!(matches!(
        log.append(ts(), "later", serde_json::Value::Null).await,
        Err(EventLogError::Closed)
    ));
    drop(log);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), live.recv())
            .await
            .unwrap(),
        Err(broadcast::error::RecvError::Closed)
    ));
    assert_eq!(
        fs::read(dir.path().join("log/raw/00000001.jsonl")).unwrap(),
        bytes
    );
}

fn expire(path: &Path) {
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
}

#[tokio::test]
async fn raw_pruning_preserves_high_water_even_when_all_expire() {
    for keep_older in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let old = segment(dir.path(), Stream::Raw, 1, &record(1));
        let newest = segment(dir.path(), Stream::Raw, 2, &record(2));
        if !keep_older {
            expire(&old);
        }
        expire(&newest);
        let log = EventLog::open(dir.path(), Stream::Raw).unwrap();
        assert!(!newest.exists());
        assert_eq!(old.exists(), keep_older);
        let marker = dir.path().join("log/raw/00000003.jsonl");
        assert_eq!(fs::metadata(&marker).unwrap().len(), 0);
        drop(log);
        let log = reopen(dir.path(), Stream::Raw);
        assert_eq!(
            log.append(ts(), "continued", serde_json::Value::Null)
                .await
                .unwrap(),
            3
        );
        drop(log);
        let log = reopen(dir.path(), Stream::Raw);
        assert_eq!(
            log.append(ts(), "continued", serde_json::Value::Null)
                .await
                .unwrap(),
            4
        );
    }
}

#[tokio::test]
async fn empty_newest_marker_survives_age_and_interrupted_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let old = segment(dir.path(), Stream::Raw, 1, &record(1));
    let marker = segment(dir.path(), Stream::Raw, 9, b"");
    expire(&old);
    expire(&marker);
    let log = EventLog::open(dir.path(), Stream::Raw).unwrap();
    assert!(!old.exists());
    assert!(marker.exists());
    assert_eq!(
        log.append(ts(), "continued", serde_json::Value::Null)
            .await
            .unwrap(),
        9
    );
}

#[test]
fn event_files_are_never_pruned_and_raw_delete_errors_are_not_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let event = segment(dir.path(), Stream::Events, 1, &record(1));
    expire(&event);
    let _events = EventLog::open(dir.path(), Stream::Events).unwrap();
    assert!(event.exists());
    let old = segment(dir.path(), Stream::Raw, 1, b"");
    fs::remove_file(&old).unwrap();
    fs::create_dir(&old).unwrap();
    expire(&old);
    segment(dir.path(), Stream::Raw, 2, &record(2));
    let error = EventLog::open(dir.path(), Stream::Raw).expect_err("delete directory must fail");
    assert!(error.to_string().contains(&old.display().to_string()));
}

#[test]
fn replay_uses_numeric_names_and_can_start_within_a_segment() {
    let dir = tempfile::tempdir().unwrap();
    segment(dir.path(), Stream::Events, 99_999_999, &record(99_999_999));
    segment(
        dir.path(),
        Stream::Events,
        100_000_000,
        &[record(100_000_000), record(100_000_001)].concat(),
    );
    let rows: Vec<_> = read_from(dir.path(), Stream::Events, 0)
        .map(|r| r.unwrap().seq)
        .collect();
    assert_eq!(rows, [99_999_999, 100_000_000, 100_000_001]);
    assert_eq!(
        read_from(dir.path(), Stream::Events, 100_000_000)
            .next()
            .unwrap()
            .unwrap()
            .seq,
        100_000_001
    );
    assert!(read_from(dir.path(), Stream::Events, u64::MAX)
        .next()
        .is_none());
}

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
