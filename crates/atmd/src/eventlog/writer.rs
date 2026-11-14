use super::reader::{open_file, segments, SegmentReader};
use super::{io_error, lock, Envelope, EventLogError, Stream};
use chrono::{DateTime, Utc};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const RAW_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

fn prune_candidates(
    files: &[(PathBuf, SystemTime)],
    now: SystemTime,
    max_age: Duration,
) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|(_, modified)| now.duration_since(*modified).is_ok_and(|age| age > max_age))
        .map(|(path, _)| path.clone())
        .collect()
}

fn create_segment(directory: &Path, seq: u64) -> Result<(File, PathBuf), EventLogError> {
    let path = directory.join(format!("{seq:08}.jsonl"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io_error(&path, e))?;
    sync_directory(directory)?;
    Ok((file, path))
}

// Persist directory entries from the existing ancestor down to the stream.
// Syncing only the stream leaves freshly created parents vulnerable to loss.
fn create_directory(
    directory: &Path,
    sync: &mut impl FnMut(&Path) -> Result<(), EventLogError>,
) -> Result<(), EventLogError> {
    let parent = directory.parent().map(|parent| {
        if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        }
    });
    if !directory.is_dir() {
        if let Some(parent) = parent.filter(|parent| *parent != directory) {
            create_directory(parent, sync)?;
        }
        fs::create_dir_all(directory).map_err(|e| io_error(directory, e))?;
    }
    if let Some(parent) = parent {
        sync(parent)?;
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> Result<(), EventLogError> {
    File::open(directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io_error(directory, e))
}

pub(super) struct Writer {
    _lock: File,
    file: File,
    path: PathBuf,
    next: Option<u64>,
    stream: Stream,
    directory: PathBuf,
    size: u64,
    max_bytes: u64,
}

impl Writer {
    pub fn open(directory: &Path, stream: Stream) -> Result<Self, EventLogError> {
        create_directory(directory, &mut sync_directory)?;
        let lock_path = directory.join(".lock");
        let lock = lock::try_lock(&lock_path)
            .map_err(|e| io_error(&lock_path, e))?
            .ok_or(EventLogError::Locked(lock_path))?;
        let files = segments(directory)?;
        // Snapshot ages before repairing the tail changes the newest mtime.
        let ages = if stream == Stream::Raw {
            files
                .iter()
                .map(|segment| {
                    let modified = fs::metadata(&segment.path)
                        .and_then(|m| m.modified())
                        .map_err(|e| io_error(&segment.path, e))?;
                    Ok((segment.path.clone(), modified))
                })
                .collect::<Result<Vec<_>, EventLogError>>()?
        } else {
            Vec::new()
        };
        let (file, path, next) = if let Some(newest) = files.last() {
            let mut reader = SegmentReader::new(newest.clone(), true)?;
            while reader.next_record()?.is_some() {}
            let mut file = open_file(&newest.path, true)?;
            if reader.tail_bytes > 0 {
                file.set_len(reader.valid_bytes)
                    .map_err(|e| io_error(&newest.path, e))?;
                file.sync_data().map_err(|e| io_error(&newest.path, e))?;
                tracing::warn!(?stream, path = %newest.path.display(), line = reader.line,
                    dropped_bytes = reader.tail_bytes, "Dropped incomplete event log tail");
            }
            file.seek(SeekFrom::End(0))
                .map_err(|e| io_error(&newest.path, e))?;
            (
                file,
                newest.path.clone(),
                reader
                    .last
                    .map_or(Some(newest.first), |last| last.checked_add(1)),
            )
        } else {
            let (file, path) = create_segment(directory, 1)?;
            (file, path, Some(1))
        };
        let size = file.metadata().map_err(|e| io_error(&path, e))?.len();
        let mut writer = Self {
            _lock: lock,
            file,
            path,
            next,
            stream,
            directory: directory.to_owned(),
            size,
            max_bytes: SEGMENT_BYTES,
        };
        let expired = prune_candidates(&ages, SystemTime::now(), RAW_MAX_AGE);
        if writer.size > 0 && expired.contains(&writer.path) {
            // Preserve the high-water mark even if an older file has a newer
            // mtime. A crash before/during deletion leaves this empty successor.
            writer.rotate()?;
        }
        for path in &expired {
            if *path != writer.path {
                fs::remove_file(path).map_err(|e| io_error(path, e))?;
            }
        }
        if !expired.is_empty() {
            sync_directory(directory)?;
        }
        Ok(writer)
    }

    pub fn append(
        &mut self,
        ts: DateTime<Utc>,
        kind: String,
        body: serde_json::Value,
    ) -> Result<Envelope, EventLogError> {
        let seq = self.next.ok_or(EventLogError::SequenceExhausted)?;
        let envelope = Envelope {
            seq,
            ts,
            kind,
            body,
        };
        let mut bytes = serde_json::to_vec(&envelope)
            .map_err(|e| io_error(&self.path, std::io::Error::other(e)))?;
        bytes.push(b'\n');
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let unknown = |source| EventLogError::WriteOutcomeUnknown {
            path: self.path.clone(),
            source,
        };
        // write_all can fail after a partial write; a failed sync can leave a
        // complete record visible to replay. Neither is a known-unwritten error.
        self.file.write_all(&bytes).map_err(unknown)?;
        if self.stream.sync_each_record() {
            self.file.sync_data().map_err(unknown)?;
        }
        self.size = self.size.saturating_add(bytes.len() as u64);
        self.next = seq.checked_add(1);
        Ok(envelope)
    }

    fn rotate(&mut self) -> Result<(), EventLogError> {
        let next = self.next.ok_or(EventLogError::SequenceExhausted)?;
        // Raw is synced at segment boundaries, never per record. Events have
        // already synced each append; syncing here also covers an empty marker.
        self.file.sync_data().map_err(|e| io_error(&self.path, e))?;
        let (file, path) = create_segment(&self.directory, next)?;
        self.file = file;
        self.path = path;
        self.size = 0;
        Ok(())
    }

    pub fn close(&self) -> Result<(), EventLogError> {
        if !self.stream.sync_each_record() {
            self.file.sync_data().map_err(|e| io_error(&self.path, e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_directories_sync_their_parents() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let directory = state.join("log/events");
        let mut synced = Vec::new();
        create_directory(&directory, &mut |path| {
            sync_directory(path)?;
            synced.push(path.to_owned());
            Ok(())
        })
        .unwrap();
        assert!(directory.is_dir());
        for path in [root.path().to_owned(), state.clone(), state.join("log")] {
            assert!(
                synced.contains(&path),
                "parent {} was not synced: {synced:?}",
                path.display()
            );
        }
        let error = create_directory(&directory, &mut |path| {
            Err(io_error(
                path,
                std::io::Error::other("injected parent sync failure"),
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("parent sync failure"));
    }

    #[test]
    fn pruning_is_strictly_older_than_limit_and_ignores_future_times() {
        let now = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let limit = Duration::from_secs(14 * 24 * 60 * 60);
        let files = vec![
            (PathBuf::from("old"), now - limit - Duration::from_secs(1)),
            (PathBuf::from("boundary"), now - limit),
            (PathBuf::from("new"), now),
            (PathBuf::from("future"), now + Duration::from_secs(1)),
        ];
        assert_eq!(prune_candidates(&files, now, limit), [PathBuf::from("old")]);
    }

    #[test]
    fn exact_rotation_limit_reopen_and_failed_rotation_keep_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = Stream::Raw.directory(dir.path());
        let ts = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut writer = Writer::open(&path, Stream::Raw).unwrap();
        writer
            .append(ts, "same".into(), serde_json::Value::Null)
            .unwrap();
        let one = writer.file.metadata().unwrap().len();
        writer.max_bytes = one * 2;
        writer
            .append(ts, "same".into(), serde_json::Value::Null)
            .unwrap();
        assert_eq!(segments(&path).unwrap().len(), 1, "rotated at exact limit");
        drop(writer);
        // Parallel subprocess tests can fork while the lock fd is open. Even
        // CLOEXEC fds retain flock in that child until exec; wait for that brief
        // inheritance window, just as the public reopen tests wait for cleanup.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut writer = loop {
            match Writer::open(&path, Stream::Raw) {
                Ok(writer) => break writer,
                Err(EventLogError::Locked(_)) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("reopen: {error}"),
            }
        };
        writer.max_bytes = one * 2;
        // Existing destination must not be overwritten; failed rotation sync
        // or create must leave the committed old file and next seq intact.
        fs::write(path.join("00000003.jsonl"), "do not overwrite").unwrap();
        assert!(writer
            .append(ts, "same".into(), serde_json::Value::Null)
            .is_err());
        assert_eq!(writer.next, Some(3));
        assert_eq!(
            fs::read_to_string(path.join("00000003.jsonl")).unwrap(),
            "do not overwrite"
        );
        fs::remove_file(path.join("00000003.jsonl")).unwrap();
        writer
            .append(ts, "same".into(), serde_json::Value::Null)
            .unwrap();
        assert_eq!(
            segments(&path)
                .unwrap()
                .iter()
                .map(|s| s.first)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        let rows: Vec<_> = super::super::read_from(dir.path(), Stream::Raw, 1)
            .map(|r| r.unwrap().seq)
            .collect();
        assert_eq!(rows, [2, 3]);
    }

    #[test]
    fn stream_sync_policy_and_write_errors_are_observable() {
        let dir = tempfile::tempdir().unwrap();
        let ts = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for stream in [Stream::Events, Stream::Raw] {
            let mut writer = Writer::open(&stream.directory(dir.path()), stream).unwrap();
            // Real Unix devices: /dev/full fails write; /dev/null accepts write
            // but rejects sync_data. Replace only the low-level I/O boundary.
            writer.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
            let error = writer
                .append(ts, "full".into(), serde_json::Value::Null)
                .unwrap_err();
            assert!(error.to_string().contains("outcome unknown"), "{error}");
            assert!(!matches!(error, EventLogError::Io { .. }));
            assert_eq!(writer.next, Some(1));
            writer.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
            let result = writer.append(ts, "sync".into(), serde_json::Value::Null);
            if stream == Stream::Events {
                let error = result.expect_err("events acknowledged without a successful sync");
                assert!(error.to_string().contains("outcome unknown"), "{error}");
                assert!(!matches!(error, EventLogError::Io { .. }));
                assert_eq!(writer.next, Some(1));
            } else {
                assert_eq!(result.unwrap().seq, 1, "raw attempted per-record sync");
                assert!(writer.close().is_err(), "raw close skipped sync");
            }
        }
    }
}
