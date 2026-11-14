use std::path::PathBuf;

/// Returns the directory used for the TUI log.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(xdg_state) = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(xdg_state).join("atm"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/atm"))
}
