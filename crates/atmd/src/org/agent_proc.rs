//! Reading a pane's root process from procfs.
//!
//! The lifecycle flows call this once when they record what a pane runs, or
//! check that it returned to its shell. It is I/O, so it never runs inside a
//! guard predicate. Anything unreadable gives no facts, and no facts means no
//! baseline: the caller refuses.

use super::agent_flow::ProcFacts;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{oneshot, Semaphore};

/// Reads the facts of one process; injected so tests can script them.
pub(super) type ProcReader =
    Arc<dyn Fn(u32) -> Pin<Box<dyn Future<Output = Option<ProcFacts>> + Send>> + Send + Sync>;

const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The reader the daemon uses.
#[must_use]
pub(super) fn procfs_reader() -> ProcReader {
    Arc::new(|pid| Box::pin(read(pid)))
}

/// Read `pid`'s facts on a detached thread: one of the paths is resolved on
/// the filesystem (`/etc/shells` entries), which can hang, and a hung blocking
/// task would keep the daemon from exiting. At most one read is in flight; a
/// read that can't start or finish in time answers `None`.
async fn read(pid: u32) -> Option<ProcFacts> {
    static SLOT: std::sync::LazyLock<Arc<Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));
    tokio::time::timeout(READ_TIMEOUT, async {
        let permit = SLOT.clone().acquire_owned().await.ok()?;
        let (send, receive) = oneshot::channel();
        std::thread::Builder::new()
            .name("agent-procfs".into())
            .spawn(move || {
                let _permit = permit;
                let _ = send.send(read_blocking(pid));
            })
            .ok()?;
        receive.await.ok()?
    })
    .await
    .ok()?
}

fn read_blocking(pid: u32) -> Option<ProcFacts> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    let exe = std::fs::canonicalize(format!("/proc/{pid}/exe")).ok()?;
    let shells: Vec<PathBuf> = std::fs::read_to_string("/etc/shells")
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| std::fs::canonicalize(line).ok())
        .collect();
    ProcFacts::parse(&stat, &cmdline, &children, &exe, &shells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    // Whether this system lists `sh` as a shell, read independently of the reader.
    fn sh_is_listed() -> bool {
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        std::fs::read_to_string("/etc/shells")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| std::fs::canonicalize(line.trim()).ok())
            .any(|shell| shell == sh)
    }

    #[tokio::test]
    async fn reads_a_shell_holding_a_child() {
        // A shell that runs a second shell, which says it is there and then
        // becomes `cat` on our pipe. Once the line arrives, the outer shell
        // has exactly one child, for as long as we keep the pipe open.
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "/bin/sh -c 'echo ready; exec cat'; true"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = shell.stdin.take().unwrap();
        let mut lines = BufReader::new(shell.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "ready");
        let facts = procfs_reader()(shell.id()).await.unwrap();
        assert_eq!(facts.name, "sh");
        assert_eq!(facts.children, 1);
        assert!(facts.process_group > 0);
        assert_eq!(facts.system_shell, sh_is_listed());
        // Release the child and reap the shell; then the pid has no facts.
        stdin.write_all(b"\n").unwrap();
        drop(stdin);
        let pid = shell.id();
        shell.wait().unwrap();
        assert_eq!(procfs_reader()(pid).await, None);
    }

    #[tokio::test]
    async fn reads_a_program_that_is_no_shell() {
        let facts = procfs_reader()(std::process::id()).await.unwrap();
        assert!(!facts.system_shell);
        assert!(!facts.name.is_empty());
        assert!(!facts.name.contains('/'));
        // No such process.
        assert_eq!(procfs_reader()(0).await, None);
    }
}
