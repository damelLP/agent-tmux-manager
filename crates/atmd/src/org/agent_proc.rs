//! Reading a pane's root process from procfs.
//!
//! The lifecycle flows call this once when they record what a pane runs, or
//! check that it returned to its shell. It is I/O, so it never runs inside a
//! guard predicate. Unreadable procfs facts mean no baseline. An unavailable
//! shell list changes only how the executable is matched against known shells.

use super::agent_flow::ProcFacts;
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellListIssue {
    Missing,
    Unreadable,
    Empty,
}

impl ShellListIssue {
    pub(super) const fn detail(self) -> &'static str {
        match self {
            Self::Missing => "/etc/shells is missing (using built-in shell names)",
            Self::Unreadable => "/etc/shells could not be read (using built-in shell names)",
            Self::Empty => "/etc/shells has no usable shell entries",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcSample {
    pub facts: ProcFacts,
    pub shell_list_issue: Option<ShellListIssue>,
}

// A borrowed observation can be used as process evidence without its diagnostic.
impl std::ops::Deref for ProcSample {
    type Target = ProcFacts;

    fn deref(&self) -> &Self::Target {
        &self.facts
    }
}
use tokio::sync::{oneshot, Semaphore};

/// Reads the facts of one process; injected so tests can script them.
pub(super) type ProcReader<T = ProcFacts> =
    Arc<dyn Fn(u32) -> Pin<Box<dyn Future<Output = Option<T>> + Send>> + Send + Sync>;

const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The reader the daemon uses.
#[must_use]
pub(super) fn procfs_reader() -> ProcReader<ProcSample> {
    Arc::new(|pid| Box::pin(read(pid)))
}

/// Read `pid`'s facts on a detached thread: one of the paths is resolved on
/// the filesystem (`/etc/shells` entries), which can hang, and a hung blocking
/// task would keep the daemon from exiting. At most one read is in flight; a
/// read that can't start or finish in time answers `None`.
async fn read(pid: u32) -> Option<ProcSample> {
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

fn read_blocking(pid: u32) -> Option<ProcSample> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    let exe = std::fs::canonicalize(format!("/proc/{pid}/exe")).ok()?;
    facts_with_shells(
        &stat,
        &cmdline,
        &children,
        &exe,
        std::fs::read_to_string("/etc/shells"),
        |line| std::fs::canonicalize(line),
    )
}

fn facts_with_shells(
    stat: &str,
    cmdline: &[u8],
    children: &str,
    exe: &Path,
    shell_text: io::Result<String>,
    mut resolve: impl FnMut(&str) -> io::Result<PathBuf>,
) -> Option<ProcSample> {
    let (shells, shell_list_issue) = match shell_text {
        Ok(text) => {
            let shells: Vec<_> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(|line| resolve(line).ok())
                .collect();
            let issue = shells.is_empty().then_some(ShellListIssue::Empty);
            (shells, issue)
        }
        Err(error) => {
            let issue = if error.kind() == io::ErrorKind::NotFound {
                ShellListIssue::Missing
            } else {
                ShellListIssue::Unreadable
            };
            (Vec::new(), Some(issue))
        }
    };
    let mut facts = ProcFacts::parse(stat, cmdline, children, exe, &shells)?;
    if matches!(
        shell_list_issue,
        Some(ShellListIssue::Missing | ShellListIssue::Unreadable)
    ) {
        facts.system_shell = common_shell(exe);
    }
    Some(ProcSample {
        facts,
        shell_list_issue,
    })
}

fn common_shell(exe: &Path) -> bool {
    matches!(
        exe.file_name().and_then(std::ffi::OsStr::to_str),
        Some("sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    // Whether this system's /bin/sh is a shell under the available source.
    fn sh_is_listed() -> bool {
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        match std::fs::read_to_string("/etc/shells") {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(|line| std::fs::canonicalize(line).ok())
                .any(|shell| shell == sh),
            Err(_) => common_shell(&sh),
        }
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
        let facts = procfs_reader()(shell.id()).await.unwrap().facts;
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

    #[test]
    fn missing_shell_list_still_allows_baselines() {
        use crate::org::agent_flow::{adopt_baseline, shell_baseline};
        use crate::org::agent_screen::PaneBaseline;

        let sample = |exe, argv0| {
            facts_with_shells(
                "4242 (root) S 1 4242 4242 34816 4242",
                argv0,
                "",
                Path::new(exe),
                Err(io::ErrorKind::NotFound.into()),
                |_| panic!("no file entries to resolve"),
            )
            .expect("missing shell list must not suppress valid proc facts")
        };
        let shell = sample("/bin/dash", &b"-sh\0"[..]);
        assert_eq!(shell.shell_list_issue, Some(ShellListIssue::Missing));
        assert_eq!(
            shell_baseline(&shell.facts, 4242, "sh"),
            Some(PaneBaseline::shell(4242, "sh".into()))
        );
        let harness = sample("/bin/node", &b"node\0"[..]);
        assert_eq!(
            adopt_baseline(&harness.facts, 4242, "node"),
            Some(PaneBaseline::adopted(4242))
        );
        assert!(!harness.facts.system_shell);
    }

    #[test]
    fn shell_list_source_preserves_classification_guards() {
        use crate::org::agent_flow::{adopt_baseline, classify_pane, shell_baseline, PaneShape};

        const SAME_GROUP: &str = "4242 (root) S 1 4242 4242 34816 4242";
        const FOREGROUND_JOB: &str = "4242 (root) S 1 4242 4242 34816 4300";
        let read =
            |stat, cmdline, children, exe, source| {
                facts_with_shells(stat, cmdline, children, Path::new(exe), source, |entry| {
                    match entry {
                        "/bin/custom-alias" => Ok(PathBuf::from("/bin/custom-shell")),
                        "/bin/unresolvable" => Err(io::ErrorKind::NotFound.into()),
                        _ => panic!("unexpected shell entry: {entry}"),
                    }
                })
            };
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidData] {
            for shell in [
                "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu",
            ] {
                let exe = format!("/bin/{shell}");
                let sample = facts_with_shells(
                    SAME_GROUP,
                    b"sh\0",
                    "",
                    Path::new(&exe),
                    Err(kind.into()),
                    |_| panic!("no file entries to resolve"),
                )
                .unwrap();
                assert_eq!(sample.shell_list_issue, Some(ShellListIssue::Unreadable));
                assert_eq!(classify_pane(&sample.facts), Some(PaneShape::IdleShell));
            }
        }

        for (exe, expected) in [("/bin/custom-shell", true), ("/bin/bash", false)] {
            let sample = read(
                SAME_GROUP,
                &b"sh\0"[..],
                "",
                exe,
                Ok(" # shells\n\n /bin/custom-alias \n/bin/unresolvable\n".into()),
            )
            .unwrap();
            assert_eq!(sample.shell_list_issue, None);
            assert_eq!(sample.facts.system_shell, expected, "{exe}");
        }
        for text in ["", " # shell list\n", "/bin/unresolvable\n"] {
            let sample =
                read(SAME_GROUP, &b"bash\0"[..], "", "/bin/bash", Ok(text.into())).unwrap();
            assert_eq!(
                sample.shell_list_issue,
                Some(ShellListIssue::Empty),
                "{text:?}"
            );
            assert_eq!(classify_pane(&sample.facts), Some(PaneShape::RootProgram));
        }

        let missing = || Err(io::ErrorKind::NotFound.into());
        let false_argv = read(SAME_GROUP, &b"/bin/bash\0"[..], "", "/bin/node", missing()).unwrap();
        assert!(!false_argv.facts.system_shell);
        let with_child = read(SAME_GROUP, &b"bash\0"[..], "4300", "/bin/bash", missing()).unwrap();
        assert_eq!(
            classify_pane(&with_child.facts),
            Some(PaneShape::ShellHoldingChild)
        );
        assert!(shell_baseline(&with_child.facts, 4242, "bash").is_none());
        assert!(adopt_baseline(&with_child.facts, 4242, "bash").is_none());
        let job = read(
            FOREGROUND_JOB,
            &b"bash\0"[..],
            "4300",
            "/bin/bash",
            missing(),
        )
        .unwrap();
        assert!(adopt_baseline(&job.facts, 4242, "bash").is_none());
        assert!(adopt_baseline(&job.facts, 4242, "node").is_some());
        assert!(read("invalid", &b"bash\0"[..], "", "/bin/bash", missing()).is_none());
    }

    #[tokio::test]
    async fn reads_a_program_that_is_no_shell() {
        let facts = procfs_reader()(std::process::id()).await.unwrap().facts;
        assert!(!facts.system_shell);
        assert!(!facts.name.is_empty());
        assert!(!facts.name.contains('/'));
        // No such process.
        assert_eq!(procfs_reader()(0).await, None);
    }
}
