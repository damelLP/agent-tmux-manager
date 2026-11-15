// The dialogs a harness shows at its start, answered by `agent.up`.
//
// Included after `agent_up_tests.rs`, into the same module: no module docs,
// and `Starting` and the helpers of the files before it are in scope. As
// there, a test runs on the real clock until the flow has read its role, then
// on a paused clock with in-memory I/O, and synchronises on committed events,
// the flow's progress events and the order of scripted reads. A negative test
// ends when its scripted frames run out, so its frames, its Stuck reason and
// the exact keys sent are its assertion; each names the twin that sends.
//
// The frames are the adapters' real captures, in the order probe P1 recorded
// them. One thing in them is not as captured: the captures were sanitised to
// a placeholder folder, and a role's trust directory must exist, so a trust
// frame gets the test project's own directory in the placeholder's place.

/// Claude's folder trust at one width, as P1 captured it in order: "No"
/// selected, "Yes" selected after one Down, "No" again after a rapid Enter,
/// and the editor.
const CLAUDE_TRUST_FRAMES: [(&str, &str, &str, &str); 3] = [
    (
        fixture!("claude", "control_p1_A80-default-80x40_2.1.325"),
        fixture!("claude", "control_p1_A80-selected-80x40_2.1.325"),
        fixture!("claude", "control_p1_A80-reset-after-enter-80x40_2.1.325"),
        fixture!("claude", "control_p1_A80-ready-80x40_2.1.325"),
    ),
    (
        CLAUDE_TRUST,
        fixture!("claude", "control_p1_A120-selected-120x40_2.1.325"),
        fixture!("claude", "control_p1_A120-reset-after-enter-120x40_2.1.325"),
        CLAUDE_READY,
    ),
    (
        fixture!("claude", "control_p1_A200-default-200x40_2.1.325"),
        fixture!("claude", "control_p1_A200-selected-200x40_2.1.325"),
        fixture!("claude", "control_p1_A200-reset-after-enter-200x40_2.1.325"),
        fixture!("claude", "control_p1_A200-ready-200x40_2.1.325"),
    ),
];
const CLAUDE_YES: &str = CLAUDE_TRUST_FRAMES[1].1;
const CLAUDE_RESET: &str = CLAUDE_TRUST_FRAMES[1].2;
/// Codex's Folder access dialog, its accepting option selected by default.
const CODEX_FOLDER: &str =
    fixture!("codex", "control_p1_a16-default-fresh-folder-trust-100x24_0.167.0");
const CODEX_HOOKS: &str =
    fixture!("codex", "control_p1_a17-default-fresh-hook-trust-100x24_0.167.0");
/// Codex's startup warning viewer (a plain capture, as `CODEX_UPDATE`).
const CODEX_WARNING: &str =
    include_str!("../../../atm-codex-adapter/tests/fixtures/screen_codex-warning_0.167.0.txt");
/// The folders the captures were sanitised to.
const SANITISED: [&str; 2] = [
    "/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd",
    "/tmp/xxxxxxxxxxxxxxxxxxxxxx/repo-fresh",
];
const TRUSTING: &str = "trust_dirs=['.']\n";
const STILL_SHOWN: &str = "the trust dialog is still shown after it was accepted";
const NOT_SELECTED: &str = "the trust dialog does not show the accepting option selected yet";
const EXITED: &str = "the harness exited to its shell during the start";

impl Starting {
    /// A start whose role trusts its own project directory, scripted up to
    /// and including the launch's Enter.
    async fn trusting(harness: Harness) -> Self {
        let starting = Self::with_role(Rig::new().await, harness, TRUSTING).await;
        starting.script_launch();
        starting
    }

    /// The project directory: the role's cwd and, with `TRUSTING`, its one
    /// trust directory.
    fn folder(&self) -> String {
        let root = self.project.path().join("q").canonicalize().unwrap();
        root.to_str().unwrap().to_owned()
    }

    /// A real trust frame asking about this start's own folder, with
    /// `command` in front.
    fn asks(&self, fixture: &str, command: &str) -> PaneCapture {
        let text = SANITISED
            .iter()
            .fold(fixture.to_owned(), |text, folder| text.replace(folder, &self.folder()));
        assert_ne!(text, fixture, "the frame names no sanitised folder");
        Self::screen(&text, command)
    }

