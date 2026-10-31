//! Setup for ATM integrations across coding-agent harnesses.
//!
//! Detects which harnesses are installed (Claude Code, pi, Codex CLI,
//! future) and wires the matching hook for each:
//!
//! - **Claude Code**: writes the `atm-hook` bash script to
//!   `~/.local/bin/`, then registers it in `~/.claude/settings.json`'s
//!   `hooks` and `statusLine` blocks. A statusLine of the user's own is
//!   saved first to `claude-statusline-backup.json` in ATM's config dir
//!   (`~/.config/atm/` on Linux), and `atm uninstall` restores it.
//! - **pi** (<https://pi.dev/>): writes the `pi-atm` TypeScript
//!   extension to `~/.pi/agent/packages/pi-atm/`, then adds
//!   `"packages/pi-atm"` to `~/.pi/agent/settings.json`'s `packages`
//!   array (the entry is resolved relative to pi's `agentDir`).
//!   Mirrors how pi-amplike documents local-dev installs.
//! - **Codex CLI**: writes the `atm-codex-hook` bash script to
//!   `~/.local/bin/`, then registers it for every Codex hook event in
//!   `~/.codex/hooks.json`'s `hooks` object (5s timeout per hook; no
//!   statusLine equivalent exists). Codex requires a one-time trust
//!   approval of non-managed hooks — `setup_codex` prints the `/hooks`
//!   instruction for this. codex-cli 0.163.0 also asks at startup
//!   ("Hooks need review"), and `/hooks` still exists.

use std::io;
use std::path::{Path, PathBuf};

use atm_claude_adapter::ClaudeEventType;
use atm_codex_adapter::CodexEventType;
use thiserror::Error;

mod claude;
mod codex;
mod config;
mod hooks;
mod json;
mod pi;

use claude::{
    detect_claude_code, hook_script_path, read_settings, setup_claude_code,
    status_line_backup_path, uninstall_status_line, write_settings,
};
use codex::{
    codex_hook_script_path, detect_codex, read_codex_hooks, setup_codex, write_codex_hooks,
};
use config::install_tmux_bindings;
pub use config::{atm_config_path, ensure_default_atm_config};
use hooks::{remove_hook_entries, remove_script_file};
use json::{or_empty_if_not_object, read_json_file_or_empty};
use pi::{detect_pi, setup_pi, uninstall_pi_extension};

/// Errors from installing or removing ATM's harness integrations.
#[derive(Debug, Error)]
pub enum SetupError {
    /// The home directory could not be determined.
    #[error("Could not determine home directory")]
    HomeDirUnknown,

    /// The user config directory could not be determined.
    #[error("could not determine config directory")]
    ConfigDirUnknown,

