//! Vendor-neutral observations of a harness's current screen, and launch policy data.
//!
//! These are transient facts, not registry status, delivery permission or persisted
//! session identity. Consumers must require positively empty input, no running turn
//! and no dialog, plus their own live-pane/copy-mode and continuity checks. Unknown
//! (including future enum variants) never authorizes input. There is no wire format.

/// Contents of the active editor, not a prompt echoed in conversation history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputState {
    #[default]
    Unknown,
    Empty,
    Draft,
}

/// Visible turn activity, independent of whether the editor is empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnState {
    #[default]
    Unknown,
    Running,
    NotRunning,
}

/// A positively recognized dialog, or evidence that no dialog occupies the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialogState {
    #[default]
    Unknown,
    None,
    /// The fully displayed folder; not canonicalized or authorized for trust.
    FolderTrust {
        folder: String,
    },
    Permission,
    Other,
}

/// Positive evidence of a fresh layout, not proof that a particular clear completed.
///
/// The same layout may already have been visible before the command. The caller
/// must tie observations to its own operation/transition and check input, turn and
/// dialog separately. This never asserts a new session ID or delivery readiness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FreshEditorEvidence {
    /// No positive fresh-layout evidence; this includes ordinary conversations.
    #[default]
    Unknown,
    Present,
}

/// Independent observations. Default is entirely unknown, never safe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenFacts {
    pub input: InputState,
    pub turn: TurnState,
    pub dialog: DialogState,
    pub fresh_editor: FreshEditorEvidence,
}

/// Zero-based terminal-cell coordinates in the same live viewport as the text.
/// These are not UTF-8 byte offsets; absent metadata is represented by `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCursor {
    pub column: u32,
    pub row: u32,
}

/// Adapter-owned agent-launch requirements, not legacy spawn defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchPolicy {
    /// Required argument tokens; a launch override must not silently drop them.
    pub required_args: &'static [&'static str],
    /// Flag preceding the role's model as one argument, when a model is supplied.
    pub model_flag: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_unknown() {
        let facts = ScreenFacts::default();
        assert_eq!(facts.input, InputState::Unknown);
        assert_eq!(facts.turn, TurnState::Unknown);
        assert_eq!(facts.dialog, DialogState::Unknown);
        assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown);
    }
}
