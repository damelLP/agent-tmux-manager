// Captured: Claude Code 2.1.325, 2026-11-15, read-only styled tmux captures.
// The adapter fixture README documents cropping and status-value redaction.
const LEAD_IDLE: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/screen_styled_lead-idle-shells_2.1.325.txt"
);
const LEAD_WORKING: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/screen_styled_lead-working-shells_2.1.325.txt"
);

#[tokio::test]
async fn lead_editor_chrome_pins_during_work_and_binds_when_idle() {
    for (text, status) in [
        (LEAD_IDLE, AgentStatus::Ready),
        (LEAD_WORKING, AgentStatus::Starting),
    ] {
        let mut adoption = Adoption::new().await;
        adoption
            .rig
            .row_of("lead-session", "%1", false, Harness::ClaudeCode)
            .await;
        *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
        let untagged = Adoption::inspection(None, "claude", text);
        adoption
            .rig
            .client
            .script_inspect(&pane("%1"), Some(&untagged))
            .unwrap();
        adoption.script_untagged_capture(text, "claude");
        adoption.script_untagged_capture(text, "claude");
        adoption.rig.client.script_tag_check(&untagged).unwrap();
        adoption.script_follow_up_of(
            "claude",
            (status == AgentStatus::Starting).then_some((text, "claude")),
        );
        assert_eq!(adoption.adopt().await.unwrap().state, status);
        assert_eq!(
            adoption.agents.baseline_of(&adoptee(), "%1"),
            PaneBaseline::adopted(100)
                .with_harness("claude".into())
                .unwrap()
        );
        assert_eq!(
            adoption.agent().await.unwrap().session_id,
            (status == AgentStatus::Ready).then(|| SessionId::new("lead-session"))
        );
        adoption.followed().await;
        assert!(
            adoption.only_tags(),
            "adoption never types into a working editor"
        );
    }
}

