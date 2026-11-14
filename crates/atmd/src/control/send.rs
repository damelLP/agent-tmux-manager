//! Resolve session targets and safely paste text into their tmux panes.

use atm_core::SessionView;
use atm_tmux::TmuxClient;

use super::ControlError;

/// Resolves a target (session ID prefix or pane ID) to a tmux pane ID.
pub fn resolve_pane_id(sessions: &[SessionView], target: &str) -> Result<String, ControlError> {
    // If it starts with %, it's already a pane ID
    if target.starts_with('%') {
        return Ok(target.to_string());
    }

    // Search by session ID prefix
    let matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|s| s.id.as_str().starts_with(target) || s.id_short.starts_with(target))
        .collect();

    match matches.as_slice() {
        [] => Err(ControlError::NoSession {
            target: target.to_string(),
        }),
        [session] => session
            .tmux_pane
            .clone()
            .ok_or_else(|| ControlError::NoPane {
                session: session.id_short.clone(),
            }),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|s| s.id_short.as_str()).collect();
            Err(ControlError::AmbiguousTarget {
                target: target.to_string(),
                count: matches.len(),
                ids: ids.join(", "),
            })
        }
    }
}

/// Read the target pane's state, refuse unsafe input, then paste and submit.
/// Returns the pane id for the caller to report; never prints.
pub async fn send(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
    text: &str,
) -> Result<String, ControlError> {
    let pane_id = resolve_pane_id(sessions, target)?;
    send_resolved(client, sessions, target, &pane_id, text).await
}

/// The actor resolves, fences and projects rows before entering this same turn.
pub(super) async fn send_resolved(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
    pane_id: &str,
    text: &str,
) -> Result<String, ControlError> {
    let pane_state = client
        .raw_command(
            "display-message",
            &["-p", "-t", pane_id, "#{pane_in_mode} #{bracket_paste_flag}"],
        )
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to read the state of pane {pane_id}"),
            source,
        })?;
    if let Some(reason) = send_refusal(sessions, target, pane_id, &pane_state, text) {
        return Err(ControlError::SendRefused {
            pane: pane_id.into(),
            reason,
        });
    }
    client
        .paste_text(pane_id, text)
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to paste into pane {pane_id}"),
            source,
        })?;
    // Send Enter to submit the text, on its own call: tmux reads an
    // argument ending in `;` as a command separator.
    client
        .send_keys(pane_id, "Enter")
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to send Enter to pane {pane_id}"),
            source,
        })?;
    Ok(pane_id.into())
}

/// Why `atm send` must not paste `text` into `pane` for `target`, if it
/// must not (#116). `pane_state` is `#{pane_in_mode} #{bracket_paste_flag}`;
/// tmux before 3.7 has no `bracket_paste_flag` and leaves it empty, which
/// counts as unknown.
#[must_use]
fn send_refusal(
    sessions: &[SessionView],
    target: &str,
    pane: &str,
    pane_state: &str,
    text: &str,
) -> Option<&'static str> {
    let mut fields = pane_state.split_whitespace();
    let in_mode = fields.next().is_some_and(|modes| modes != "0");
    let bracketed_paste = fields.next();
    let in_pane = || {
        sessions
            .iter()
            .filter(|s| s.tmux_pane.as_deref() == Some(pane))
    };
    let named = sessions
        .iter()
        .find(|s| s.id.as_str().starts_with(target) || s.id_short.starts_with(target));
    // An in-process child, such as a Claude background subagent, carries its
    // lead's pane. A `%N` target names the pane itself.
    if !target.starts_with('%')
        && named
            .and_then(|s| s.parent_session_id.as_ref())
            .is_some_and(|parent| in_pane().any(|s| &s.id == parent))
    {
        return Some("the session is a subagent in its lead's pane; the text would reach the lead");
    }
    // A dialog (permission, question, plan) takes the paste, and Enter picks
    // its highlighted option. A subagent's dialog shows in its lead's pane.
    // A dialog denied with Esc sends no event, so the status can outlast it.
    if in_pane().any(|s| s.status == atm_core::SessionStatus::AttentionNeeded) {
        return Some(
            "the session is waiting on a dialog that Enter would answer; use `atm reply`, \
             or, if the dialog is gone, type in the pane",
        );
    }
    input_refusal(
        in_mode,
        false,
        bracketed_paste.map(|flag| flag != "0"),
        Some(text),
    )
}

