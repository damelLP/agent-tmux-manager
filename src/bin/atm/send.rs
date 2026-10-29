//! `atm send`: decide whether a message may go to a pane, then paste it.
//!
//! A message whose first non-blank character is one of the harness's
//! command prefixes is a command the harness runs itself (`/clear`,
//! `!ls`). Anything else is content for the agent. Both are pasted with
//! [`TmuxClient::paste_text`], then `Enter` follows on its own call.
//!
//! Found on sealed real TUIs (#116; Claude Code 2.1.311 and 2.1.312, pi
//! 0.73.1, codex-cli 0.163.0, tmux 3.8):
//! - Only typing into the pane runs a command. A half-typed draft in the
//!   input merges with the paste, which would submit the command as plain
//!   text. No harness has a stash key ATM can press safely (see
//!   [`DraftStash`]), so commands need `--force`.
//! - In copy mode, `Enter` goes to tmux and nothing is submitted.
//! - Without bracketed paste, each pasted newline submits a line.

use std::fmt;

use anyhow::{anyhow, bail, Context, Result};
use atm_core::{builtin_harnesses, DraftStash, HarnessDefinition, SessionView};
use atm_tmux::{PaneInputState, TmuxClient};

/// What a message does once it is submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// The harness runs it as a command (`/clear`, `!ls`).
    Operational,
    /// The agent reads it.
    Content,
}

impl fmt::Display for MessageClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Operational => "operational",
            Self::Content => "content",
        })
    }
}

/// Classifies `text` by `harness`'s command prefixes, or by every built-in
/// harness's prefixes when the harness is unknown.
///
/// Leading whitespace is skipped. Claude Code 2.1.312 submits ` /clear` as
/// text, but pi and Codex were not checked, so ATM treats it as a command.
#[must_use]
pub fn classify(text: &str, harness: Option<&HarnessDefinition>) -> MessageClass {
    let Some(first) = text.trim_start().chars().next() else {
        return MessageClass::Content;
    };
    let is_prefix = match harness {
        Some(definition) => definition.command_prefixes.contains(&first),
        None => builtin_harnesses().any(|definition| definition.command_prefixes.contains(&first)),
    };
    if is_prefix {
        MessageClass::Operational
    } else {
        MessageClass::Content
    }
}

/// Why `atm send` won't send a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// There is nothing to paste.
    Empty,
    /// The pane is in copy mode or another tmux mode.
    PaneInMode,
    /// A command spans lines.
    MultiLineCommand,
    /// Multi-line content for a program that didn't ask for bracketed paste.
    NoBracketedPaste,
    /// A draft in the input would turn the command into plain text.
    DraftUnprotected {
        harness: &'static str,
        stash: Option<DraftStash>,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the message is empty"),
            Self::PaneInMode => f.write_str(
                "the pane is in copy mode or another tmux mode, so Enter would not reach \
                 the program; leave the mode first",
            ),
            Self::MultiLineCommand => f.write_str("a command must be a single line"),
            Self::NoBracketedPaste => f.write_str(
                "the program in the pane did not ask for bracketed paste, so each line \
                 would be submitted on its own",
            ),
            Self::DraftUnprotected { harness, stash } => {
                write!(
                    f,
                    "atm can't keep a half-typed draft in {harness}'s input from merging \
                     with the command, which would submit it as plain text"
                )?;
                if let Some(DraftStash::Toggle(key)) = stash {
                    write!(
                        f,
                        " ({key} toggles {harness}'s stash, so it could restore an earlier \
                         draft instead)"
                    )?;
                }
                f.write_str("; pass --force to send it anyway")
            }
        }
    }
}

