//! Classify closed stdout at its write boundary, never at a socket boundary.

use anyhow::Result;
use std::io;

#[derive(Debug, thiserror::Error)]
#[error("stdout reader closed")]
pub(super) struct StdoutClosed;

/// Render synchronously to stdout. All other errors retain their original chain.
pub(super) fn stdout(write: impl FnOnce(&mut io::StdoutLock<'_>) -> Result<()>) -> Result<()> {
    write(&mut io::stdout().lock()).map_err(|error| {
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        }) {
            StdoutClosed.into()
        } else {
            error
        }
    })
}
