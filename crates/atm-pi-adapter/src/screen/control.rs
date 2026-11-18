//! Captured: pi 1.5.0, 2026-11-14, sealed P1 private-tmux capture.
//! No startup dialogs appeared: only existing editor facts can give None.
//! Conversation after recognized startup requires at least two blank-separated
//! transcript blocks, all singly indented and free of warnings/notices/queued
//! output. Captured: pi 1.5.0, 2026-11-14, sealed P3/P5 private-tmux captures:
//! download status alone is not history. A single block or unsupported startup
//! before real history can remain false; false is not freshness. While the startup
//! banner is visible, any block with a deeper-indented line (list item or code),
//! notice (`✓ ...`) or queued input (`↳ ...`) also makes real history read false:
//! every block must be valid. Once the banner scrolls off, invalid blocks are
//! ignored, so the same history can read true if at least two valid blocks remain
//! and the usage footer is nonzero. A false-to-true change is not a new turn.
//! Arbitrary custom UI imitating transcript blocks is unverified. Scrolled captures
//! require a nonzero-usage footer; usage alone never proves conversation.
use super::*;
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlPolicy, ControlScreen, SessionArrival,
};

pub const CONTROL_POLICY: ControlPolicy = ControlPolicy::new(
    SCREEN_TEXT_FORMAT,
    SessionArrival::AtStart,
    CLEAR_COMMAND,
    ClearProof::ChangedSession { start_source: None },
    0,
);

#[must_use]
pub fn observe_control_screen(
    input: ScreenInput<'_>,
    cursor: Option<ScreenCursor>,
) -> ControlScreen {
    let mut result = ControlScreen::default();
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
            #[cfg(test)]
            DECODE_COUNT.with(|count| count.set(count.get() + 1));
            let Ok(screen) = StyledScreen::parse(text) else {
                return result;
            };
            decoded = screen;
            (decoded.text(), Some(&decoded))
        }
        _ => return result,
    };
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return result;
    }
    result.facts = inspect_prepared(text, styled).0;
    if result.facts.dialog != DialogState::None {
        return result;
    }
    result.dialog = ControlDialog::None;
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    // The existing observer already proved the active editor. Find its upper
    // border (the rule preceding the final rule), excluding the draft itself.
    let Some(lower) = lines.iter().rposition(|s| rule(s)) else {
        return result;
    };
    let Some(history) = lines.get(..lower).and_then(|before| {
        before
            .iter()
            .rposition(|s| s.starts_with("──"))
            .and_then(|top| lines.get(..top))
    }) else {
        return result;
    };
    let startup_end = (0..=history.len())
        .rev()
        .find(|end| history.get(..*end).is_some_and(startup_only));
    result.conversation = if let Some(end) = startup_end {
        history.get(end..).is_some_and(|tail| {
            let blocks: Vec<_> = tail
                .split(|s| s.trim().is_empty())
                .filter(|block| !block.is_empty())
                .collect();
            blocks.len() >= 2 && blocks.iter().all(|block| transcript_block(block))
        })
    } else {
        // P1's working/draft captures have scrolled past the banner. Do not
        // mistake damaged startup chrome for this observed transcript layout.
        let startup_visible = history.iter().any(|s| {
            s.contains("▀▀█")
                || s.contains("escape interrupt ·")
                || s.contains("Press ctrl+o")
                || s.contains("Pi can explain")
        });
        let blocks = history
            .split(|s| s.trim().is_empty())
            .filter(|block| transcript_block(block))
            .count();
        !startup_visible
            && blocks >= 2
            && lines
                .iter()
                .rev()
                .find(|s| !s.is_empty())
                .and_then(|s| usage_footer(s))
                == Some(false)
    };
    result
}

#[cfg(test)]
mod decode_tests;

fn transcript_block(block: &[&str]) -> bool {
    !block.is_empty()
        && block.iter().all(|s| {
            s.starts_with(' ')
                && !s.starts_with("  ")
                && !s.trim().starts_with(['✓', '↳'])
                && !s.trim().starts_with("Warning:")
        })
}
