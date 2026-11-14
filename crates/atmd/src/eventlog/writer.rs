use super::reader::{open_file, segments, SegmentReader};
use super::{io_error, lock, Envelope, EventLogError, Stream};
use chrono::{DateTime, Utc};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(super) struct Writer {
    _lock: File,
    file: File,
    path: PathBuf,
    next: Option<u64>,
    stream: Stream,
}

impl Writer {
    pub fn open(directory: &Path, stream: Stream) -> Result<Self, EventLogError> {
        fs::create_dir_all(directory).map_err(|e| io_error(directory, e))?;
        let lock_path = directory.join(".lock");
        let lock = lock::try_lock(&lock_path)
            .map_err(|e| io_error(&lock_path, e))?
            .ok_or(EventLogError::Locked(lock_path))?;
        let files = segments(directory)?;
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
            let path = directory.join("00000001.jsonl");
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| io_error(&path, e))?;
            File::open(directory)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| io_error(directory, e))?;
            (file, path, Some(1))
        };
        Ok(Self {
            _lock: lock,
            file,
            path,
            next,
            stream,
        })
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
        self.file
            .write_all(&bytes)
            .map_err(|e| io_error(&self.path, e))?;
        if self.stream.sync_each_record() {
            self.file.sync_data().map_err(|e| io_error(&self.path, e))?;
        }
        self.next = seq.checked_add(1);
        Ok(envelope)
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
    fn stream_sync_policy_and_write_errors_are_observable() {
        let dir = tempfile::tempdir().unwrap();
        let ts = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for stream in [Stream::Events, Stream::Raw] {
            let mut writer = Writer::open(&stream.directory(dir.path()), stream).unwrap();
            // Real Unix devices: /dev/full fails write; /dev/null accepts write
            // but rejects sync_data. Replace only the low-level I/O boundary.
            writer.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
            assert!(writer
                .append(ts, "full".into(), serde_json::Value::Null)
                .is_err());
            assert_eq!(writer.next, Some(1));
            writer.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
            let result = writer.append(ts, "sync".into(), serde_json::Value::Null);
            if stream == Stream::Events {
                assert!(
                    result.is_err(),
                    "events acknowledged without a successful sync"
                );
                assert_eq!(writer.next, Some(1));
            } else {
                assert_eq!(result.unwrap().seq, 1, "raw attempted per-record sync");
                assert!(writer.close().is_err(), "raw close skipped sync");
            }
        }
    }
}