#[tokio::test]
async fn refused_bound_lead_recovers_on_its_own_session_event() {
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("lead-session", "%1", false, Harness::ClaudeCode)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    let untagged = Adoption::inspection(None, "claude", "");
    adoption
        .rig
        .client
        .script_inspect(&pane("%1"), Some(&untagged))
        .unwrap();
    adoption.script_untagged_capture("unknown\n", "claude");
    adoption.script_untagged_capture("unknown\n", "claude");
    adoption.rig.client.script_tag_check(&untagged).unwrap();
    adoption.script_follow_up_of("claude", Some(("unknown\n", "claude")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    adoption.followed().await;
    // The first event binds identity without claiming to have recognized a screen.
    adoption.script_follow_up_of("claude", None);
    update(&adoption.rig, "lead-session").await;
    progress(&mut adoption.events, "finished", Some("q/lead")).await;
    assert_eq!(adoption.agent().await.unwrap().status, AgentStatus::Ready);
    assert_eq!(
        adoption.agents.refusal_of(&adoptee(), "%1").as_deref(),
        Some(NOT_CONFIRMED)
    );
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );

    adoption.script_follow_up_of("claude", Some((LEAD_IDLE, "claude")));
    update(&adoption.rig, "lead-session").await;
    adoption.followed().await;
    let recipient = adoption.agents.recipient(&adoptee()).await.unwrap();
    assert_eq!(recipient.session_id, Some(SessionId::new("lead-session")));
    assert_eq!(
        recipient.baseline,
        PaneBaseline::adopted(100)
            .with_harness("claude".into())
            .unwrap()
    );
    assert!(adoption.only_tags());
}

#[tokio::test]
async fn lead_chrome_never_pins_a_shell_holding_its_child() {
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("lead-session", "%1", false, Harness::ClaudeCode)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(shell_holding_child());
    let untagged = Adoption::inspection(None, "claude", "");
    adoption
        .rig
        .client
        .script_inspect(&pane("%1"), Some(&untagged))
        .unwrap();
    adoption.script_untagged_capture(LEAD_IDLE, "claude");
    adoption.script_untagged_capture(LEAD_IDLE, "claude");
    adoption.rig.client.script_tag_check(&untagged).unwrap();
    adoption.script_follow_up_of("claude", Some((LEAD_IDLE, "claude")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    adoption.followed().await;
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    assert!(adoption.only_tags());
}

#[test]
fn recognized_lead_chrome_cannot_bypass_input_identity_ownership_or_focus() {
    use crate::org::agent_flow::{input_refusal, safe_editor, safe_to_paste};
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    let capture = PaneCapture::new(
        LEAD_IDLE.into(),
        Adoption::inspection(Some("q/lead"), "claude", LEAD_IDLE),
        CaptureFormat::Ansi,
    )
    .with_focus(Some(false));
    assert!(input_refusal(screen, &capture, &baseline, "q/lead").is_none());
    assert!(safe_editor(&screen.control(&capture, &baseline)));
    for case in 0..7 {
        let mut refused = capture.clone();
        match case {
            0 => refused.inspection.pane.pane_pid = 101,
            1 => refused.inspection.foreground_command = Some("other".into()),
            2 => refused.inspection.local_agent_tag = Some("q/other".into()),
            3 => refused.inspection.dead = true,
            4 => refused.inspection.in_mode = true,
            5 => refused.focused = Some(true),
            _ => refused.focused = None,
        }
        assert!(
            input_refusal(screen, &refused, &baseline, "q/lead").is_some(),
            "case {case}"
        );
        assert!(
            !safe_to_paste(screen, &refused, &baseline, "q/lead"),
            "case {case}"
        );
    }
    for unsafe_text in [
        LEAD_WORKING.to_owned(),
        LEAD_IDLE.replace("❯\u{a0}", "❯\u{a0}draft"),
        "unknown dialog\n".into(),
    ] {
        let unsafe_frame = PaneCapture {
            text: unsafe_text,
            ..capture.clone()
        };
        assert!(!safe_editor(&screen.control(&unsafe_frame, &baseline)));
        assert!(!safe_to_paste(screen, &unsafe_frame, &baseline, "q/lead"));
    }
}

// Captured: Claude Code 2.1.325, 2026-11-17, read-only tmux capture-pane -p -e -N
// of an idle seat (supplied by the lead), redacted.
const UPDATE_NOTICE_CAPTURES: [&str; 2] = [
    include_str!("../../../atm-claude-adapter/tests/fixtures/screen_styled_clear-update-installed_2.1.325.txt"),
    include_str!("../../../atm-claude-adapter/tests/fixtures/screen_styled_update-installed_2.1.325.txt"),
];

fn update_notice_editor(capture: &str) -> String {
    // Synthetic: append the already-captured mode footer missing from the short viewport.
    format!("{}⏵⏵ auto mode on (shift+tab to cycle)\n", body(capture))
}

#[tokio::test]
async fn update_installed_notice_pins_idle_adoption_but_not_footerless_views() {
    for captured in UPDATE_NOTICE_CAPTURES {
        let editor = update_notice_editor(captured);
        for (text, status) in [
            (editor.as_str(), AgentStatus::Ready),
            (body(captured), AgentStatus::Starting),
        ] {
            let mut adoption = Adoption::new().await;
            adoption
                .rig
                .row_of("update-session", "%1", false, Harness::ClaudeCode)
                .await;
            *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
            let untagged = Adoption::inspection(None, "claude", text);
            adoption
                .rig
                .client
                .script_inspect(&pane("%1"), Some(&untagged))
                .unwrap();
            adoption.script_untagged_capture(text, "claude");
            adoption.script_untagged_capture(text, "claude");
            adoption.rig.client.script_tag_check(&untagged).unwrap();
            adoption.script_follow_up_of(
                "claude",
                (status == AgentStatus::Starting).then_some((text, "claude")),
            );
            assert_eq!(adoption.adopt().await.unwrap().state, status);
            assert_eq!(
                adoption.agents.baseline_of(&adoptee(), "%1"),
                if status == AgentStatus::Ready {
                    PaneBaseline::adopted(100)
                        .with_harness("claude".into())
                        .unwrap()
                } else {
                    PaneBaseline::unproven()
                }
            );
            assert_eq!(
                adoption.agent().await.unwrap().session_id,
                (status == AgentStatus::Ready).then(|| SessionId::new("update-session"))
            );
            adoption.followed().await;
            assert!(
                adoption.only_tags(),
                "adoption must not type into the editor"
            );
        }
    }
}

#[test]
fn busy_footer_never_authorizes_paste_on_idle_or_fresh_transcript() {
    use crate::org::agent_flow::{safe_editor, safe_to_paste};
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    // Synthetic: add only the real busy segment to the captured completed/fresh
    // frames; correct ownership and focus must not make contradictory chrome safe.
    // Captured: Claude Code 2.1.326, 2026-11-17, sealed fake-API TUI.
    for source in [
        include_str!("../../../atm-claude-adapter/tests/fixtures/screen_cc326-native-finished_2.1.326.styled.txt"),
        include_str!("../../../atm-claude-adapter/tests/fixtures/screen_cc326-drawn-finished_2.1.326.styled.txt"),
        include_str!("../../../atm-claude-adapter/tests/fixtures/screen_cc326-native-fresh_2.1.326.styled.txt"),
        include_str!("../../../atm-claude-adapter/tests/fixtures/screen_cc326-drawn-fresh_2.1.326.styled.txt"),
    ] {
        let original = body(source);
        let capture_of = |text: &str| {
            PaneCapture::new(
                text.into(),
                Adoption::inspection(Some("q/lead"), "claude", text),
                CaptureFormat::Ansi,
            )
            .with_focus(Some(false))
        };
        let ready = capture_of(original);
        assert!(safe_editor(&screen.control(&ready, &baseline)));
        assert!(safe_to_paste(screen, &ready, &baseline, "q/lead"));
        for agents in [true, false] {
            let busy = original.replace(" · ← for agents", " · esc to interrupt · ← for agents");
            assert_ne!(busy, original);
            let text = if agents {
                busy
            } else {
                busy.replace(" · ← for agents", "")
            };
            let capture = capture_of(&text);
            assert_eq!(
                (
                    safe_editor(&screen.control(&capture, &baseline)),
                    safe_to_paste(screen, &capture, &baseline, "q/lead"),
                ),
                (false, false),
                "busy footer with agents={agents} must refuse input"
            );
        }
    }
}

#[test]
fn update_installed_notice_never_bypasses_input_guards() {
    use crate::org::agent_flow::safe_to_paste;
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    for captured in UPDATE_NOTICE_CAPTURES {
        let text = update_notice_editor(captured);
        let capture = PaneCapture::new(
            text.clone(),
            Adoption::inspection(Some("q/lead"), "claude", &text),
            CaptureFormat::Ansi,
        )
        .with_focus(Some(false));
        assert!(safe_to_paste(screen, &capture, &baseline, "q/lead"));
        for case in 0..7 {
            let mut changed = capture.clone();
            match case {
                0 => changed.inspection.pane.pane_pid = 101,
                1 => changed.inspection.foreground_command = Some("other".into()),
                2 => changed.inspection.local_agent_tag = Some("q/other".into()),
                3 => changed.inspection.dead = true,
                4 => changed.inspection.in_mode = true,
                5 => changed.focused = Some(true),
                _ => changed.focused = None,
            }
            assert!(
                !safe_to_paste(screen, &changed, &baseline, "q/lead"),
                "case {case}"
            );
        }
    }
}
