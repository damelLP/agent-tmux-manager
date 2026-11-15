//! `session.kill` and `session.interrupt` through the real control actor over
//! a mock tmux. Reservations are held on channels the test owns; the tests on
//! a paused clock use no organisation I/O.
use super::pane_request::{admit, Operation};
use super::*;
use crate::control::reservation::{state::InputGate, testing::ScriptedTmuxClient, *};
use crate::server::methods::{CallerId, CallerPane};
use atm_core::{SessionDomain, SessionId};
use atm_org::AgentId;
use atm_tmux::{mock::MockCall, MockTmuxClient, TmuxError};
use std::{error::Error, future::Future, task::Poll};

const CLEARING: &str = "Refusing to interrupt %1: agent p/a is clearing; try again in a moment";
const QUARANTINED: &str = "pane %1: agent p/a is quarantined; restart atmd and reconcile";

fn pane() -> PaneId {
    PaneId::parse("%1").unwrap()
}
fn agent() -> AgentId {
    "p/a".parse().unwrap()
}
fn request(kind: ReservationKind) -> ReservationRequest {
    ReservationRequest::new(
        agent(),
        Some(pane()),
        kind,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}
async fn noop(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}
/// Poll once, so the request is in the actor's queue, then run it as a task.
async fn queued<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> JoinHandle<T> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("expected queued request"),
    })
    .await;
    tokio::spawn(future)
}
/// A reservation of `kind` on `%1` whose work has started and waits for the
/// returned sender. `after` runs inside the work once it is released.
async fn hold<F, Fut>(
    control: &ControlHandle,
    kind: ReservationKind,
    after: F,
) -> (
    JoinHandle<Result<(), ReservationError>>,
    oneshot::Sender<()>,
)
where
    F: FnOnce(Reservation) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), ReservationError>> + Send + 'static,
{
    let c = control.clone();
    let (entered, entry) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let job = queued(async move {
        c.run_reserved(
            request(kind),
            move |r| async move {
                entered.send(()).unwrap();
                released.await.unwrap();
                after(r).await
            },
            noop,
        )
        .await
    })
    .await;
    entry.await.unwrap();
    (job, release)
}
fn start(client: Box<dyn TmuxClient>) -> (ControlHandle, JoinHandle<()>, CancellationToken) {
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        client,
        crate::registry::spawn_registry(),
        OrgHandle::disabled("pane request test"),
        cancel.clone(),
    );
    (control, actor, cancel)
}
async fn logged() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    (dir, org, cancel)
}
async fn events(org: &OrgHandle) -> Vec<(String, serde_json::Value)> {
    org.tail(crate::org::TailParams::default())
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.kind, event.body))
        .collect()
}
fn effects(mock: &MockTmuxClient) -> Vec<MockCall> {
    mock.calls()
        .into_iter()
        .filter(|call| !matches!(call, MockCall::RawCommand { .. }))
        .collect()
}
fn is_kill(call: &MockCall, expected: &str) -> bool {
    matches!(call, MockCall::KillPane { pane } if pane == expected)
}
fn is_ctrl_c(call: &MockCall, expected: &str) -> bool {
    matches!(call, MockCall::SendKeys { pane, keys } if pane == expected && keys == "C-c")
}

#[test]
fn only_an_interrupt_is_ever_refused_and_nothing_waits() {
    let gates = || {
        [
            InputGate::Free,
            InputGate::Held,
            InputGate::Clearing(agent()),
            InputGate::Quarantined(agent()),
            InputGate::Unavailable,
        ]
    };
    for gate in gates() {
        admit(Operation::Kill, &pane(), gate).unwrap();
    }
    let answers: Vec<_> = gates()
        .into_iter()
        .map(|gate| admit(Operation::Interrupt, &pane(), gate).map_err(|error| error.to_string()))
        .collect();
    assert_eq!(
        answers,
        [
            Ok(()),
            Ok(()),
            Err(CLEARING.to_owned()),
            Err(QUARANTINED.to_owned()),
            Err("control service is unavailable".to_owned()),
        ]
    );
    assert!(matches!(
        admit(Operation::Interrupt, &pane(), InputGate::Unavailable),
        Err(ServiceError::Unavailable)
    ));
}