    /// Script the same frame twice: the flow's capture, and its key's guard.
    fn answered(&self, capture: PaneCapture) {
        self.frame(capture.clone());
        self.frame(capture);
    }

    /// The launch's effects, then these keys.
    fn launched_then(&self, keys: &[&str]) -> Vec<String> {
        let mut effects = self.launched();
        effects.extend(keys.iter().map(|key| format!("keys %1 {key}")));
        effects
    }

    /// A start that ends without Ready: the caller got an error, and the
    /// agent is Stuck for `reason` with every scripted frame read.
    async fn ends_stuck(&mut self, reason: &str, keys: &[&str]) {
        let result = self.up().await;
        assert!(result.is_err(), "{result:?}");
        assert_eq!(self.effects(), self.launched_then(keys));
        self.assert_stuck(reason).await;
        assert_eq!(self.unread(), 0);
    }

    /// `reason`, as a start says it that had sent `keys` to its trust dialog.
    fn unaccepted(&self, reason: &str, keys: &str) -> String {
        format!(
            "{reason}; atm had sent {keys} to the trust dialog for {}",
            self.folder()
        )
    }

    /// The audit of an accept whose dialog was not seen to go, written when
    /// the start ends.
    fn unseen_audit(&self, key: &str) -> serde_json::Value {
        let mut audit = self.trust_audit(key);
        audit["action"]["dialog_gone"] = false.into();
        audit
    }

    fn trust_audit(&self, key: &str) -> serde_json::Value {
        serde_json::json!({
            "pane": "%1",
            "action": {
                "method": "agent.up",
                "agent": "q/lead",
                "dialog": "folder_trust",
                "key": key,
                "folder": self.folder(),
            },
        })
    }
}

/// The `control.sent` audits committed so far.
fn sent_audits(
    committed: &mut broadcast::Receiver<std::sync::Arc<crate::eventlog::Envelope>>,
) -> Vec<serde_json::Value> {
    let mut audits = Vec::new();
    while let Ok(event) = committed.try_recv() {
        if event.kind == "control.sent" {
            audits.push(event.body.clone());
        }
    }
    audits
}

/// Let a Claude start that waits on its editor for a session have one.
async fn announce(starting: &mut Starting, call: tokio::task::JoinHandle<Result<UpResult, AgentError>>) {
    progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
    starting.follow_up();
    start_session(&starting.rig, Harness::ClaudeCode, "s1", Some("startup")).await;
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
}

#[tokio::test]
async fn claude_trust_no_down_yes_enter_editor() {
    // P1's order at each width: the dialog with "No" selected gets one Down,
    // with "Yes" selected one Enter, and the editor that follows is the
    // acceptance, audited once with its folder.
    for (no, yes, _, ready) in CLAUDE_TRUST_FRAMES {
        let mut starting = Starting::trusting(Harness::ClaudeCode).await;
        let mut committed = starting.rig.org.committed.subscribe();
        starting.answered(starting.asks(no, "claude"));
        starting.answered(starting.asks(yes, "claude"));
        starting.frame(Starting::screen(ready, "claude"));
        starting.frame(Starting::screen(ready, "claude"));
        starting.follow_up();
        let call = starting.start().await;
        progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
        // Both keys are out before any session is there.
        assert_eq!(starting.effects(), starting.launched_then(&["Down", "Enter"]));
        starting.follow_up();
        start_session(&starting.rig, Harness::ClaudeCode, "s1", Some("startup")).await;
        let result = call.await.unwrap().unwrap();
        assert_eq!(result.state, AgentStatus::Ready);
        assert_eq!(starting.effects(), starting.launched_then(&["Down", "Enter"]));
        let mut log = Vec::new();
        while let Ok(event) = committed.try_recv() {
            log.push(event);
        }
        assert_eq!(
            log.iter().map(|event| event.kind.as_str()).collect::<Vec<_>>(),
            [
                "project.registered",
                "agent.up_requested",
                "agent.pane_created",
                "control.spawned",
                "control.sent",
                "agent.session_bound",
            ]
        );
        assert_eq!(
            log.get(4).map(|event| &event.body),
            Some(&starting.trust_audit("Enter"))
        );
        assert_eq!(
            starting.agent().await.unwrap().session_id,
            Some(SessionId::new("s1"))
        );
        starting.settled().await;
        assert_eq!(starting.unread(), 0);
    }
}