    /// A filesystem operation (`op`, e.g. "read") on `path` failed.
    #[error("Failed to {op} {}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Parsing or serializing the JSON file at `path` failed.
    #[error("Failed to {op} {}", .path.display())]
    Json {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A settings file is valid JSON but not the shape ATM edits.
    #[error("{0}")]
    Shape(&'static str),

    /// The settings file at `path` is valid JSON but not an object.
    #[error("{} is not a JSON object", .path.display())]
    NotAnObject { path: PathBuf },
}

type Result<T> = std::result::Result<T, SetupError>;

/// Maps an io error from `op` on `path` into [`SetupError::Io`].
fn io_err<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> SetupError + 'a {
    move |source| SetupError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// Maps a serde_json error from `op` on `path` into [`SetupError::Json`].
fn json_err<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(serde_json::Error) -> SetupError + 'a {
    move |source| SetupError::Json {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// Installs atm integration for every detected coding-agent harness.
pub fn setup() -> Result<()> {
    println!("Setting up ATM...\n");

    // Detect installed harnesses up-front so the user sees what
    // will (and won't) be configured.
    let claude = detect_claude_code();
    let pi = detect_pi();
    let codex = detect_codex();

    println!("Detected coding agents:");
    println!(
        "  {} Claude Code  (~/.claude/{})",
        if claude { "✓" } else { "✗" },
        if claude { "" } else { " not present" }
    );
    println!(
        "  {} pi           (~/.pi/agent/{})",
        if pi { "✓" } else { "✗" },
        if pi { "" } else { " not present" }
    );
    println!(
        "  {} Codex CLI    (~/.codex/{})",
        if codex { "✓" } else { "✗" },
        if codex { "" } else { " not present" }
    );

    if !claude && !pi && !codex {
        println!(
            "\nNo supported agent installations found. Install Claude Code, pi, or Codex first."
        );
        return Ok(());
    }

    if claude {
        setup_claude_code()?;
    }

    if pi {
        setup_pi()?;
    }

    if codex {
        setup_codex()?;
    }

    // Step N: Install tmux keybindings (vendor-neutral).
    println!();
    install_tmux_bindings()?;

    // Step N+1: Materialize atm's own config.toml so users have a documented
    // starting point. Spawn no longer auto-creates this; setup is the only
    // writer.
    if let Some(path) = atm_config_path() {
        print!("\nWriting atm config to {}... ", path.display());
        match ensure_default_atm_config(&path)? {
            true => println!("created"),
            false => println!("already present (preserved)"),
        }
    }

    println!("\nNext step:");
    println!("  Run: atm");

    Ok(())
}

/// Removes atm hooks from Claude Code settings and the hook script
pub fn uninstall() -> Result<()> {
    println!("Uninstalling ATM...\n");

    // Step 1: Remove from Claude Code settings
    println!("Removing Claude Code hooks...");
    let mut settings = or_empty_if_not_object(read_settings())?;

    let removed = remove_hook_entries(&mut settings, ClaudeEventType::hook_names(), "atm-hook");

    if removed > 0 {
        write_settings(&settings)?;
    } else {
        println!("  No hooks found");
    }

    // Step 2: Remove statusLine if it uses atm-hook, restoring the user's
    // own from setup's backup. ATM's backup never stops uninstall: one it
    // can't read or delete is left for the user, with a warning.
    let backup_path = status_line_backup_path().ok_or(SetupError::ConfigDirUnknown)?;
    let (backup, readable) = match read_json_file_or_empty(&backup_path) {
        Ok(mut backup) => (backup.remove("statusLine"), true),
        Err(e) => {
            println!(
                "\nWarning: {e}; skipping the statusLine restore and leaving the backup in place"
            );
            (None, false)
        }
    };
    let has_backup = backup.is_some();
    let changed = uninstall_status_line(&mut settings, backup);
    if changed {
        write_settings(&settings)?;
    }
    match (changed, has_backup) {
        (true, true) => println!("\nstatusLine restored from {}", backup_path.display()),
        (true, false) => println!("\nstatusLine configuration removed"),
        (false, _) => println!("\nstatusLine - not configured by atm"),
    }
    // The backup is done with once restored, or once a statusLine of the
    // user's own replaced ATM's. With no statusLine left (settings.json
    // lost or reset) it may be the only copy, so it stays.
    if readable && (changed || settings.contains_key("statusLine")) {
        if let Err(e) = remove_script_file(&backup_path) {
            println!("\nWarning: {e}");
        }
    } else if has_backup {
        println!(
            "  Your earlier statusLine is still saved in {}",
            backup_path.display()
        );
    }

    // Step 3: Remove the hook script
    let hook_path = hook_script_path();
    print!("\nRemoving hook script {}... ", hook_path.display());
    if remove_script_file(&hook_script_path())? {
        println!("done");
    } else {
        println!("not found");
    }

    // Step 4: Uninstall pi-atm extension if present
    if detect_pi() {
        print!("\nRemoving pi-atm extension... ");
        match uninstall_pi_extension() {
            Ok(true) => println!("done"),
            Ok(false) => println!("not present"),
            Err(e) => println!("failed: {e}"),
        }
    }

    // Step 5: Remove Codex hooks if configured
    if detect_codex() {
        println!("\nRemoving Codex hooks...");
        let mut codex_settings = or_empty_if_not_object(read_codex_hooks())?;
        let codex_removed = remove_hook_entries(
            &mut codex_settings,
            CodexEventType::hook_names(),
            "atm-codex-hook",
        );

        if codex_removed > 0 {
            write_codex_hooks(&codex_settings)?;
        } else {
            println!("  No hooks found");
        }
    }

    // The script can remain after ~/.codex is removed, so clean it up
    // independently of Codex installation detection.
    print!(
        "\nRemoving codex hook script {}... ",
        codex_hook_script_path().display()
    );
    if remove_script_file(&codex_hook_script_path())? {
        println!("done");
    } else {
        println!("not found");
    }

    println!("\nATM uninstalled successfully!");
    Ok(())
}

#[cfg(test)]
mod error_tests {
    use super::{ensure_default_atm_config, io_err, json_err, SetupError};
    use std::error::Error as _;
    use std::fs;
    use std::io;
    use std::path::Path;

    /// For each `op` string used in this file, Display must match the anyhow
    /// context string setup printed before SetupError existed (`serialize`
    /// is new: that site had no context).
    #[test]
    fn display_matches_previous_messages() {
        let p = Path::new("/h/.claude/settings.json");
        let io = || io::Error::from(io::ErrorKind::PermissionDenied);
        let json = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let cases = [
            (
                SetupError::HomeDirUnknown,
                "Could not determine home directory",
            ),
            (
                SetupError::ConfigDirUnknown,
                "could not determine config directory",
            ),
            (
                io_err("read", p)(io()),
                "Failed to read /h/.claude/settings.json",
            ),
            (
                io_err("create", p)(io()),
                "Failed to create /h/.claude/settings.json",
            ),
            (
                io_err("write", p)(io()),
                "Failed to write /h/.claude/settings.json",
            ),
            (
                io_err("set permissions on", p)(io()),
                "Failed to set permissions on /h/.claude/settings.json",
            ),
            (
                io_err("remove", p)(io()),
                "Failed to remove /h/.claude/settings.json",
            ),
            (
                io_err("create config directory", p)(io()),
                "Failed to create config directory /h/.claude/settings.json",
            ),
            (
                io_err("write default config", p)(io()),
                "Failed to write default config /h/.claude/settings.json",
            ),
            (
                json_err("parse", p)(json()),
                "Failed to parse /h/.claude/settings.json",
            ),
            (
                json_err("serialize", p)(json()),
                "Failed to serialize /h/.claude/settings.json",
            ),
            (
                SetupError::Shape("hooks is not an object"),
                "hooks is not an object",
            ),
            (
                SetupError::Shape("hook type is not an array"),
                "hook type is not an array",
            ),
            (
                SetupError::Shape("hooks is not an object in codex hooks.json"),
                "hooks is not an object in codex hooks.json",
            ),
            (
                SetupError::Shape("packages is not an array in pi settings.json"),
                "packages is not an array in pi settings.json",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    /// The underlying error stays reachable via `source()`, so anyhow in
    /// `main` still prints it under "Caused by:".
    #[test]
    fn io_and_json_errors_keep_their_source() {
        let p = Path::new("/x");
        let io = io_err("read", p)(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(
            io.source().map(ToString::to_string),
            Some(io::Error::from(io::ErrorKind::NotFound).to_string())
        );

        let parse = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let expected = parse.to_string();
        let json = json_err("parse", p)(parse);
        assert_eq!(json.source().map(ToString::to_string), Some(expected));
    }

    #[test]
    fn ensure_default_atm_config_reports_a_typed_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("atm");
        fs::write(&blocker, "not a directory").unwrap();

        let err = ensure_default_atm_config(&blocker.join("config.toml")).unwrap_err();

        assert!(
            matches!(err, SetupError::Io { op: "create config directory", ref path, .. } if path == &blocker),
            "unexpected error: {err:?}"
        );
    }
}