/// Decides whether a `class` message may be pasted into a pane in `state`.
/// `force` overrides only [`Refusal::DraftUnprotected`].
pub fn check(
    text: &str,
    class: MessageClass,
    harness: Option<&HarnessDefinition>,
    state: PaneInputState,
    force: bool,
) -> Result<(), Refusal> {
    if text.is_empty() {
        return Err(Refusal::Empty);
    }
    if state.in_mode {
        return Err(Refusal::PaneInMode);
    }
    let multi_line = text.contains(['\n', '\r']);
    match class {
        MessageClass::Operational if multi_line => Err(Refusal::MultiLineCommand),
        MessageClass::Operational if !force => Err(Refusal::DraftUnprotected {
            harness: harness.map_or("the harness", |definition| definition.display_name),
            stash: harness.and_then(|definition| definition.draft_stash),
        }),
        MessageClass::Content if multi_line && !state.bracketed_paste => {
            Err(Refusal::NoBracketedPaste)
        }
        _ => Ok(()),
    }
}

/// True when `session` carries its parent's pane: an in-process child,
/// such as a Claude background subagent. Typing there reaches the lead.
fn shares_lead_pane(session: &SessionView, sessions: &[SessionView]) -> bool {
    session.parent_session_id.as_ref().is_some_and(|parent| {
        sessions
            .iter()
            .any(|s| &s.id == parent && s.tmux_pane == session.tmux_pane)
    })
}

/// Resolves `target`, a session id prefix or a pane id, to the pane to type
/// into and the session that reads it, if ATM knows one.
pub fn resolve_target<'a>(
    sessions: &'a [SessionView],
    target: &str,
) -> Result<(String, Option<&'a SessionView>)> {
    if target.starts_with('%') {
        let lead = sessions
            .iter()
            .find(|s| s.tmux_pane.as_deref() == Some(target) && !shares_lead_pane(s, sessions));
        return Ok((target.to_string(), lead));
    }
    let session = crate::find_session(sessions, target)?;
    if shares_lead_pane(session, sessions) {
        bail!(
            "Session {} is a subagent running in its lead's pane; a send would type into the lead",
            session.id_short
        );
    }
    let pane = session
        .tmux_pane
        .clone()
        .ok_or_else(|| anyhow!("Session {} has no tmux pane", session.id_short))?;
    Ok((pane, Some(session)))
}

