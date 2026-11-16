use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, ControlPolicy, ControlScreen, HookListEntry,
    ScreenFacts, ScreenTextFormat, SessionArrival,
};

#[test]
fn default_is_unknown_not_fresh() {
    let screen = ControlScreen::default();
    assert_eq!(screen.facts, ScreenFacts::default());
    assert_eq!(screen.dialog, ControlDialog::Unknown);
    assert!(!screen.conversation);
}

#[test]
fn constructors_preserve_policy_and_rows() {
    const POLICY: ControlPolicy = ControlPolicy::new(
        ScreenTextFormat::Styled,
        SessionArrival::AtStart,
        "/example",
        ClearProof::ChangedSession {
            start_source: Some("example"),
        },
        1,
    );
    assert_eq!(POLICY.capture, ScreenTextFormat::Styled);
    assert_eq!(POLICY.session_arrival, SessionArrival::AtStart);
    assert_eq!(POLICY.clear_command, "/example");
    assert_eq!(
        POLICY.clear_proof,
        ClearProof::ChangedSession {
            start_source: Some("example")
        }
    );
    assert_eq!(POLICY.trust_selection_retries, 1);
    for review in [None, Some(1)] {
        let entry = HookListEntry::new("example".into(), 1, 0, review);
        assert_eq!(
            (
                entry.event.as_str(),
                entry.installed,
                entry.active,
                entry.review
            ),
            ("example", 1, 0, review)
        );
    }
    let mut screen = ControlScreen::default();
    screen.dialog = ControlDialog::LaunchSkip {
        key: ControlKey::Escape,
    };
    screen.conversation = true;
    assert!(screen.conversation);
    assert_eq!(
        screen.dialog,
        ControlDialog::LaunchSkip {
            key: ControlKey::Escape
        }
    );
}
