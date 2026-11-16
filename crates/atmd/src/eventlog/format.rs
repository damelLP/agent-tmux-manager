//! Atomic publication of the shared disk-format marker.

use super::{io_error, reader::open_file, EventLogError};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const FORMAT: &[u8] = b"1\n";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(super) fn ensure_format(directory: &Path) -> Result<(), EventLogError> {
    let path = directory.join("format");
    let mut file = match open_file(&path, false) {
        Ok(file) => file,
        Err(EventLogError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            publish(directory, &path)?;
            open_file(&path, false)?
        }
        Err(error) => return Err(error),
    };
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|e| io_error(&path, e))?;
    if contents != FORMAT {
        return Err(EventLogError::UnsupportedFormat { path, contents });
    }
    Ok(())
}

fn publish(directory: &Path, marker: &Path) -> Result<(), EventLogError> {
    // Events and raw may open concurrently, even in different processes. Publish
    // a complete synced inode without overwriting an existing marker; create-new
    // plus write directly to `format` would expose an empty/partial marker.
    let (temporary, mut file) = loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".format-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error(&path, e)),
        }
    };
    let result = (|| {
        file.write_all(FORMAT)
            .and_then(|()| file.sync_all())
            .map_err(|e| io_error(&temporary, e))?;
        match fs::hard_link(&temporary, marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(io_error(marker, e)),
        }
    })();
    if let Err(error) = fs::remove_file(&temporary) {
        tracing::warn!(path = %temporary.display(), %error, "Cannot remove format temporary file");
    }
    result?;
    super::writer::sync_directory(directory)
}
