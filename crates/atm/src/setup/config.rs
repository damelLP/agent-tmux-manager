//! ATM's own files: `config.toml` and the tmux bindings.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::{io_err, Result, SetupError};

/// Installs the ATM tmux keybindings file to ~/.config/atm/tmux-bindings.conf.
pub(super) fn install_tmux_bindings() -> Result<()> {
    let config_dir = dirs::config_dir()
        .ok_or(SetupError::ConfigDirUnknown)?
        .join("atm");

    std::fs::create_dir_all(&config_dir).map_err(io_err("create", &config_dir))?;

    let bindings_path = config_dir.join("tmux-bindings.conf");
    let content = r#"# ATM — Agent Tmux Manager bindings
# Source this in your .tmux.conf: source-file ~/.config/atm/tmux-bindings.conf

# Spawn a new agent (default: below current pane)
bind C-n run-shell "atm spawn --target-pane #{pane_id}"

# Directional agent spawn (vim-style: h=left, j=below, k=above, l=right)
bind C-h run-shell "atm spawn --direction left --target-pane #{pane_id}"
bind C-j run-shell "atm spawn --direction below --target-pane #{pane_id}"
bind C-k run-shell "atm spawn --direction above --target-pane #{pane_id}"
bind C-l run-shell "atm spawn --direction right --target-pane #{pane_id}"

# Toggle ATM sidebar panel
bind C-a run-shell "atm toggle-panel"

# ATM popup overlay (alternative to sidebar)
bind C-s display-popup -E -w 35% -h 100% -x 0 "atm"

# Status bar integration (uncomment and add to status-right):
# set -g status-right '#(atm status) | %H:%M'
"#;

    std::fs::write(&bindings_path, content).map_err(io_err("write", &bindings_path))?;
    println!("Installed tmux bindings: {}", bindings_path.display());
    println!(
        "Add to your .tmux.conf: source-file {}",
        bindings_path.display()
    );
    Ok(())
}

/// Returns the path to atm's own configuration file (`$XDG_CONFIG_HOME/atm/config.toml`).
pub fn atm_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("atm/config.toml"))
}

fn default_atm_config_text() -> &'static str {
    r#"# ATM configuration
#
# The default harness is used by `atm workspace create`, and by `atm spawn`
# when it omits `--harness`.
[harness]
default = "claude"

# Per-harness defaults override built-ins when no env override is set.
# Environment precedence: ATM_SPAWN_<HARNESS>_BIN/ARGS, then
# ATM_SPAWN_BIN/ARGS, then this file, then built-in defaults.
#
# Example: make `atm spawn` launch pi through mise:
# [harness]
# default = "pi"
#
# [harness.pi]
# binary = "mise"
# default_args = ["x", "pi"]
#
# Example: define a custom harness:
# [harness.custom]
# binary = "custom-agent"
# default_args = ["--profile", "atm"]
# model_flag = "--model-id"
"#
}

/// Writes the default atm config to `path` if it does not already exist.
///
/// Idempotent: an existing file is left untouched so user edits are preserved.
/// Reports back whether the file was newly created.
pub fn ensure_default_atm_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create config directory", parent))?;
    }
    match fs::write(path, default_atm_config_text()) {
        Ok(()) => Ok(true),
        // Lost a race with another writer — treat as already-present.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).map_err(io_err("write default config", path)),
    }
}

#[cfg(test)]
mod config_tests {
    use super::{default_atm_config_text, ensure_default_atm_config};
    use std::fs;

    #[test]
    fn creates_config_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        let created = ensure_default_atm_config(&path).unwrap();

        assert!(created, "expected newly-created config to report true");
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, default_atm_config_text());
    }

    #[test]
    fn preserves_existing_user_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let user_content = "[harness]\ndefault = \"pi\"\n# my custom edits\n";
        fs::write(&path, user_content).unwrap();

        let created = ensure_default_atm_config(&path).unwrap();

        assert!(!created, "existing file must not be reported as created");
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, user_content, "user edits must be preserved");
    }

    #[test]
    fn second_call_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let first = ensure_default_atm_config(&path).unwrap();
        let second = ensure_default_atm_config(&path).unwrap();

        assert!(first);
        assert!(!second);
    }
}
