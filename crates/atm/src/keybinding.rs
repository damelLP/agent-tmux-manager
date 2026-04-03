//! Vim-style keybinding system for the ATM TUI.
//!
//! Implements a 3-state DFA parsing the vim grammar `[count] [motion]`,
//! backed by a `VimKeyResolver` that maps `KeyEvent` to `KeyMeaning`.
//!
//! Pending states (count accumulation, `g` prefix) persist until the next
//! keypress resolves or cancels them. There is no time-based timeout.
//!
//! All code follows the panic-free policy: saturating arithmetic,
//! no unwrap/expect/panic.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Maximum count value. Inputs that would exceed this are clamped.
const MAX_COUNT: usize = 9999;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// An action the UI layer should execute in response to user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// Move the cursor down by `n` rows.
    MoveDown(usize),
    /// Move the cursor up by `n` rows.
    MoveUp(usize),
    /// Jump to an absolute row (0-indexed).
    GoToRow(usize),
    /// Jump to the last row.
    GoToLast,
    /// Jump to the first row.
    GoToFirst,
    /// Scroll half a page down, repeated `n` times.
    HalfPageDown(usize),
    /// Scroll half a page up, repeated `n` times.
    HalfPageUp(usize),
    /// Open / jump to the currently selected session.
    JumpToSession,
    /// Refresh the display.
    Refresh,
    /// Quit the application.
    Quit,
    /// Toggle the help popup.
    ToggleHelp,
    /// Collapse the current tree node (or move to parent).
    CollapseNode,
    /// Expand the current tree node (or move to first child).
    ExpandNode,
}

// ---------------------------------------------------------------------------
// Keybinding metadata (single source of truth for help/footer displays)
// ---------------------------------------------------------------------------

/// Category for grouping keybindings in the help popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintCategory {
    /// Navigation bindings (movement, scrolling).
    Navigation,
    /// Action bindings (quit, refresh, jump, help).
    Actions,
}

/// Metadata for a single keybinding, used by both the help popup and footer bar.
///
/// Entries where `footer_key` is empty are not shown in the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeybindingHint {
    /// Key display for the help popup (e.g., "j / \u{2193}").
    pub help_key: &'static str,
    /// Description for the help popup (e.g., "Move down").
    pub help_desc: &'static str,
    /// Key display for the footer bar (e.g., "j/\u{2193}"). Empty = not shown.
    pub footer_key: &'static str,
    /// Description for the footer bar (e.g., "down"). Empty = not shown.
    pub footer_desc: &'static str,
    /// Category for help popup grouping.
    pub category: HintCategory,
    /// Only show when running inside tmux.
    pub tmux_only: bool,
}

