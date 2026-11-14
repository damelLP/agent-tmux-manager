#![allow(clippy::unwrap_used, clippy::expect_used)] // synthetic serial history
mod support;
use atm_org::{delivery::*, *};
use chrono::Duration;
use support::*;

#[test]
fn editor_ready_delivery_uses_missing_row_path() {
    let mut state = OrgState::default();
    let mut seq = 0;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: id(),
            pane: "%1".into(),
            session_id: Some("old".into()),
        },
        OrgCommand::Clear { agent_id: id() },
        OrgCommand::EditorReady { agent_id: id() },
        OrgCommand::SendMessage(content(Address::Agent(id()))),
    ] {
        for event in state.decide(&command, time()).unwrap() {
            seq += 1;
            state.apply(seq, time(), &event).unwrap();
        }
    }
    let agent = state.agent(&id()).unwrap();
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, Some("old".into()));
    let mut live = RecipientLiveView {
        session: None,
        pane: PaneLiveness::Alive,
        agent_status: agent.status,
        other_delivery_in_flight: false,
        last_paste_at: None,
        hold_since: None,
        last_attempt_before_start: false,
        copy_mode: Some(false),
        dialog_open: Some(false),
        owner_focused: false,
        input_line: InputLine::EmptySince(time()),
        send_now: false,
        confirmation: None,
        probably_landed: false,
    };
    let message = state.message(&MessageId::new(seq)).unwrap().clone();
    let policy = DeliveryPolicy::default();
    assert_eq!(
        next_delivery(&message, &live, time() + Duration::seconds(4), &policy).unwrap(),
        DeliveryStep::Wait {
            until: time() + Duration::seconds(5),
            reason: WaitReason::InputStabilizing
        }
    );
    assert_eq!(
        next_delivery(&message, &live, time() + Duration::seconds(5), &policy).unwrap(),
        DeliveryStep::PasteNow { attempt: 1 }
    );
    live.input_line = InputLine::Unknown;
    assert!(matches!(
        next_delivery(&message, &live, time() + Duration::seconds(5), &policy).unwrap(),
        DeliveryStep::Wait {
            reason: WaitReason::InputUnknown,
            ..
        }
    ));
    live.input_line = InputLine::EmptySince(time());
    for event in state
        .decide(&OrgCommand::Clear { agent_id: id() }, time())
        .unwrap()
    {
        seq += 1;
        state.apply(seq, time(), &event).unwrap();
    }
    live.agent_status = state.agent(&id()).unwrap().status;
    assert!(matches!(
        next_delivery(&message, &live, time() + Duration::seconds(5), &policy).unwrap(),
        DeliveryStep::Wait {
            reason: WaitReason::AgentNotReady,
            ..
        }
    ));
}