#[tokio::test]
async fn trust_reset_after_enter_recovers_with_second_select_and_accept() {
    // P1 measured it: after the Enter the dialog is back with "No" selected.
    // One more Down and one more Enter, each on its own guard capture, and
    // one audit for the acceptance.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.answered(starting.asks(CLAUDE_RESET, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(Starting::screen(CLAUDE_READY, "claude"));
    starting.frame(Starting::screen(CLAUDE_READY, "claude"));
    starting.follow_up();
    let call = starting.start().await;
    announce(&mut starting, call).await;
    assert_eq!(
        starting.effects(),
        starting.launched_then(&["Down", "Enter", "Down", "Enter"])
    );
    assert_eq!(sent_audits(&mut committed), [starting.trust_audit("Enter")]);
    starting.settled().await;
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn accept_sent_dialog_unchanged_waits_without_second_enter() {
    // After the Enter the dialog still shows "Yes" selected (a stale frame,
    // or a slow harness): no second Enter, however often it is seen.
    // Twin: claude_trust_no_down_yes_enter_editor.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    // The dialog was not seen to go: the reason names the keys that were
    // sent, and the accept is audited as such when the start ends.
    let reason = starting.unaccepted(STILL_SHOWN, "Down, Enter");
    starting.ends_stuck(&reason, &["Down", "Enter"]).await;
    assert_eq!(sent_audits(&mut committed), [starting.unseen_audit("Enter")]);
}

#[tokio::test]
async fn second_reset_after_enter_is_stuck() {
    // Claude's policy allows one more round after a reset. A second reset
    // ends the start; the third dialog gets no key.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.answered(starting.asks(CLAUDE_RESET, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(starting.asks(CLAUDE_RESET, "claude"));
    let reason = starting.unaccepted(
        "the trust dialog reset again after it was accepted",
        "Down, Enter, Down, Enter",
    );
    starting
        .ends_stuck(&reason, &["Down", "Enter", "Down", "Enter"])
        .await;
}

#[tokio::test]
async fn trust_selection_changes_before_guard_sends_no_enter() {
    // The flow's capture shows "Yes" selected; by the control actor's own
    // capture the selection is back on "No". No Enter goes out, and no second
    // Down in the same round. Twin: claude_trust_no_down_yes_enter_editor.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.frame(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(starting.asks(CLAUDE_RESET, "claude"));
    starting.frame(starting.asks(CLAUDE_RESET, "claude"));
    let reason = starting.unaccepted(NOT_SELECTED, "Down");
    starting.ends_stuck(&reason, &["Down"]).await;
}

#[tokio::test]
async fn unallowlisted_folder_is_stuck_with_zero_keys() {
    // The dialog's folder must equal one of the role's trust directories as
    // a string. A role that lists none, and a role that trusts another
    // folder than the one asked about: no key. Twin (the same frame, about
    // the role's own folder): claude_trust_no_down_yes_enter_editor.
    for (harness, more, frame, command, folder) in [
        (Harness::ClaudeCode, "", CLAUDE_TRUST, "claude", SANITISED[0]),
        (Harness::ClaudeCode, TRUSTING, CLAUDE_YES, "claude", SANITISED[0]),
        (Harness::Codex, TRUSTING, CODEX_FOLDER, "codex", SANITISED[1]),
    ] {
        let mut starting = Starting::with_role(Rig::new().await, harness, more).await;
        starting.script_launch();
        starting.frame(Starting::screen(frame, command));
        starting
            .ends_stuck(
                &format!(
                    "the harness asks to trust {folder}, which is not one of the role's trust directories"
                ),
                &[],
            )
            .await;
    }
}

#[tokio::test]
async fn start_dialog_frame_with_shell_in_foreground_sends_zero_keys() {
    // The harness dies while its dialog is on the screen: the frame still
    // reads as the dialog, and tmux names the shell in front. On the guard's
    // capture that refuses the key; on the flow's next capture it ends the
    // start. Once for the select, once for the accept.
    // Twin: claude_trust_no_down_yes_enter_editor.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    starting.frame(starting.asks(CLAUDE_TRUST, "claude"));
    starting.frame(starting.asks(CLAUDE_TRUST, "zsh"));
    starting.frame(starting.asks(CLAUDE_TRUST, "zsh"));
    starting.ends_stuck(EXITED, &[]).await;

    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.frame(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(starting.asks(CLAUDE_YES, "zsh"));
    starting.frame(starting.asks(CLAUDE_YES, "zsh"));
    let reason = starting.unaccepted(EXITED, "Down");
    starting.ends_stuck(&reason, &["Down"]).await;
}

#[tokio::test]
async fn dialog_key_waits_while_pane_focused() {
    // A person looking at the pane may be answering the dialog: no key while
    // a client is focused on it, or while that can't be read. The start waits
    // and, at its end, says why.
    let focus = |capture: PaneCapture, focused| capture.with_focus(focused);
    for (focused, reason) in [
        (Some(true), "the pane is focused"),
        (None, "whether the pane is focused could not be read"),
    ] {
        // On the flow's own capture: the guard is not even asked.
        let mut starting = Starting::trusting(Harness::Codex).await;
        starting.frame(focus(starting.asks(CODEX_FOLDER, "codex"), focused));
        starting.ends_stuck(reason, &[]).await;

        // On the guard's capture, after the flow's capture showed nobody.
        let mut starting = Starting::trusting(Harness::Codex).await;
        starting.frame(starting.asks(CODEX_FOLDER, "codex"));
        starting.frame(focus(starting.asks(CODEX_FOLDER, "codex"), focused));
        starting.frame(focus(starting.asks(CODEX_FOLDER, "codex"), focused));
        starting.ends_stuck(reason, &[]).await;

        // The twin: once nobody is focused on it, the dialog is answered.
        let mut starting = Starting::trusting(Harness::Codex).await;
        starting.frame(focus(starting.asks(CODEX_FOLDER, "codex"), focused));
        starting.answered(starting.asks(CODEX_FOLDER, "codex"));
        starting.frame(Starting::screen(CODEX_FRESH, "codex"));
        let result = starting.up().await.unwrap();
        assert_eq!(result.state, AgentStatus::Ready);
        assert_eq!(starting.effects(), starting.launched_then(&["Enter"]));
        assert_eq!(starting.unread(), 0);
    }
}

#[tokio::test]
async fn codex_update_escape_once_with_audit() {
    // Codex offers an update before anything else: one Escape, audited, and
    // the start goes on to the editor. (The update frame and the editor are
    // real captures; that the editor follows the Escape directly is assumed.)
    let mut starting = Starting::new(Harness::Codex).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.script_launch();
    starting.answered(Starting::screen(CODEX_UPDATE, "codex"));
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched_then(&["Escape"]));
    assert_eq!(
        sent_audits(&mut committed),
        [serde_json::json!({
            "pane": "%1",
            "action": {
                "method": "agent.up",
                "agent": "q/lead",
                "dialog": "launch_skip",
                "key": "Escape",
            },
        })]
    );
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn update_prompt_still_shown_after_escape_is_stuck() {
    // The offer is still there after the Escape: it is not sent again.
    // Twin: codex_update_escape_once_with_audit.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_launch();
    starting.answered(Starting::screen(CODEX_UPDATE, "codex"));
    starting.frame(Starting::screen(CODEX_UPDATE, "codex"));
    starting.frame(Starting::screen(CODEX_UPDATE, "codex"));
    starting
        .ends_stuck(
            "the update prompt is still shown after it was skipped; atm had sent Escape to the update prompt",
            &["Escape"],
        )
        .await;
}

#[tokio::test]
async fn codex_folder_default_enter_with_audit() {
    // Codex's Folder access dialog has its accepting option selected: one
    // Enter and no select, audited when the dialog has gone.
    let mut starting = Starting::trusting(Harness::Codex).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched_then(&["Enter"]));
    assert_eq!(sent_audits(&mut committed), [starting.trust_audit("Enter")]);
    assert_eq!(starting.unread(), 0);

    // P1's order in a folder with new hooks: the hook review follows the
    // folder. The acceptance is audited; the review gets no key, and the
    // owner is told to do it.
    let mut starting = Starting::trusting(Harness::Codex).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    starting.frame(Starting::screen(CODEX_HOOKS, "codex"));
    let reason = starting.unaccepted(
        "the harness asks to review its hooks; review them in the pane (atm cannot trust them yet)",
        "Enter",
    );
    starting.ends_stuck(&reason, &["Enter"]).await;
    assert_eq!(sent_audits(&mut committed), [starting.trust_audit("Enter")]);
}

#[tokio::test]
async fn codex_warnings_modal_is_stuck() {
    // The startup warning viewer is a dialog atm must not answer.
    // Twin: codex_update_escape_once_with_audit (a dialog it may dismiss).
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_launch();
    starting.frame(Starting::screen(CODEX_WARNING, "codex"));
    starting
        .ends_stuck("a dialog atm must not answer is open", &[])
        .await;
}

#[tokio::test]
async fn permission_or_unknown_modal_sends_zero_keys() {
    // Claude's permission dialog (a real capture; the control read does not
    // know it as a start dialog), and a screen no read recognises, get no
    // key: the start waits on them to its end. The second is synthetic:
    // Claude's real trust dialog, about the role's own folder and with "Yes"
    // selected, whose accepting option reads differently. As captured it gets
    // an Enter (claude_trust_no_down_yes_enter_editor). Each follows a frame
    // that pins the harness and waits for another reason.
    let other = CLAUDE_YES.replace("Yes, I trust this folder", "Yes, and its subfolders");
    assert_ne!(other, CLAUDE_YES);
    for permission in [true, false] {
        let mut starting = Starting::trusting(Harness::ClaudeCode).await;
        starting.frame(Starting::screen(CLAUDE_RUNNING, "claude"));
        starting.frame(if permission {
            Starting::screen(CLAUDE_MODAL, "claude")
        } else {
            starting.asks(&other, "claude")
        });
        starting
            .ends_stuck("the screen was not recognised", &[])
            .await;
    }
}

#[tokio::test]
async fn benign_counter_change_passes_semantic_guard() {
    // The guard compares what the dialog is, not the frame: between the
    // flow's capture and the guard's the offered version changed (synthetic:
    // the real update frame with another version number), and the Escape
    // still goes out. A change of the dialog itself refuses:
    // trust_selection_changes_before_guard_sends_no_enter.
    let mut starting = Starting::new(Harness::Codex).await;
    starting.script_launch();
    let later = CODEX_UPDATE.replace("→ 0.167.1", "→ 0.168.0");
    assert_ne!(later, CODEX_UPDATE);
    starting.frame(Starting::screen(CODEX_UPDATE, "codex"));
    starting.frame(Starting::screen(&later, "codex"));
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    let result = starting.up().await.unwrap();
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(starting.effects(), starting.launched_then(&["Escape"]));
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn dialog_key_outcome_unknown_is_never_repeated() {
    // tmux takes the key and its call never returns: the call's own limit
    // answers, the key is not sent again though the dialog is still on the
    // screen, and the start stops. A guard that refused sent nothing and is
    // looked at again (dialog_key_waits_while_pane_focused).
    let mut starting = Starting::trusting(Harness::Codex).await;
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    let call = starting.start().await;
    progress(&mut starting.events, "up_answering", Some("q/lead")).await;
    starting.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    assert_told_stuck(&result, &starting.agent().await.unwrap());
    assert_eq!(starting.effects(), starting.launched_then(&["Enter"]));
    let reason = starting.agent().await.unwrap().reason.unwrap_or_default();
    assert!(
        reason.starts_with(&format!(
            "{START}: sending Enter to the trust dialog for {} failed, and whether it reached the pane is unknown",
            starting.folder()
        )),
        "{reason}"
    );
    // The two frames of a second answer were scripted and not read (a
    // scripted capture is three reads).
    assert_eq!(starting.unread(), 2 * 3);
}

#[tokio::test]
async fn deadline_during_a_dialog_key_names_the_key() {
    // Somebody is focused on the pane for most of the start's 60 seconds
    // (forty of them here, a paced capture each quarter second on the paused
    // clock), then leaves. The key goes out with less time left than its own
    // limit, and tmux never answers it: the start's deadline ends the flow
    // while the key is out, and the reason says that the key may have reached
    // the harness, not that the pane is focused.
    let mut starting = Starting::trusting(Harness::Codex).await;
    for _ in 0..160 {
        starting.frame(starting.asks(CODEX_FOLDER, "codex").with_focus(Some(true)));
    }
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    let call = starting.start().await;
    // The key comes after those forty seconds of the paused clock.
    progress_within(&mut starting.events, "up_answering", Some("q/lead"), 59).await;
    starting.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    assert_told_stuck(&result, &starting.agent().await.unwrap());
    assert_eq!(starting.effects(), starting.launched_then(&["Enter"]));
    starting
        .assert_stuck(&format!(
            "Enter was being sent to the trust dialog for {} and may have reached the harness",
            starting.folder()
        ))
        .await;
    assert_eq!(starting.unread(), 0);
}

#[tokio::test]
async fn accept_sent_then_harness_exits_names_the_key_that_was_sent() {
    // The Enter went out on the trust dialog and the harness is gone before
    // any other screen: the harness may have saved the trust. The accept is
    // audited when the start ends, as not seen to go, and the reason says
    // what was sent. Twin (the dialog goes, the audit has no such field):
    // the hook review in codex_folder_default_enter_with_audit.
    let mut starting = Starting::trusting(Harness::Codex).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CODEX_FOLDER, "codex"));
    starting.frame(starting.asks(CODEX_FOLDER, "zsh"));
    let reason = starting.unaccepted(EXITED, "Enter");
    starting.ends_stuck(&reason, &["Enter"]).await;
    assert_eq!(sent_audits(&mut committed), [starting.unseen_audit("Enter")]);
}