/// All keybindings with display metadata, in presentation order.
///
/// This is the single source of truth consumed by both the help popup
/// and the footer status bar. When adding a new keybinding to the DFA,
/// add a corresponding entry here.
pub(crate) static KEYBINDING_HINTS: &[KeybindingHint] = &[
    // -- Navigation ----------------------------------------------------------
    KeybindingHint {
        help_key: "j / \u{2193}",
        help_desc: "Move down",
        footer_key: "j/\u{2193}",
        footer_desc: "down",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "k / \u{2191}",
        help_desc: "Move up",
        footer_key: "k/\u{2191}",
        footer_desc: "up",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "0 / gg",
        help_desc: "Go to top",
        footer_key: "gg",
        footer_desc: "top",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "G",
        help_desc: "Go to bottom",
        footer_key: "G",
        footer_desc: "end",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Ctrl-d",
        help_desc: "Half page down",
        footer_key: "^d/^u",
        footer_desc: "page",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Ctrl-u",
        help_desc: "Half page up",
        footer_key: "",
        footer_desc: "",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Ngg",
        help_desc: "Go to row N",
        footer_key: "",
        footer_desc: "",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Nj / Nk",
        help_desc: "Move N rows",
        footer_key: "",
        footer_desc: "",
        category: HintCategory::Navigation,
        tmux_only: false,
    },
    // -- Actions -------------------------------------------------------------
    KeybindingHint {
        help_key: "Enter",
        help_desc: "Jump to session (tmux)",
        footer_key: "Enter",
        footer_desc: "jump",
        category: HintCategory::Actions,
        tmux_only: true,
    },
    KeybindingHint {
        help_key: "r",
        help_desc: "Rescan / refresh",
        footer_key: "r",
        footer_desc: "rescan",
        category: HintCategory::Actions,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "q",
        help_desc: "Quit",
        footer_key: "q",
        footer_desc: "quit",
        category: HintCategory::Actions,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "?",
        help_desc: "Toggle this help",
        footer_key: "?",
        footer_desc: "help",
        category: HintCategory::Actions,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Esc",
        help_desc: "Close help / quit",
        footer_key: "",
        footer_desc: "",
        category: HintCategory::Actions,
        tmux_only: false,
    },
    KeybindingHint {
        help_key: "Ctrl-c",
        help_desc: "Quit",
        footer_key: "",
        footer_desc: "",
        category: HintCategory::Actions,
        tmux_only: false,
    },
];

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// The semantic meaning of a single key press, before DFA processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyMeaning {
    /// A decimal digit (0-9).
    Digit(u8),
    /// A motion command.
    Motion(MotionKind),
    /// The `g` prefix key.
    GPrefix,
    /// A self-contained action that needs no count or prefix.
    SimpleAction(UiAction),
    /// A key we don't recognise.
    Unbound,
}

/// The set of motions that can be preceded by an optional count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MotionKind {
    Down,
    Up,
    GoToBottom,
    HalfPageDown,
    HalfPageUp,
}

/// Internal DFA state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputState {
    /// Waiting for input, no partial parse in progress.
    Ready,
    /// Accumulating a numeric count prefix.
    Count(usize),
    /// Received a `g` prefix, optionally preceded by a count.
    PendingG { count: Option<usize> },
}

impl Default for InputState {
    fn default() -> Self {
        Self::Ready
    }
}

// ---------------------------------------------------------------------------
// VimKeyResolver
// ---------------------------------------------------------------------------

/// Maps raw `KeyEvent` values to their semantic `KeyMeaning`.
///
/// This is a stateless, pure function object.
#[derive(Debug, Clone, Default)]
pub(crate) struct VimKeyResolver;

impl VimKeyResolver {
    /// Resolve a crossterm `KeyEvent` into a `KeyMeaning`.
    #[must_use]
    pub fn resolve(&self, key: &KeyEvent) -> KeyMeaning {
        // Handle Ctrl combinations first (exact match to avoid triggering on Ctrl+Alt etc.).
        if key.modifiers == KeyModifiers::CONTROL {
            return match key.code {
                KeyCode::Char('d') => KeyMeaning::Motion(MotionKind::HalfPageDown),
                KeyCode::Char('u') => KeyMeaning::Motion(MotionKind::HalfPageUp),
                KeyCode::Char('c') => KeyMeaning::SimpleAction(UiAction::Quit),
                _ => KeyMeaning::Unbound,
            };
        }

        // Reject other modifier combinations (Alt, Super, Hyper, Meta).
        if key.modifiers.intersects(
            KeyModifiers::ALT
                .union(KeyModifiers::SUPER)
                .union(KeyModifiers::HYPER)
                .union(KeyModifiers::META),
        ) {
            return KeyMeaning::Unbound;
        }

        // Plain keys (no modifiers, or SHIFT which crossterm folds into the
        // character for letters).
        match key.code {
            KeyCode::Char(c) => Self::resolve_char(c),
            KeyCode::Down => KeyMeaning::Motion(MotionKind::Down),
            KeyCode::Up => KeyMeaning::Motion(MotionKind::Up),
            KeyCode::Enter => KeyMeaning::SimpleAction(UiAction::JumpToSession),
            KeyCode::Esc => KeyMeaning::SimpleAction(UiAction::Quit),
            _ => KeyMeaning::Unbound,
        }
    }

