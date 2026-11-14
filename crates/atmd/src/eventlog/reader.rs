use super::{corrupt, io_error, Envelope, EventLogError, Stream};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct Segment {
    pub first: u64,
    pub path: PathBuf,
}

pub(super) fn segments(directory: &Path) -> Result<Vec<Segment>, EventLogError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(directory, e)),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| io_error(directory, e))?.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let first = name.parse::<u64>().ok().filter(|n| *n > 0);
        let Some(first) =
            first.filter(|_| name.len() >= 8 && name.bytes().all(|b| b.is_ascii_digit()))
        else {
            return Err(corrupt(&path, 0, "invalid segment name"));
        };
        segments.push(Segment { first, path });
    }
    segments.sort_by_key(|segment| segment.first);
    for pair in segments.windows(2) {
        if let (Some(a), Some(b)) = (pair.first(), pair.get(1)) {
            if a.first == b.first {
                return Err(corrupt(&b.path, 0, "duplicate segment sequence"));
            }
        }
    }
    Ok(segments)
}

pub(super) fn open_file(path: &Path, write: bool) -> Result<File, EventLogError> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    if !file.metadata().map_err(|e| io_error(path, e))?.is_file() {
        return Err(corrupt(path, 0, "segment is not a regular file"));
    }
    Ok(file)
}

pub(super) struct SegmentReader {
    reader: BufReader<File>,
    pub segment: Segment,
    pub line: u64,
    pub valid_bytes: u64,
    pub tail_bytes: u64,
    pub last: Option<u64>,
    allow_tail: bool,
}

impl SegmentReader {
    pub fn new(segment: Segment, allow_tail: bool) -> Result<Self, EventLogError> {
        let file = open_file(&segment.path, false)?;
        Ok(Self {
            reader: BufReader::new(file),
            segment,
            line: 0,
            valid_bytes: 0,
            tail_bytes: 0,
            last: None,
            allow_tail,
        })
    }

    pub fn next_record(&mut self) -> Result<Option<Envelope>, EventLogError> {
        let mut bytes = Vec::new();
        let count = self
            .reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| io_error(&self.segment.path, e))?;
        if count == 0 {
            return Ok(None);
        }
        self.line += 1;
        if bytes.last() != Some(&b'\n') {
            if !self.allow_tail {
                return Err(corrupt(
                    &self.segment.path,
                    self.line,
                    "unterminated older segment",
                ));
            }
            self.tail_bytes = count as u64;
            return Ok(None);
        }
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|e| corrupt(&self.segment.path, self.line, e))?;
        if self.last.is_some_and(|last| envelope.seq <= last)
            || (self.last.is_none() && envelope.seq != self.segment.first)
        {
            return Err(corrupt(
                &self.segment.path,
                self.line,
                "sequence does not increase or match segment name",
            ));
        }
        self.last = Some(envelope.seq);
        self.valid_bytes += count as u64;
        Ok(Some(envelope))
    }
}

/// Replays envelopes whose sequence is strictly greater than `after_seq`.
/// Blocking iterator, without an internal timeout: async callers must use a
/// detached thread, never `spawn_blocking`. Files are listed once; this is not a
/// live tail or a transactional snapshot. A newest incomplete tail is excluded.
/// Errors include the file and (for invalid records) its 1-based line; replay
/// stops after the first error. Missing stream directories yield no records.
pub fn read_from(
    state_dir: &Path,
    stream: Stream,
    after_seq: u64,
) -> impl Iterator<Item = Result<Envelope, EventLogError>> {
    let listed = segments(&stream.directory(state_dir));
    let (files, error) = match listed {
        Ok(files) => (files, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let start = files
        .iter()
        .rposition(|file| file.first <= after_seq)
        .unwrap_or(0);
    Replay {
        files: files
            .into_iter()
            .skip(start)
            .collect::<Vec<_>>()
            .into_iter(),
        current: None,
        previous: 0,
        after_seq,
        error,
        done: false,
    }
}

struct Replay {
    files: std::vec::IntoIter<Segment>,
    current: Option<SegmentReader>,
    previous: u64,
    after_seq: u64,
    error: Option<EventLogError>,
    done: bool,
}

impl Replay {
    fn advance(&mut self) -> Result<Option<Envelope>, EventLogError> {
        loop {
            if self.current.is_none() {
                let Some(segment) = self.files.next() else {
                    return Ok(None);
                };
                if segment.first <= self.previous {
                    return Err(corrupt(
                        &segment.path,
                        1,
                        "sequence regresses across segments",
                    ));
                }
                self.current = Some(SegmentReader::new(segment, self.files.len() == 0)?);
            }
            if let Some(reader) = &mut self.current {
                match reader.next_record()? {
                    Some(envelope) => {
                        self.previous = envelope.seq;
                        if envelope.seq > self.after_seq {
                            return Ok(Some(envelope));
                        }
                    }
                    None => {
                        self.current = None;
                    }
                }
            }
        }
    }
}

impl Iterator for Replay {
    type Item = Result<Envelope, EventLogError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = match self.error.take() {
            Some(error) => Err(error),
            None => self.advance(),
        };
        match result {
            Ok(Some(envelope)) => Some(Ok(envelope)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}
