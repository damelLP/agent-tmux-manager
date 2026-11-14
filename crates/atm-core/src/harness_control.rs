//! Vendor-neutral observations of a harness's current screen, and launch policy data.
//!
//! These are transient facts, not registry status, delivery permission or persisted
//! session identity. Consumers must require positively empty input, no running turn
//! and no dialog, plus their own live-pane/copy-mode and continuity checks. Unknown
//! (including future enum variants) never authorizes input. There is no wire format.

/// Explicit capture provenance, not escape-sequence auto-detection.
///
/// Styled requires the complete live `capture-pane -p -e -N` viewport: no
/// joining, scrollback or caller trimming. Plain is the legacy lossy capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenInput<'a> {
    Plain(&'a str),
    Styled(&'a str),
}

/// Adapter-requested capture representation. Capture boundaries map Styled to
/// ANSI capture with retained trailing spaces, and verify the returned format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenTextFormat {
    Plain,
    Styled,
}

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
///
/// Build from `Default` and set known fields; future facts must default to unknown.
/// Exhaustive construction outside this crate is intentionally unavailable:
/// ```compile_fail
/// use atm_core::ScreenFacts;
/// let facts = ScreenFacts {
///     input: Default::default(),
///     turn: Default::default(),
///     dialog: Default::default(),
///     fresh_editor: Default::default(),
/// };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
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

/// When a harness announces a session to the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionArrival {
    AtStart,
    AtFirstPrompt,
}

/// Evidence the caller must correlate with its own clear operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClearProof {
    ChangedSession { start_source: Option<&'static str> },
    ConversationToFresh { ready_unbound_noop: bool },
}

/// Adapter-owned lifecycle requirements; no action or authorization is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ControlPolicy {
    pub capture: ScreenTextFormat,
    pub session_arrival: SessionArrival,
    pub clear_command: &'static str,
    pub clear_proof: ClearProof,
    pub trust_selection_retries: u8,
}

impl ControlPolicy {
    #[must_use]
    pub const fn new(
        capture: ScreenTextFormat,
        session_arrival: SessionArrival,
        clear_command: &'static str,
        clear_proof: ClearProof,
        trust_selection_retries: u8,
    ) -> Self {
        Self {
            capture,
            session_arrival,
            clear_command,
            clear_proof,
            trust_selection_retries,
        }
    }
}

/// A screen's key vocabulary, not permission or a capability to send that key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlKey {
    Enter,
    Escape,
    Up,
    Down,
    Character(char),
}

/// Visible hook counts. These cannot establish the identity of a hook's command.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookListEntry {
    pub event: String,
    pub installed: u32,
    pub active: u32,
    /// None when the trusted layout omits the review column.
    pub review: Option<u32>,
}

impl HookListEntry {
    #[must_use]
    pub fn new(event: String, installed: u32, active: u32, review: Option<u32>) -> Self {
        Self {
            event,
            installed,
            active,
            review,
        }
    }
}

/// Complete recognized active dialog. Unknown/future variants authorize nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlDialog {
    #[default]
    Unknown,
    None,
    /// A displayed full path, never canonicalized or authorized here.
    /// Enter requires selected_accept; select_accept only changes selection.
    FolderTrust {
        folder: String,
        selected_accept: bool,
        select_accept: Option<ControlKey>,
        accept: ControlKey,
    },
    LaunchSkip {
        key: ControlKey,
    },
    CurrentWorktree {
        selected: bool,
        accept: ControlKey,
    },
    HookReview {
        default_selected: bool,
        open: ControlKey,
    },
    HookList {
        entries: Vec<HookListEntry>,
        above: bool,
        below: bool,
        /// Pending-list vocabulary; not an advertised action on a trusted list.
        trust: ControlKey,
        close: ControlKey,
    },
    Blocked,
}

/// Additional transient observations, built from Default and known fields.
///
/// conversation=false is absence of positive evidence, NOT proof of freshness.
/// Consumers still require positive fresh/editor facts and operation continuity.
/// ```compile_fail
/// use atm_core::harness_control::ControlScreen;
/// let screen = ControlScreen { facts: Default::default(),
///     dialog: Default::default(), conversation: false };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ControlScreen {
    pub facts: ScreenFacts,
    pub dialog: ControlDialog,
    pub conversation: bool,
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
