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
async fn contract_adoption_cannot_send_launch_input() {
    // The real supervisor mints the capability, but kind limits input.
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(MockTmuxClient::new()),
        crate::registry::spawn_registry(),
        crate::org::OrgHandle::disabled("contract test"),
        cancel.clone(),
    );
    control
        .run_reserved(
            ReservationRequest {
                agent: agent(),
                pane: Some(PaneId::parse("%7").unwrap()),
                kind: ReservationKind::Adopt,
                wait: WaitPolicy::Wait,
                deadline: Instant::now() + Duration::from_secs(30),
            },
            |reservation| async move {
                assert_eq!(reservation.agent(), &agent());
                let pane = PaneId::parse("%7").unwrap();
                assert!(reservation.inspect(&pane).await?.is_none());
                for key in [PaneKey::Enter, PaneKey::Escape, PaneKey::Up, PaneKey::Down] {
                    assert!(matches!(
                        reservation.keys(&pane, agent(), key).await,
                        Err(ReservationError::InvalidRequest { .. })
                    ));
                }
                assert!(matches!(
                    reservation
                        .paste(&pane, agent(), "literal;\nEnter".into())
                        .await,
                    Err(ReservationError::InvalidRequest { .. })
                ));
                Ok::<_, ReservationError>(())
            },
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn contract_expired_job_is_not_admitted_and_never_invokes_callbacks() {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        crate::org::OrgHandle::disabled("contract test"),
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
                        deadline: Instant::now(),
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
            assert!(matches!(result, Err(ReservationError::Timeout)));
            assert!(!called.load(Ordering::SeqCst));
        }
    }
    assert_eq!(mock.call_count(), 0, "expired jobs must have no effects");
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn contract_org_constructor_preserves_existing_control_calls() {
    let mock = MockTmuxClient::new();
    mock.set_next_raw_output("ATM-PANE-v1\t%7\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
    mock.set_next_raw_output(""); // positively present, pane-local tag unset
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
            synchronized: false,
            input_off: false,
            foreground_pid: None,
            foreground_command: None,
        },
        format: CaptureFormat::Plain,
        focused: None,
    };
    // Consumer-facing construction and ownership: no duplicate cursor/core type.
    let cloned = capture.clone();
    assert_eq!(cloned.text, "draft  \n\n");
    assert_eq!(cloned.inspection.local_agent_tag.as_deref(), Some(""));
    assert_eq!(cloned.inspection.bracketed_paste, None);
}
