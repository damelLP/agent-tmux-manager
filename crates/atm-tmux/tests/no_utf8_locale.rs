//! Real tmux check (#138): `RealTmuxClient` reads tmux's output back
//! whatever the caller's locale is.
//!
//! A tmux client counts as UTF-8 when `$TMUX` is set, or when the first
//! non-empty one of `LC_ALL`, `LC_CTYPE` and `LANG` names UTF-8. Otherwise
//! tmux prints each tab and each non-ASCII character of a command's output
//! as `_` (tmux 3.4 and 3.8), so `list-panes` lines lose their separators
//! and names and paths lose their letters. This test removes all four
//! variables from its own process; the `tmux` children inherit that.
//!
//! It is the only test in this binary, so changing the process environment
//! races no other test. Skips (prints a message, passes) when tmux can't
//! start a server.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use atm_tmux::{RealTmuxClient, TmuxClient};

/// The server's `-L` label. Its socket lives in the test's own directory
/// (`TMUX_TMPDIR`), so the label needn't be unique.
const LABEL: &str = "u8";

/// A session name that only reads back intact as UTF-8.
const SESSION: &str = "café";

/// A temporary directory, removed on drop.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private tmux server, killed on drop, including when the test panics.
struct IsolatedServer;

impl IsolatedServer {
    /// Starts the server with one 80x24 session, [`SESSION`], whose pane
    /// runs in `cwd`. `-f /dev/null` keeps the developer's tmux.conf out.
    fn start(cwd: &Path) -> Option<Self> {
        let started = Command::new("tmux")
            .args(["-L", LABEL, "-f", "/dev/null", "new-session", "-d"])
            .args(["-s", SESSION, "-x", "80", "-y", "24", "-c"])
            .arg(cwd)
            .arg("cat")
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        started.then_some(Self)
    }
}

impl Drop for IsolatedServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", LABEL, "kill-server"])
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
async fn panes_and_paths_read_back_without_a_utf8_locale() {
    for var in ["TMUX", "TMUX_PANE", "LC_ALL", "LC_CTYPE", "LANG"] {
        std::env::remove_var(var);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Short, so `<dir>/tmux-<uid>/u8` fits in a socket address.
    let dir =
        ScratchDir(std::env::temp_dir().join(format!("atm-u8-{}-{nanos}", std::process::id())));
    let cwd = dir.0.join("dé日");
    std::fs::create_dir_all(&cwd).expect("create the pane's directory");
    // tmux reports the resolved path, so compare against that.
    let cwd = std::fs::canonicalize(&cwd).expect("resolve the pane's directory");
    // A run that dies before the drop leaves its socket here, not in the
    // shared tmux directory.
    std::env::set_var("TMUX_TMPDIR", &dir.0);

    // Declared after `dir`, so it drops first: the server dies, then its
    // directory goes.
    let Some(_server) = IsolatedServer::start(&cwd) else {
        eprintln!("tmux not available, skipping");
        return;
    };
    let client = RealTmuxClient::with_socket(LABEL);

    let panes = client.list_panes().await.expect("list_panes");
    assert_eq!(panes.len(), 1, "one pane, not none: {panes:?}");
    let pane = &panes[0];
    assert!(pane.pane_id.starts_with('%'), "{pane:?}");
    assert_eq!(pane.session_name, SESSION, "{pane:?}");
    assert_ne!(pane.pane_pid, 0, "{pane:?}");
    assert_eq!(
        (pane.window_index, pane.width, pane.height, pane.is_active),
        (0, 80, 24, true),
        "{pane:?}"
    );

    let read_cwd = client
        .get_pane_cwd(&pane.pane_id)
        .await
        .expect("get_pane_cwd");
    assert_eq!(read_cwd.map(PathBuf::from), Some(cwd));
}
