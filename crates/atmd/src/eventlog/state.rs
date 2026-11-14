use std::path::PathBuf;

/// Resolves state storage: nonempty ATM_STATE_DIR, XDG_STATE_HOME/atm,
/// HOME/.local/state/atm, or /tmp/atm. Relative overrides are used as given.
/// Like the original CLI resolver, non-UTF-8 environment values are ignored.
pub fn state_dir() -> PathBuf {
    if let Some(state) = std::env::var("ATM_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(state);
    }
    if let Some(xdg_state) = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(xdg_state).join("atm");
    }
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/atm"))
        .unwrap_or_else(|| PathBuf::from("/tmp/atm"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn resolver_child() {
        if let Ok(expected) = std::env::var("ATM_TEST_STATE_EXPECT") {
            assert_eq!(serde_json::to_value(state_dir()).unwrap(), expected);
        }
    }

    // Removing override precedence or the fallback breaks these public-API cases.
    #[test]
    fn resolver_precedence_in_isolated_processes() {
        for (atm, xdg, home, expected) in [
            ("/override", "/xdg", "/home/test", "/override"),
            ("relative", "/xdg", "/home/test", "relative"),
            ("", "/xdg", "/home/test", "/xdg/atm"),
            ("", "relative", "/home/test", "relative/atm"),
            ("", "", "/home/test", "/home/test/.local/state/atm"),
            ("", "", "", "/tmp/atm"),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "eventlog::state::tests::resolver_child"])
                .env("ATM_STATE_DIR", atm)
                .env("XDG_STATE_HOME", xdg)
                .env("HOME", home)
                .env("ATM_TEST_STATE_EXPECT", expected)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("resolver child timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(status.success(), "resolver case {expected}");
        }
    }
}
