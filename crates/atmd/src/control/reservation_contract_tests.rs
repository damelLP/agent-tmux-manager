//! Checkpoint tests: replace stub assertions as each operation is implemented.
//! Breaks caught: unsafe pane targets, successful/no-op placeholders, callbacks
//! admitted before the reservation implementation, and changes to old control.

use super::*;
use crate::control::service::{spawn_control, spawn_control_with_org};
use atm_tmux::MockTmuxClient;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn agent() -> AgentId {
    "project/lead".parse().unwrap()
}

fn not_implemented<T>(result: Result<T, ReservationError>, operation: &str) {
    assert!(
        matches!(result, Err(ReservationError::NotImplemented { operation: actual }) if actual == operation)
    );
}

#[test]
fn contract_pane_id_accepts_only_exact_ascii_targets() {
    for value in ["%0", "%42", "%1234567890"] {
        let pane = PaneId::parse(value).unwrap();
        assert_eq!(pane.as_str(), value);
        assert_eq!(pane.to_string(), value);
    }
    for value in [
        "", "%", "42", " %1", "%1\n", "%1junk", "%١", "%1;", "-t", "%+1", "%00", "%01",
    ] {
        assert!(
            matches!(
                PaneId::parse(value),
                Err(ReservationError::InvalidRequest { .. })
            ),
            "{value:?}"
        );
    }
}

#[tokio::test]
async fn contract_operations_return_typed_not_implemented() {
    // No public constructor: only the real supervisor will mint capabilities.
    let reservation = Reservation { agent: agent() };
    assert_eq!(reservation.agent(), &agent());
    let pane = PaneId::parse("%7").unwrap();
    not_implemented(reservation.inspect(&pane).await, "inspect");
    for expected in [ExpectedTag::Absent, ExpectedTag::Agent(agent())] {
        not_implemented(reservation.tag(&pane, expected).await, "tag");
    }
    not_implemented(reservation.untag(&pane, agent()).await, "untag");
    for key in [
        PaneKey::Enter,
        PaneKey::Escape,
        PaneKey::Up,
        PaneKey::Down,
        PaneKey::CtrlC,
    ] {
        not_implemented(reservation.keys(&pane, agent(), key).await, "keys");
    }
    not_implemented(
        reservation
            .paste(&pane, agent(), "literal;\nEnter".into())
            .await,
        "paste",
    );
    for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
        not_implemented(reservation.capture(&pane, format).await, "capture");
    }
    not_implemented(
        reservation
            .split(
                SpawnContext::new("%0".into(), None, false),
                PaneDirection::Below,
                "50%".into(),
            )
            .await,
        "split",
    );
    not_implemented(
        reservation
            .kill(
                &pane,
                agent(),
                serde_json::json!({"reason":"down"}),
                DateTime::from_timestamp(0, 0).unwrap(),
            )
            .await,
        "kill",
    );
}

#[tokio::test]
async fn contract_job_is_not_admitted_and_never_invokes_callbacks() {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        cancel.clone(),
    );
    for kind in [
        ReservationKind::Up,
        ReservationKind::Adopt,
        ReservationKind::Clear,
        ReservationKind::Respawn,
        ReservationKind::Down,
        ReservationKind::Delivery,
        ReservationKind::Observe,
    ] {
        for wait in [WaitPolicy::Wait, WaitPolicy::Refuse] {
            let called = Arc::new(AtomicBool::new(false));
            let work_called = called.clone();
            let cleanup_called = called.clone();
            let result: Result<(), ReservationError> = control
                .run_reserved(
                    ReservationRequest {
                        agent: agent(),
                        pane: Some(PaneId::parse("%7").unwrap()),
                        kind,
                        wait,
                        deadline: Instant::now() + Duration::from_secs(30),
                    },
                    move |reservation| async move {
                        work_called.store(true, Ordering::SeqCst);
                        let _ = reservation.agent();
                        Ok(())
                    },
                    move |_, completion| async move {
                        cleanup_called.store(true, Ordering::SeqCst);
                        match completion {
                            Completion::Succeeded
                            | Completion::Failed
                            | Completion::TimedOut
                            | Completion::WorkerStopped => Ok(()),
                        }
                    },
                )
                .await;
            not_implemented(result, "run_reserved");
            assert!(!called.load(Ordering::SeqCst));
        }
    }
    assert_eq!(
        mock.call_count(),
        0,
        "unimplemented jobs must have no effects"
    );
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn contract_org_constructor_preserves_existing_control_calls() {
    let mock = MockTmuxClient::new();
    mock.set_next_raw_output("0 1");
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control_with_org(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        crate::org::OrgHandle::disabled("contract checkpoint"),
        cancel.clone(),
    );
    assert_eq!(
        control.send("%7".into(), "hello;".into()).await.unwrap(),
        "%7"
    );
    assert!(
        matches!(mock.calls().last(), Some(atm_tmux::mock::MockCall::SendKeys { pane, keys }) if pane == "%7" && keys == "Enter")
    );
    cancel.cancel();
    task.await.unwrap();
}

#[test]
fn contract_transport_observations_reuse_pane_info() {
    let capture = PaneCapture {
        text: "draft  \n\n".into(),
        inspection: PaneInspection {
            pane: atm_tmux::PaneInfo {
                pane_id: "%7".into(),
                session_name: "private".into(),
                window_index: 0,
                pane_pid: 123,
                width: 80,
                height: 24,
                is_active: true,
            },
            local_agent_tag: Some(String::new()),
            dead: false,
            in_mode: false,
            bracketed_paste: None,
            cursor_column: 7,
            cursor_row: 0,
        },
        format: CaptureFormat::Plain,
    };
    // Consumer-facing construction and ownership: no duplicate cursor/core type.
    let cloned = capture.clone();
    assert_eq!(cloned.text, "draft  \n\n");
    assert_eq!(cloned.inspection.local_agent_tag.as_deref(), Some(""));
    assert_eq!(cloned.inspection.bracketed_paste, None);
}