#[tokio::test]
async fn targets_resolve_in_the_registry_and_keep_their_refusals() {
    let mock = MockTmuxClient::new();
    let registry = crate::registry::spawn_registry();
    for (id, pane) in [
        ("abc-session-1", Some("%5")),
        ("dup-session-1", Some("%6")),
        ("dup-session-2", Some("%7")),
        ("bare-session", None),
    ] {
        registry
            .register(SessionDomain {
                id: SessionId::new(id),
                tmux_pane: pane.map(Into::into),
                ..SessionDomain::default()
            })
            .await
            .unwrap();
    }
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry,
        OrgHandle::disabled("resolution test"),
        cancel.clone(),
    );
    assert_eq!(control.kill("abc".into()).await.unwrap(), "%5");
    assert_eq!(control.interrupt("abc".into()).await.unwrap(), "%5");
    assert_eq!(control.kill("%9".into()).await.unwrap(), "%9");
    let calls = mock.calls();
    assert!(
        matches!(calls.as_slice(), [kill, key, literal]
            if is_kill(kill, "%5") && is_ctrl_c(key, "%5") && is_kill(literal, "%9")),
        "{calls:?}"
    );
    mock.clear_calls();
    for (target, refusal) in [
        ("nope", "No session matching 'nope'".to_owned()),
        ("bare", "Session bare-ses has no tmux pane".to_owned()),
        (
            "dup",
            "Ambiguous target 'dup' matches 2 sessions: ".to_owned(),
        ),
    ] {
        for result in [
            control.kill(target.into()).await,
            control.interrupt(target.into()).await,
        ] {
            let error = result.unwrap_err().to_string();
            assert!(error.starts_with(&refusal), "{target}: {error}");
            assert!(error == refusal || target == "dup", "{target}: {error}");
        }
    }
    assert_eq!(mock.call_count(), 0, "a refused target reached tmux");
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn each_action_is_audited_with_its_caller_and_outcome() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let caller = control.clone().with_caller(CallContext {
        client_id: CallerId("cli".into()),
        from_pane: Some(CallerPane("%9".into())),
    });
    assert_eq!(caller.interrupt("%1".into()).await.unwrap(), "%1");
    assert_eq!(caller.kill("%1".into()).await.unwrap(), "%1");
    mock.set_next_error(TmuxError::NotFound);
    let error = control.kill("%2".into()).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to kill pane %2");
    assert_eq!(
        error.source().unwrap().to_string(),
        "tmux not found in PATH"
    );
    mock.set_next_error(TmuxError::NotFound);
    let error = control.interrupt("%2".into()).await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to interrupt pane %2");
    // No session matched: no pane, so no action to record.
    control.kill("nope".into()).await.unwrap_err();
    control.flush_audits().await;
    let action = |method: &str, outcome: &str, error: Option<&str>, attributed: bool| {
        serde_json::json!({
            "method": method,
            "outcome": outcome,
            "error": error,
            "client_id": attributed.then_some("cli"),
            "from_pane": attributed.then_some("%9"),
        })
    };
    let recorded: Vec<_> = events(&org)
        .await
        .into_iter()
        .map(|(kind, body)| (kind, body["pane"].clone(), body["action"].clone()))
        .collect();
    assert_eq!(
        recorded,
        [
            (
                "control.sent".to_owned(),
                "%1".into(),
                action("session.interrupt", "ok", None, true)
            ),
            (
                "control.killed".to_owned(),
                "%1".into(),
                action("session.kill", "ok", None, true)
            ),
            (
                "control.killed".to_owned(),
                "%2".into(),
                action(
                    "session.kill",
                    "error",
                    Some("Failed to kill pane %2"),
                    false
                )
            ),
            (
                "control.sent".to_owned(),
                "%2".into(),
                action(
                    "session.interrupt",
                    "error",
                    Some("Failed to interrupt pane %2"),
                    false
                )
            ),
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn kill_and_interrupt_never_wait_for_a_held_pane() {
    for kind in [
        ReservationKind::Delivery,
        ReservationKind::Observe,
        ReservationKind::Adopt,
        ReservationKind::Up,
        ReservationKind::Respawn,
        ReservationKind::Down,
    ] {
        let mock = MockTmuxClient::new();
        let (control, actor, cancel) = start(Box::new(mock.clone()));
        let (job, release) = hold(&control, kind, |_| async { Ok(()) }).await;
        let before = Instant::now();
        assert_eq!(control.interrupt("%1".into()).await.unwrap(), "%1");
        assert_eq!(control.kill("%1".into()).await.unwrap(), "%1");
        assert_eq!(before, Instant::now(), "{kind:?}: the request waited");
        let calls = effects(&mock);
        assert!(
            matches!(calls.as_slice(), [key, kill] if is_ctrl_c(key, "%1") && is_kill(kill, "%1")),
            "{kind:?}: {calls:?}"
        );
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        cancel.cancel();
        actor.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn interrupt_is_refused_during_a_clear_and_kill_is_not() {
    let mock = MockTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    let (job, release) = hold(&control, ReservationKind::Clear, |_| async { Ok(()) }).await;
    let before = Instant::now();
    let error = control.interrupt("%1".into()).await.unwrap_err();
    assert_eq!(error.to_string(), CLEARING);
    assert!(effects(&mock).is_empty(), "a refused interrupt sent a key");
    assert_eq!(control.kill("%1".into()).await.unwrap(), "%1");
    assert_eq!(before, Instant::now(), "the request waited");
    let calls = effects(&mock);
    assert!(
        matches!(calls.as_slice(), [kill] if is_kill(kill, "%1")),
        "{calls:?}"
    );
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    // The clear is over: the same interrupt goes through.
    assert_eq!(control.interrupt("%1".into()).await.unwrap(), "%1");
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn interrupt_is_refused_on_a_quarantined_pane_and_kill_is_not() {
    let mock = MockTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    let failed = control
        .run_reserved(
            request(ReservationKind::Down),
            |_| async { Ok(()) },
            |_, _| async { Err(ReservationError::Unavailable) },
        )
        .await;
    assert!(matches!(failed, Err(ReservationError::Unavailable)));
    let error = control.interrupt("%1".into()).await.unwrap_err();
    assert_eq!(error.to_string(), QUARANTINED);
    assert!(effects(&mock).is_empty(), "a refused interrupt sent a key");
    assert_eq!(control.kill("%1".into()).await.unwrap(), "%1");
    // The kill is no release: the quarantine stays until the daemon restarts.
    let quarantines = control
        .quarantines(Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(quarantines.len(), 1);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_delivery_whose_pane_is_killed_sees_it_gone_and_releases() {
    let client = ScriptedTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(client.clone()));
    let (job, release) = hold(&control, ReservationKind::Delivery, |r| async move {
        let sent = r.keys(&pane(), agent(), PaneKey::Enter).await;
        assert!(
            matches!(sent, Err(ReservationError::PaneGone { .. })),
            "{sent:?}"
        );
        Ok(())
    })
    .await;
    assert_eq!(control.kill("%1".into()).await.unwrap(), "%1");
    // What tmux answers about the pane from now on: it is not there.
    client.script_inspect(&pane(), None).unwrap();
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    let calls = effects(&client.mock());
    assert!(
        matches!(calls.as_slice(), [kill] if is_kill(kill, "%1")),
        "the delivery typed into a killed pane: {calls:?}"
    );
    let quarantines = control
        .quarantines(Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    assert!(quarantines.is_empty(), "{quarantines:?}");
    // Released: the next reservation of the agent and pane is granted.
    control
        .run_reserved(
            request(ReservationKind::Observe),
            |_| async { Ok(()) },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn another_tmux_server_is_refused_before_it_is_an_action() {
    let (_dir, org, org_cancel) = logged().await;
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("own");
    let other = dir.path().join("other");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&other, "").unwrap();
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let foreign = control
        .clone()
        .for_server(Some(other.to_str().unwrap().into()));
    for (verb, kill) in [("kill", true), ("interrupt", false)] {
        mock.set_next_raw_output(own.to_str().unwrap());
        let result = if kill {
            foreign.kill("%1".into()).await
        } else {
            foreign.interrupt("%1".into()).await
        };
        assert_eq!(
            result.unwrap_err().to_string(),
            format!(
                "Refusing to {verb} %1: atmd controls tmux server {}, not this one ({})",
                own.canonicalize().unwrap().display(),
                other.canonicalize().unwrap().display()
            )
        );
    }
    assert!(effects(&mock).is_empty());
    // The caller's own server is accepted.
    mock.set_next_raw_output(own.to_str().unwrap());
    let same = control
        .clone()
        .for_server(Some(own.to_str().unwrap().into()));
    assert_eq!(same.kill("%1".into()).await.unwrap(), "%1");
    control.flush_audits().await;
    let kinds: Vec<_> = events(&org).await.into_iter().map(|e| e.0).collect();
    assert_eq!(kinds, ["control.killed"]);
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test]
async fn expired_or_abandoned_requests_touch_nothing_and_log_nothing() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let send = |deadline, request| {
        let control = control.clone();
        async move {
            control
                .sender
                .send(Pending {
                    server: None,
                    deadline,
                    caller: None,
                    request,
                })
                .await
                .ok()
                .unwrap();
        }
    };
    // Expired while it queued.
    let (reply, kill) = oneshot::channel();
    let (reply_key, key) = oneshot::channel();
    let target = || "%1".to_owned();
    send(
        Instant::now(),
        Request::Kill {
            target: target(),
            reply,
        },
    )
    .await;
    send(
        Instant::now(),
        Request::Interrupt {
            target: target(),
            reply: reply_key,
        },
    )
    .await;
    assert!(matches!(kill.await.unwrap(), Err(ServiceError::Timeout)));
    assert!(matches!(key.await.unwrap(), Err(ServiceError::Timeout)));
    // Its caller is gone.
    let later = Instant::now() + ACTION_TIMEOUT;
    let (reply, gone) = oneshot::channel();
    drop(gone);
    send(
        later,
        Request::Kill {
            target: target(),
            reply,
        },
    )
    .await;
    let (reply, gone) = oneshot::channel();
    drop(gone);
    send(
        later,
        Request::Interrupt {
            target: target(),
            reply,
        },
    )
    .await;
    control.flush_audits().await;
    assert_eq!(mock.call_count(), 0);
    assert!(events(&org).await.is_empty());
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test]
async fn an_admitted_kill_finishes_and_is_logged_when_the_daemon_stops() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let (reply, response) = oneshot::channel();
    control
        .sender
        .send(Pending {
            server: None,
            deadline: Instant::now() + ACTION_TIMEOUT,
            caller: None,
            request: Request::Kill {
                target: "%1".into(),
                reply,
            },
        })
        .await
        .ok()
        .unwrap();
    cancel.cancel();
    assert_eq!(response.await.unwrap().unwrap(), "%1");
    drop(control);
    actor.await.unwrap();
    let calls = effects(&mock);
    assert!(
        matches!(calls.as_slice(), [kill] if is_kill(kill, "%1")),
        "{calls:?}"
    );
    let recorded = events(&org).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "control.killed");
    assert_eq!(recorded[0].1["action"]["outcome"], "ok");
    org_cancel.cancel();
}

/// tmux takes `%01` for `%1`. The gate must be asked about the pane tmux
/// will act on, or a non-canonical id slips an interrupt past its clear.
#[tokio::test(start_paused = true)]
async fn a_noncanonical_pane_id_meets_the_gate_of_its_canonical_pane() {
    let mock = MockTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    let (job, release) = hold(&control, ReservationKind::Clear, |_| async { Ok(()) }).await;
    mock.set_next_raw_output("%1\n");
    let error = control.interrupt("%01".into()).await.unwrap_err();
    assert_eq!(error.to_string(), CLEARING);
    assert!(effects(&mock).is_empty(), "{:?}", mock.calls());
    mock.set_next_raw_output("%1\n");
    assert_eq!(control.kill("%01".into()).await.unwrap(), "%1");
    let calls = effects(&mock);
    assert!(
        matches!(calls.as_slice(), [kill] if is_kill(kill, "%1")),
        "{calls:?}"
    );
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

/// A caller can leave after its request was taken up: while the daemon checks
/// the tmux server or reads the registry. Its kill must not happen then.
mod caller_gone_during_the_reads {
    use super::*;
    use atm_tmux::{PaneDirection, PaneInfo};
    type Call<'a, T> = std::pin::Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
    macro_rules! forward {
        ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
            fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
            where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
                Box::pin(self.mock.$method($($arg),*))
            }
        };
    }
    /// Every tmux read reports that it began, then waits for the test.
    struct Stalled {
        mock: MockTmuxClient,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl TmuxClient for Stalled {
        fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
            &'s self,
            command: &'c str,
            args: &'a [&'v str],
        ) -> Call<'async_trait, String>
        where
            's: 'async_trait,
            'c: 'async_trait,
            'a: 'async_trait,
            'v: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
                self.mock.raw_command(command, args).await
            })
        }
        forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
        forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
        forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
        forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
        forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
        forward!(kill_pane<'a>(pane: &'a str) -> ());
        forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
        forward!(list_panes<>() -> Vec<PaneInfo>);
        forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
        forward!(select_pane<'a>(pane: &'a str) -> ());
        forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
        forward!(new_session<'a>(name: &'a str) -> String);
    }

    #[tokio::test]
    async fn a_caller_gone_during_the_server_check_gets_no_kill_and_no_log_entry() {
        let (_dir, org, org_cancel) = logged().await;
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own");
        std::fs::write(&own, "").unwrap();
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output(own.to_str().unwrap());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(Stalled {
                mock: mock.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
            crate::registry::spawn_registry(),
            org.clone(),
            cancel.clone(),
        );
        let (reply, response) = oneshot::channel();
        control
            .sender
            .send(Pending {
                server: Some(own.to_str().unwrap().into()),
                deadline: Instant::now() + ACTION_TIMEOUT,
                caller: None,
                request: Request::Kill {
                    target: "%1".into(),
                    reply,
                },
            })
            .await
            .ok()
            .unwrap();
        // The request is past its first caller check and inside the server check.
        entered.notified().await;
        drop(response);
        release.add_permits(1);
        // Queued behind the kill's turn: answered once that turn is over.
        control.flush_audits().await;
        let calls = effects(&mock);
        assert!(
            calls.is_empty(),
            "a gone caller's kill reached tmux: {calls:?}"
        );
        assert!(events(&org).await.is_empty());
        cancel.cancel();
        actor.await.unwrap();
        org_cancel.cancel();
    }
}