/// Shared tmux-state rule. None retains U7's pre-3.7 unknown/allow policy.
pub(super) fn input_refusal(
    in_mode: bool,
    dead: bool,
    bracketed_paste: Option<bool>,
    text: Option<&str>,
) -> Option<&'static str> {
    if in_mode {
        return Some("the pane is in copy mode (or another mode), so Enter would go to tmux");
    }
    if dead {
        return Some("the pane is dead, so input would be lost");
    }
    if bracketed_paste == Some(false) && text.is_some_and(|text| text.contains(['\n', '\r'])) {
        return Some("the program didn't ask for bracketed paste, so each line would submit alone");
    }
    None
}

#[cfg(test)]
mod send_tests {
    use super::send_refusal;
    use atm_core::{SessionId, SessionStatus, SessionView};

    fn row(id: &str, pane: Option<&str>, parent: Option<&str>) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.chars().take(8).collect(),
            tmux_pane: pane.map(str::to_string),
            parent_session_id: parent.map(SessionId::new),
            ..SessionView::default()
        }
    }

    fn needs_input(mut session: SessionView) -> SessionView {
        session.status = SessionStatus::AttentionNeeded;
        session
    }

    #[test]
    fn a_command_or_message_to_an_idle_lead_is_sent() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        for text in ["/clear", "!ls", "run the tests;"] {
            assert_eq!(
                send_refusal(&sessions, "lead", "%5", "0 1\n", text),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_pane_in_copy_mode_is_refused() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        let reason = send_refusal(&sessions, "%5", "%5", "1 1\n", "hello");
        assert!(reason.is_some_and(|r| r.contains("copy mode")));
    }

    #[test]
    fn multi_line_text_is_refused_only_when_the_flag_is_explicitly_off() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        let two_lines = "line one\nline two";
        assert!(send_refusal(&sessions, "%5", "%5", "0 0\n", two_lines)
            .is_some_and(|r| r.contains("bracketed paste")));
        assert!(send_refusal(&sessions, "%5", "%5", "0 0\n", "a\rb").is_some());
        assert_eq!(
            send_refusal(&sessions, "%5", "%5", "0 1\n", two_lines),
            None
        );
        // tmux before 3.7 has no `bracket_paste_flag` and prints it empty.
        assert_eq!(send_refusal(&sessions, "%5", "%5", "0 \n", two_lines), None);
        assert_eq!(
            send_refusal(&sessions, "%5", "%5", "0 0\n", "one line"),
            None
        );
    }

    #[test]
    fn a_subagent_in_its_leads_pane_is_refused() {
        let sessions = [
            row("lead-aaaa", Some("%5"), None),
            row("child-bbbb", Some("%5"), Some("lead-aaaa")),
        ];
        let reason = send_refusal(&sessions, "child", "%5", "0 1\n", "hi");
        assert!(reason.is_some_and(|r| r.contains("subagent") && r.contains("lead")));
        // A pane target means the pane itself, which the lead reads.
        assert_eq!(send_refusal(&sessions, "%5", "%5", "0 1\n", "hi"), None);
    }

    #[test]
    fn a_teammate_with_its_own_pane_is_sent() {
        let sessions = [
            row("lead-aaaa", Some("%5"), None),
            row("mate-cccc", Some("%7"), Some("lead-aaaa")),
        ];
        assert_eq!(send_refusal(&sessions, "mate", "%7", "0 1\n", "hi"), None);
    }

    #[test]
    fn a_dialog_in_the_target_pane_is_refused() {
        let reason = |sessions: &[SessionView], target| {
            send_refusal(sessions, target, "%5", "0 1\n", "/clear")
        };
        let lead_at_dialog = [needs_input(row("lead-aaaa", Some("%5"), None))];
        // The status can outlive a dialog denied with Esc, so the reason
        // names the way out too.
        assert!(reason(&lead_at_dialog, "lead")
            .is_some_and(|r| r.contains("atm reply") && r.contains("type in the pane")));
        assert!(reason(&lead_at_dialog, "%5").is_some());
        // A background subagent's dialog shows in its lead's pane.
        let child_at_dialog = [
            row("lead-aaaa", Some("%5"), None),
            needs_input(row("child-bbbb", Some("%5"), Some("lead-aaaa"))),
        ];
        assert!(reason(&child_at_dialog, "lead").is_some());
        // A dialog in another pane doesn't matter.
        let other_pane = [
            row("lead-aaaa", Some("%5"), None),
            needs_input(row("else-dddd", Some("%9"), None)),
        ];
        assert_eq!(reason(&other_pane, "lead"), None);
    }
}