    /// Resolve a plain character (no Ctrl/Alt).
    fn resolve_char(c: char) -> KeyMeaning {
        match c {
            '0' => KeyMeaning::Digit(0),
            '1' => KeyMeaning::Digit(1),
            '2' => KeyMeaning::Digit(2),
            '3' => KeyMeaning::Digit(3),
            '4' => KeyMeaning::Digit(4),
            '5' => KeyMeaning::Digit(5),
            '6' => KeyMeaning::Digit(6),
            '7' => KeyMeaning::Digit(7),
            '8' => KeyMeaning::Digit(8),
            '9' => KeyMeaning::Digit(9),
            'j' => KeyMeaning::Motion(MotionKind::Down),
            'k' => KeyMeaning::Motion(MotionKind::Up),
            'G' => KeyMeaning::Motion(MotionKind::GoToBottom),
            'g' => KeyMeaning::GPrefix,
            'h' => KeyMeaning::SimpleAction(UiAction::CollapseNode),
            'l' => KeyMeaning::SimpleAction(UiAction::ExpandNode),
            'q' | 'Q' => KeyMeaning::SimpleAction(UiAction::Quit),
            'r' | 'R' => KeyMeaning::SimpleAction(UiAction::Refresh),
            '?' => KeyMeaning::SimpleAction(UiAction::ToggleHelp),
            _ => KeyMeaning::Unbound,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Accumulate a decimal digit into the running count, clamping at
/// [`MAX_COUNT`].
#[must_use]
fn accumulate_digit(current: usize, digit: u8) -> usize {
    current
        .saturating_mul(10)
        .saturating_add(digit as usize)
        .min(MAX_COUNT)
}

// ---------------------------------------------------------------------------
// InputHandler (public API)
// ---------------------------------------------------------------------------

/// Stateful DFA that consumes `KeyEvent`s and emits `UiAction`s.
///
/// The grammar recognised is: `[count] motion | [count] g motion | action`.
#[derive(Debug, Clone)]
pub struct InputHandler {
    state: InputState,
    resolver: VimKeyResolver,
}

impl Default for InputHandler {
    fn default() -> Self {
        Self {
            state: InputState::default(),
            resolver: VimKeyResolver,
        }
    }
}

impl InputHandler {
    /// Create a new handler in the `Ready` state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the DFA to the `Ready` state, discarding any partial input.
    pub fn reset(&mut self) {
        self.state = InputState::Ready;
    }

    /// Returns `true` when the handler is **not** in the `Ready` state,
    /// meaning it is accumulating a count or waiting for the second key of a
    /// `g`-prefix.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.state != InputState::Ready
    }

    /// Feed a single `KeyEvent` into the DFA.
    ///
    /// Returns `Some(action)` when the input sequence is complete, or `None`
    /// when more input is needed (or the key was unrecognised).
    #[must_use]
    pub fn handle(&mut self, key: KeyEvent) -> Option<UiAction> {
        let meaning = self.resolver.resolve(&key);
        self.step(meaning)
    }

    /// Execute a single DFA transition.
    fn step(&mut self, meaning: KeyMeaning) -> Option<UiAction> {
        // Take ownership of current state, replacing with Ready (the most
        // common target).
        let prev = std::mem::replace(&mut self.state, InputState::Ready);

        match prev {
            InputState::Ready => self.step_ready(meaning),
            InputState::Count(n) => self.step_count(n, meaning),
            InputState::PendingG { count } => self.step_pending_g(count, meaning),
        }
    }

    /// Transitions from the `Ready` state.
    fn step_ready(&mut self, meaning: KeyMeaning) -> Option<UiAction> {
        match meaning {
            KeyMeaning::Digit(0) => Some(UiAction::GoToFirst),
            KeyMeaning::Digit(d) => {
                self.state = InputState::Count(d as usize);
                None
            }
            KeyMeaning::GPrefix => {
                self.state = InputState::PendingG { count: None };
                None
            }
            KeyMeaning::Motion(kind) => Self::motion_with_count(1, kind),
            KeyMeaning::SimpleAction(action) => Some(action),
            KeyMeaning::Unbound => None,
        }
    }

    /// Transitions from the `Count(n)` state.
    fn step_count(&mut self, n: usize, meaning: KeyMeaning) -> Option<UiAction> {
        match meaning {
            KeyMeaning::Digit(d) => {
                self.state = InputState::Count(accumulate_digit(n, d));
                None
            }
            KeyMeaning::GPrefix => {
                self.state = InputState::PendingG { count: Some(n) };
                None
            }
            KeyMeaning::Motion(kind) => Self::motion_with_count(n, kind),
            KeyMeaning::SimpleAction(action) => Some(action),
            KeyMeaning::Unbound => None,
        }
    }

    /// Transitions from the `PendingG` state.
    fn step_pending_g(&mut self, count: Option<usize>, meaning: KeyMeaning) -> Option<UiAction> {
        match meaning {
            KeyMeaning::GPrefix => match count {
                None => Some(UiAction::GoToFirst),
                Some(n) => Some(UiAction::GoToRow(n.saturating_sub(1))),
            },
            KeyMeaning::SimpleAction(action) => Some(action),
            _ => None,
        }
    }

    /// Convert a motion + count into the appropriate `UiAction`.
    fn motion_with_count(count: usize, kind: MotionKind) -> Option<UiAction> {
        match kind {
            MotionKind::Down => Some(UiAction::MoveDown(count)),
            MotionKind::Up => Some(UiAction::MoveUp(count)),
            MotionKind::GoToBottom => {
                if count == 1 {
                    // Bare `G` (no explicit count) → go to last.
                    Some(UiAction::GoToLast)
                } else {
                    Some(UiAction::GoToRow(count.saturating_sub(1)))
                }
            }
            MotionKind::HalfPageDown => Some(UiAction::HalfPageDown(count)),
            MotionKind::HalfPageUp => Some(UiAction::HalfPageUp(count)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // -- helpers ------------------------------------------------------------

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    // -----------------------------------------------------------------------
    // Layer 1: DFA transition tests (~25)
    // -----------------------------------------------------------------------

    #[test]
    fn test_j_moves_down_1() {
        let mut h = InputHandler::new();
        assert_eq!(
            h.handle(key(KeyCode::Char('j'))),
            Some(UiAction::MoveDown(1))
        );
    }

    #[test]
    fn test_3j_moves_down_3() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('3'))), None);
        assert_eq!(
            h.handle(key(KeyCode::Char('j'))),
            Some(UiAction::MoveDown(3))
        );
    }

    #[test]
    fn test_k_moves_up_1() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('k'))), Some(UiAction::MoveUp(1)));
    }

    #[test]
    fn test_5k_moves_up_5() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('5'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('k'))), Some(UiAction::MoveUp(5)));
    }

    #[test]
    fn test_gg_goes_to_first() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('g'))), Some(UiAction::GoToFirst));
    }

    #[test]
    fn test_5gg_goes_to_row_4() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('5'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        assert_eq!(
            h.handle(key(KeyCode::Char('g'))),
            Some(UiAction::GoToRow(4))
        );
    }

    #[test]
    fn test_shift_g_goes_to_last() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('G'))), Some(UiAction::GoToLast));
    }

    #[test]
    fn test_5_g_goes_to_row_4() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('5'))), None);
        assert_eq!(
            h.handle(key(KeyCode::Char('G'))),
            Some(UiAction::GoToRow(4))
        );
    }

    #[test]
    fn test_0_in_ready_goes_to_first() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('0'))), Some(UiAction::GoToFirst));
    }

    #[test]
    fn test_10j_moves_down_10() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('1'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('0'))), None);
        assert_eq!(
            h.handle(key(KeyCode::Char('j'))),
            Some(UiAction::MoveDown(10))
        );
    }

    #[test]
    fn test_100_g_goes_to_row_99() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('1'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('0'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('0'))), None);
        assert_eq!(
            h.handle(key(KeyCode::Char('G'))),
            Some(UiAction::GoToRow(99))
        );
    }

    #[test]
    fn test_ctrl_d_half_page_down_1() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key_ctrl('d')), Some(UiAction::HalfPageDown(1)));
    }

    #[test]
    fn test_3_ctrl_d_half_page_down_3() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('3'))), None);
        assert_eq!(h.handle(key_ctrl('d')), Some(UiAction::HalfPageDown(3)));
    }

    #[test]
    fn test_ctrl_u_half_page_up_1() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key_ctrl('u')), Some(UiAction::HalfPageUp(1)));
    }

    #[test]
    fn test_2_ctrl_u_half_page_up_2() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('2'))), None);
        assert_eq!(h.handle(key_ctrl('u')), Some(UiAction::HalfPageUp(2)));
    }

    #[test]
    fn test_count_overflow_saturates() {
        let mut h = InputHandler::new();
        // Feed digits to build a count well beyond MAX_COUNT.
        for c in ['9', '9', '9', '9', '9', '9'] {
            assert_eq!(h.handle(key(KeyCode::Char(c))), None);
        }
        let action = h.handle(key(KeyCode::Char('j')));
        assert_eq!(action, Some(UiAction::MoveDown(MAX_COUNT)));
    }

    #[test]
    fn test_escape_cancels_count() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('3'))), None);
        assert!(h.is_pending());
        // Esc is SimpleAction(Quit), which resets state and emits Quit.
        assert_eq!(h.handle(key(KeyCode::Esc)), Some(UiAction::Quit));
        assert!(!h.is_pending());
    }

    #[test]
    fn test_q_quits() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('q'))), Some(UiAction::Quit));
    }

    #[test]
    fn test_ctrl_c_quits() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key_ctrl('c')), Some(UiAction::Quit));
    }

    #[test]
    fn test_enter_jumps() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Enter)), Some(UiAction::JumpToSession));
    }

    #[test]
    fn test_r_refreshes() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('r'))), Some(UiAction::Refresh));
    }

    #[test]
    fn test_g_then_j_cancels() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        assert!(h.is_pending());
        // 'j' is Motion(Down) which is "other" in PendingG → None.
        assert_eq!(h.handle(key(KeyCode::Char('j'))), None);
        assert!(!h.is_pending());
    }

    #[test]
    fn test_g_then_shift_g_cancels() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        // 'G' is Motion(GoToBottom) which is "other" in PendingG → None.
        assert_eq!(h.handle(key(KeyCode::Char('G'))), None);
        assert!(!h.is_pending());
    }

    #[test]
    fn test_unknown_key_resets() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('3'))), None);
        assert!(h.is_pending());
        // 'x' is Unbound → resets state, emits None.
        assert_eq!(h.handle(key(KeyCode::Char('x'))), None);
        assert!(!h.is_pending());
    }

    #[test]
    fn test_rapid_gg_gg_sequence() {
        let mut h = InputHandler::new();
        // First gg
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('g'))), Some(UiAction::GoToFirst));
        // Second gg
        assert_eq!(h.handle(key(KeyCode::Char('g'))), None);
        assert_eq!(h.handle(key(KeyCode::Char('g'))), Some(UiAction::GoToFirst));
    }

    #[test]
    fn test_reset_clears_state() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('5'))), None);
        assert!(h.is_pending());
        h.reset();
        assert!(!h.is_pending());
        assert_eq!(
            h.handle(key(KeyCode::Char('j'))),
            Some(UiAction::MoveDown(1))
        );
    }

    #[test]
    fn test_down_arrow_moves_down() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Down)), Some(UiAction::MoveDown(1)));
    }

    #[test]
    fn test_up_arrow_moves_up() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Up)), Some(UiAction::MoveUp(1)));
    }

    // -----------------------------------------------------------------------
    // Layer 2: VimKeyResolver tests (~15)
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolver_j_is_motion_down() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('j'))),
            KeyMeaning::Motion(MotionKind::Down)
        );
    }

    #[test]
    fn test_resolver_k_is_motion_up() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('k'))),
            KeyMeaning::Motion(MotionKind::Up)
        );
    }

    #[test]
    fn test_resolver_down_arrow() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Down)),
            KeyMeaning::Motion(MotionKind::Down)
        );
    }

    #[test]
    fn test_resolver_up_arrow() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Up)),
            KeyMeaning::Motion(MotionKind::Up)
        );
    }

    #[test]
    fn test_resolver_ctrl_d() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key_ctrl('d')),
            KeyMeaning::Motion(MotionKind::HalfPageDown)
        );
    }

    #[test]
    fn test_resolver_ctrl_u() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key_ctrl('u')),
            KeyMeaning::Motion(MotionKind::HalfPageUp)
        );
    }

    #[test]
    fn test_resolver_digit_5() {
        let r = VimKeyResolver;
        assert_eq!(r.resolve(&key(KeyCode::Char('5'))), KeyMeaning::Digit(5));
    }

    #[test]
    fn test_resolver_digit_0() {
        let r = VimKeyResolver;
        assert_eq!(r.resolve(&key(KeyCode::Char('0'))), KeyMeaning::Digit(0));
    }

    #[test]
    fn test_resolver_shift_g() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('G'))),
            KeyMeaning::Motion(MotionKind::GoToBottom)
        );
    }

    #[test]
    fn test_resolver_g_is_gprefix() {
        let r = VimKeyResolver;
        assert_eq!(r.resolve(&key(KeyCode::Char('g'))), KeyMeaning::GPrefix);
    }

    #[test]
    fn test_resolver_q_is_quit() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('q'))),
            KeyMeaning::SimpleAction(UiAction::Quit)
        );
    }

    #[test]
    fn test_resolver_enter_is_jump() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Enter)),
            KeyMeaning::SimpleAction(UiAction::JumpToSession)
        );
    }

    #[test]
    fn test_resolver_r_is_refresh() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('r'))),
            KeyMeaning::SimpleAction(UiAction::Refresh)
        );
    }

    #[test]
    fn test_resolver_alt_j_is_unbound() {
        let r = VimKeyResolver;
        let alt_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!(r.resolve(&alt_j), KeyMeaning::Unbound);
    }

    #[test]
    fn test_resolver_ctrl_g_is_unbound() {
        let r = VimKeyResolver;
        assert_eq!(r.resolve(&key_ctrl('g')), KeyMeaning::Unbound);
    }

    #[test]
    fn test_resolver_ctrl_alt_d_is_unbound() {
        let r = VimKeyResolver;
        let ctrl_alt_d = KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        );
        assert_eq!(r.resolve(&ctrl_alt_d), KeyMeaning::Unbound);
    }

    // -----------------------------------------------------------------------
    // ToggleHelp (?) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_question_mark_toggles_help() {
        let mut h = InputHandler::new();
        assert_eq!(
            h.handle(key(KeyCode::Char('?'))),
            Some(UiAction::ToggleHelp)
        );
    }

    #[test]
    fn test_resolver_question_mark_is_toggle_help() {
        let r = VimKeyResolver;
        assert_eq!(
            r.resolve(&key(KeyCode::Char('?'))),
            KeyMeaning::SimpleAction(UiAction::ToggleHelp)
        );
    }

    #[test]
    fn test_count_then_question_mark_discards_count() {
        let mut h = InputHandler::new();
        assert_eq!(h.handle(key(KeyCode::Char('3'))), None);
        // SimpleAction discards accumulated count
        assert_eq!(
            h.handle(key(KeyCode::Char('?'))),
            Some(UiAction::ToggleHelp)
        );
    }
}