#[tokio::test]
async fn each_accept_is_audited_once_seen_to_go_or_not() {
    // The dialog goes after the first Enter (another screen is seen: the
    // usual audit), then comes back and gets its second round, whose Enter
    // is never seen to take. Two accepts, two audits, the second saying that
    // the dialog was not seen to go; the reason names every key of the start.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(Starting::screen(CLAUDE_RUNNING, "claude"));
    starting.answered(starting.asks(CLAUDE_RESET, "claude"));
    starting.answered(starting.asks(CLAUDE_YES, "claude"));
    starting.frame(starting.asks(CLAUDE_YES, "claude"));
    let reason = starting.unaccepted(STILL_SHOWN, "Down, Enter, Down, Enter");
    starting
        .ends_stuck(&reason, &["Down", "Enter", "Down", "Enter"])
        .await;
    assert_eq!(
        sent_audits(&mut committed),
        [starting.trust_audit("Enter"), starting.unseen_audit("Enter")]
    );
}

#[tokio::test]
async fn accept_sent_then_unknown_screen_leaves_one_audit_and_names_the_key() {
    // After Down and Enter the next screen is one the control read does not
    // know (Claude's real permission capture). The acceptance is never seen,
    // so it is not audited as seen; when the start ends, the accept is
    // audited once, as not seen to go, and the reason names the keys. Once
    // with the frames running out, once with the start's deadline (the
    // unknown screen for the rest of the sixty seconds).
    for deadline in [false, true] {
        let mut starting = Starting::trusting(Harness::ClaudeCode).await;
        let mut committed = starting.rig.org.committed.subscribe();
        starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
        starting.answered(starting.asks(CLAUDE_YES, "claude"));
        for _ in 0..if deadline { 260 } else { 1 } {
            starting.frame(Starting::screen(CLAUDE_MODAL, "claude"));
        }
        let result = starting.up().await;
        if deadline {
            assert_told_stuck(&result, &starting.agent().await.unwrap());
        } else {
            assert!(result.is_err(), "{result:?}");
            assert_eq!(starting.unread(), 0);
        }
        assert_eq!(starting.effects(), starting.launched_then(&["Down", "Enter"]));
        starting
            .assert_stuck(&starting.unaccepted("the screen was not recognised", "Down, Enter"))
            .await;
        assert_eq!(sent_audits(&mut committed), [starting.unseen_audit("Enter")]);
    }
}

#[tokio::test]
async fn select_alone_names_the_key_and_leaves_no_trust_audit() {
    // A Down went out, and then somebody is focused on the pane until the
    // start ends: nothing was accepted, so there is no folder_trust audit,
    // and the reason names the Down.
    let mut starting = Starting::trusting(Harness::ClaudeCode).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.answered(starting.asks(CLAUDE_TRUST, "claude"));
    starting.frame(starting.asks(CLAUDE_YES, "claude").with_focus(Some(true)));
    starting.frame(starting.asks(CLAUDE_YES, "claude").with_focus(Some(true)));
    let reason = starting.unaccepted("the pane is focused", "Down");
    starting.ends_stuck(&reason, &["Down"]).await;
    assert_eq!(sent_audits(&mut committed), Vec::<serde_json::Value>::new());
}
