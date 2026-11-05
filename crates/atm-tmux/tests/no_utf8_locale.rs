//! Real tmux check (#138): `RealTmuxClient` reads tmux's output back
//! whatever the caller's locale is.
//!
//! A tmux client counts as UTF-8 when `$TMUX` is set, or when the first
//! non-empty one of `LC_ALL`, `LC_CTYPE` and `LANG` names UTF-8. Otherwise
//! tmux prints a command's output with each tab, and each column of each
//! non-ASCII character, as `_` (seen on tmux 3.8; 3.4's source has the same
//! code). `list-panes` lines then lose their separators, and names and
//! paths lose their letters. This test removes `TMUX`, `TMUX_PANE`,
//! `LC_ALL`, `LC_CTYPE` and `LANG` from its own process; the `tmux`
//! children inherit that.
//!
//! It is the only test in this binary, so changing the process environment
//! races no other test. It skips (prints why, passes) only when tmux can't
//! run here without a locale. A server that won't start fails the test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use atm_tmux::{RealTmuxClient, TmuxClient};

/// The server's `-L` label. Its socket lives in the test's own directory
/// (`TMUX_TMPDIR`), so the label needn't be unique.
const LABEL: &str = "u8";

/// A session name that only reads back intact as UTF-8.
const SESSION: &str = "café";

/// A private tmux server, killed on drop, including when the test panics.
struct IsolatedServer;

impl IsolatedServer {
    /// Starts the server with one 80x24 session, [`SESSION`], whose pane
    /// runs `cat -` in `cwd`. As two words, tmux runs it without a shell,
    /// so no shell startup file can change its directory. `-f /dev/null`
    /// keeps the developer's tmux.conf out.
    fn start(cwd: &Path) -> Self {
        let output = Command::new("tmux")
            .args(["-L", LABEL, "-f", "/dev/null", "new-session", "-d"])
            .args(["-s", SESSION, "-x", "80", "-y", "24", "-c"])
            .arg(cwd)
            .args(["cat", "-"])
            .output()
            .expect("run tmux new-session");
        assert!(
            output.status.success(),
            "tmux runs here, but its server didn't start: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Self
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
    // With no locale set, tmux (3.2 and later) runs only if en_US.UTF-8 or
    // C.UTF-8 is installed; without them every call fails, `-u` or not.
    match Command::new("tmux").arg("-V").output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "skipping: tmux can't run without a locale here: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return;
        }
        Err(error) => {
            eprintln!("skipping: tmux not available: {error}");
            return;
        }
    }

    // A short name, so `<dir>/tmux-<uid>/u8` fits in a socket address. A
    // run that dies before the drop leaves its socket here, not in the
    // shared tmux directory.
    let dir = tempfile::Builder::new()
        .prefix("u8")
        .tempdir()
        .expect("create a temporary directory");
    std::env::set_var("TMUX_TMPDIR", dir.path());
    let cwd = dir.path().join("dé日");
    std::fs::create_dir(&cwd).expect("create the pane's directory");
    // tmux reports the resolved path, so compare against that.
    let cwd = std::fs::canonicalize(&cwd).expect("resolve the pane's directory");

    // Declared after `dir`, so it drops first: the server dies, then its
    // directory goes.
    let _server = IsolatedServer::start(&cwd);
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
