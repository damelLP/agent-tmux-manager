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
