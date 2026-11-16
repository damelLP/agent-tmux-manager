//! Nonblocking advisory file locking, independent of event-log types.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Takes an exclusive lock, or returns None if another open holds it.
/// The caller creates the parent directory and must never unlink the lock file.
pub(super) fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "lock is not a regular file",
        ));
    }
    // SAFETY: file keeps its valid fd open throughout flock; no pointers are passed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn child_lock() {
        if let Some(path) = std::env::var_os("ATM_TEST_LOCK_PATH") {
            let held = try_lock(Path::new(&path)).unwrap().is_some();
            assert_eq!(held, std::env::var("ATM_TEST_LOCK_FREE").unwrap() == "yes");
        }
    }

    #[test]
    fn lock_is_exclusive_between_processes_and_inode_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let lock = try_lock(&path).unwrap().unwrap();
        assert!(try_lock(&path).unwrap().is_none());
        for available in [false, true] {
            super::super::test_support::run_child("eventlog::lock::tests::child_lock", |command| {
                command
                    .env("ATM_TEST_LOCK_PATH", &path)
                    .env("ATM_TEST_LOCK_FREE", if available { "yes" } else { "no" });
            });
            // Unlock without deleting the persistent inode for the second child.
            if !available {
                assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
            }
        }
        assert!(path.is_file());
        assert_eq!(fs_mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(try_lock(&path).unwrap().is_some());
    }

    fn fs_mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn refuses_symlinks_fifos_and_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        symlink(dir.path().join("missing"), &path).unwrap();
        assert!(try_lock(&path).is_err());
        assert!(!dir.path().join("missing").exists());
        std::fs::remove_file(&path).unwrap();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(try_lock(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(try_lock(&path).is_err());
        assert!(try_lock(&dir.path().join("missing/lock")).is_err());
    }
}