/// Checks `pane`, then pastes `text` and presses `Enter` as its own call.
/// Returns the class of the message it sent.
pub async fn send_to_pane(
    client: &dyn TmuxClient,
    pane: &str,
    text: &str,
    harness: Option<&HarnessDefinition>,
    force: bool,
) -> Result<MessageClass> {
    let class = classify(text, harness);
    let state = client
        .get_pane_input_state(pane)
        .await
        .with_context(|| format!("Failed to read the input state of pane {pane}"))?;
    check(text, class, harness, state, force)
        .map_err(|refusal| anyhow!("Refusing to send to {pane}: {refusal}"))?;
    client
        .paste_text(pane, text)
        .await
        .with_context(|| format!("Failed to paste into pane {pane}"))?;
    client
        .send_keys(pane, "Enter")
        .await
        .with_context(|| format!("Failed to send Enter to pane {pane}"))?;
    Ok(class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{find_harness_definition, SessionId};
    use atm_tmux::mock::MockCall;
    use atm_tmux::MockTmuxClient;

    fn harness(id: &str) -> &'static HarnessDefinition {
        find_harness_definition(id).unwrap_or_else(|| panic!("built-in harness {id}"))
    }

    fn session(id: &str, harness: &str, pane: Option<&str>, parent: Option<&str>) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.chars().take(8).collect(),
            harness: harness.to_string(),
            tmux_pane: pane.map(str::to_string),
            parent_session_id: parent.map(SessionId::new),
            ..SessionView::default()
        }
    }

    const PLAIN: PaneInputState = PaneInputState {
        in_mode: false,
        bracketed_paste: false,
    };
    const BRACKETED: PaneInputState = PaneInputState {
        in_mode: false,
        bracketed_paste: true,
    };

    #[test]
    fn slash_and_bang_are_operational_for_every_built_in_harness() {
        for id in ["claude", "pi", "codex", "amp", "qwen", "gemini"] {
            let definition = Some(harness(id));
            assert_eq!(classify("/clear", definition), MessageClass::Operational);
            assert_eq!(classify("!ls", definition), MessageClass::Operational);
            assert_eq!(classify("fix the tests", definition), MessageClass::Content);
            assert_eq!(classify("see a/b!", definition), MessageClass::Content);
        }
    }

    #[test]
    fn leading_whitespace_does_not_hide_a_command() {
        let claude = Some(harness("claude"));
        assert_eq!(classify("  /clear", claude), MessageClass::Operational);
        assert_eq!(classify("\t!ls", claude), MessageClass::Operational);
    }

    #[test]
    fn classification_reads_the_harness_prefixes() {
        let hash_only = HarnessDefinition {
            command_prefixes: &['#'],
            ..*harness("claude")
        };
        assert_eq!(
            classify("#cmd", Some(&hash_only)),
            MessageClass::Operational
        );
        assert_eq!(classify("/clear", Some(&hash_only)), MessageClass::Content);
    }

    #[test]
    fn an_unknown_harness_uses_every_built_in_prefix() {
        assert_eq!(classify("/clear", None), MessageClass::Operational);
        assert_eq!(classify("!ls", None), MessageClass::Operational);
        assert_eq!(classify("hello", None), MessageClass::Content);
    }

    #[test]
    fn an_empty_message_is_refused() {
        assert_eq!(
            check("", MessageClass::Content, None, BRACKETED, true),
            Err(Refusal::Empty)
        );
    }

    #[test]
    fn a_pane_in_a_mode_refuses_both_classes() {
        let in_mode = PaneInputState {
            in_mode: true,
            bracketed_paste: true,
        };
        let claude = Some(harness("claude"));
        assert_eq!(
            check("hello", MessageClass::Content, claude, in_mode, false),
            Err(Refusal::PaneInMode)
        );
        assert_eq!(
            check("/clear", MessageClass::Operational, claude, in_mode, true),
            Err(Refusal::PaneInMode)
        );
    }

    #[test]
    fn a_multi_line_command_is_refused_even_with_force() {
        let claude = Some(harness("claude"));
        for text in ["/clear\nhi", "/clear\r"] {
            assert_eq!(
                check(text, MessageClass::Operational, claude, BRACKETED, true),
                Err(Refusal::MultiLineCommand),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_command_needs_force_because_no_draft_can_be_protected() {
        for id in ["claude", "pi", "codex"] {
            let definition = harness(id);
            assert_eq!(
                check(
                    "/clear",
                    MessageClass::Operational,
                    Some(definition),
                    BRACKETED,
                    false
                ),
                Err(Refusal::DraftUnprotected {
                    harness: definition.display_name,
                    stash: definition.draft_stash,
                }),
                "{id}"
            );
            assert_eq!(
                check(
                    "/clear",
                    MessageClass::Operational,
                    Some(definition),
                    PLAIN,
                    true
                ),
                Ok(()),
                "{id}"
            );
        }
    }

    #[test]
    fn the_draft_refusal_says_why_and_names_force() {
        let claude = harness("claude");
        let refusal = check(
            "/clear",
            MessageClass::Operational,
            Some(claude),
            PLAIN,
            false,
        )
        .expect_err("refused");
        let message = refusal.to_string();
        assert!(message.contains("Claude Code"), "{message}");
        assert!(message.contains("C-s"), "{message}");
        assert!(message.contains("--force"), "{message}");

        let unknown = check("/x", MessageClass::Operational, None, PLAIN, false)
            .expect_err("refused")
            .to_string();
        assert!(unknown.contains("--force"), "{unknown}");
        assert!(!unknown.contains("C-s"), "{unknown}");
    }

    #[test]
    fn multi_line_content_needs_bracketed_paste() {
        let pi = Some(harness("pi"));
        for text in ["line one\nline two", "a\rb"] {
            assert_eq!(
                check(text, MessageClass::Content, pi, PLAIN, true),
                Err(Refusal::NoBracketedPaste),
                "{text:?}"
            );
            assert_eq!(
                check(text, MessageClass::Content, pi, BRACKETED, false),
                Ok(()),
                "{text:?}"
            );
        }
        assert_eq!(
            check("one line;", MessageClass::Content, pi, PLAIN, false),
            Ok(())
        );
    }

    #[test]
    fn a_session_id_resolves_to_its_pane_and_session() {
        let sessions = [session("lead-aaaa", "codex", Some("%5"), None)];
        let (pane, found) = resolve_target(&sessions, "lead").expect("resolves");
        assert_eq!(pane, "%5");
        assert_eq!(found.map(|s| s.harness.as_str()), Some("codex"));
    }

    #[test]
    fn a_subagent_in_its_leads_pane_is_refused() {
        let sessions = [
            session("lead-aaaa", "claude", Some("%5"), None),
            session("child-bbbb", "claude", Some("%5"), Some("lead-aaaa")),
        ];
        let error = resolve_target(&sessions, "child").expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("child-bb"), "{message}");
        assert!(message.contains("lead"), "{message}");
    }

    #[test]
    fn a_teammate_with_its_own_pane_can_be_reached() {
        let sessions = [
            session("lead-aaaa", "claude", Some("%5"), None),
            session("mate-cccc", "claude", Some("%7"), Some("lead-aaaa")),
        ];
        let (pane, found) = resolve_target(&sessions, "mate").expect("resolves");
        assert_eq!(pane, "%7");
        assert_eq!(found.map(|s| s.id.as_str()), Some("mate-cccc"));
    }

    #[test]
    fn a_pane_id_resolves_to_the_lead_in_that_pane() {
        let sessions = [
            session("child-bbbb", "claude", Some("%5"), Some("lead-aaaa")),
            session("lead-aaaa", "claude", Some("%5"), None),
        ];
        let (pane, found) = resolve_target(&sessions, "%5").expect("resolves");
        assert_eq!(pane, "%5");
        assert_eq!(found.map(|s| s.id.as_str()), Some("lead-aaaa"));

        let (pane, found) = resolve_target(&sessions, "%9").expect("resolves");
        assert_eq!(pane, "%9");
        assert!(found.is_none());
    }

    #[test]
    fn a_session_without_a_pane_is_refused() {
        let sessions = [session("solo-dddd", "pi", None, None)];
        assert!(resolve_target(&sessions, "solo").is_err());
    }

    #[tokio::test]
    async fn content_is_pasted_then_entered_on_its_own_call() {
        let mock = MockTmuxClient::new();
        mock.set_pane_input_state("%5", BRACKETED);

        let class = send_to_pane(
            &mock,
            "%5",
            "fix it;\nthen test",
            Some(harness("pi")),
            false,
        )
        .await
        .expect("sent");

        assert_eq!(class, MessageClass::Content);
        assert_eq!(
            mock.calls(),
            vec![
                MockCall::GetPaneInputState {
                    pane: "%5".to_string()
                },
                MockCall::PasteText {
                    pane: "%5".to_string(),
                    text: "fix it;\nthen test".to_string(),
                },
                MockCall::SendKeys {
                    pane: "%5".to_string(),
                    keys: "Enter".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_forced_command_is_pasted_without_pressing_the_stash_toggle() {
        let mock = MockTmuxClient::new();
        mock.set_pane_input_state("%5", BRACKETED);

        let class = send_to_pane(&mock, "%5", "/clear", Some(harness("claude")), true)
            .await
            .expect("sent");

        assert_eq!(class, MessageClass::Operational);
        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, MockCall::SendKeys { keys, .. } if keys == "C-s")),
            "{calls:?}"
        );
        assert_eq!(
            calls.last(),
            Some(&MockCall::SendKeys {
                pane: "%5".to_string(),
                keys: "Enter".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn a_refused_message_touches_nothing_but_the_state_query() {
        let mock = MockTmuxClient::new();
        mock.set_pane_input_state("%5", BRACKETED);

        let error = send_to_pane(&mock, "%5", "/clear", Some(harness("claude")), false)
            .await
            .expect_err("refused");

        assert!(error.to_string().contains("%5"), "{error}");
        assert_eq!(
            mock.calls(),
            vec![MockCall::GetPaneInputState {
                pane: "%5".to_string()
            }]
        );
    }
}
