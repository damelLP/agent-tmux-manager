use super::*;
use atm_core::{AgentType, Model, SessionDomain};
use chrono::TimeZone;
use std::path::PathBuf;

fn now() -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).unwrap()
}
fn view(id: &str, cost: f64, prompt: Option<&str>) -> SessionView {
    let mut v = SessionView::from_domain_at(
        &SessionDomain::new(
            SessionId::new(id),
            AgentType::GeneralPurpose,
            Model::Unknown,
        ),
        now(),
    );
    v.started_at = now().to_rfc3339();
    v.harness = "pi".into();
    v.cost_usd = cost;
    v.first_prompt = prompt.map(str::to_owned);
    v.tmux_pane = Some("%10".into());
    v
}

#[test]
fn threshold_is_five_cents_since_last_summary_not_last_update() {
    let first = from_view(&view("s", 0.0, None)).unwrap();
    let mut entry = Entry::new(first.clone(), None);
    assert!(entry.due(false));
    entry.saved = Some(first);
    entry.update(from_view(&view("s", 0.03, None)).unwrap());
    assert!(!entry.due(false));
    entry.update(from_view(&view("s", 0.049999, None)).unwrap());
    assert!(!entry.due(false));
    assert!(
        entry.due(true),
        "clean shutdown must persist exact dirty cost"
    );
    entry.update(from_view(&view("s", 0.05, None)).unwrap());
    assert!(entry.due(false));
}

#[test]
fn first_prompt_model_pane_and_end_bypass_cost_threshold() {
    for change in 0..4 {
        let first = from_view(&view("s", 0.0, None)).unwrap();
        let mut entry = Entry::new(first.clone(), Some(&first));
        let mut next = first.clone();
        match change {
            0 => next.first_prompt = Some("first".into()),
            1 => next.model = Some("new model".into()),
            2 => next.pane = Some("%11".into()),
            _ => next.ended = Some(now()),
        }
        entry.update(next);
        assert!(entry.due(false));
    }
}

#[test]
fn restart_defaults_preserve_facts_then_live_cost_can_decrease_to_zero() {
    let mut recovered = from_view(&view("s", 1.25, Some("original"))).unwrap();
    recovered.model = Some("fixture".into());
    let mut live = from_view(&view("s", 0.0, None)).unwrap();
    live.started = now() + chrono::Duration::seconds(60);
    live.pane = None;
    let mut entry = Entry::new(live, Some(&recovered));
    assert_eq!(entry.latest, recovered);
    assert!(!entry.due(true));
    entry.update(from_view(&view("s", 1.26, Some("later prompt"))).unwrap());
    assert_eq!(entry.latest.first_prompt.as_deref(), Some("original"));
    assert_eq!(entry.latest.started, now());
    assert!(!entry.due(false));
    entry.update(from_view(&view("s", 0.0, None)).unwrap());
    assert_eq!(entry.latest.cost, Some(Money::from_microdollars(0)));
    assert!(entry.due(false));
}

#[test]
fn pending_ids_are_never_summaries_and_bad_time_is_not_invented() {
    assert!(from_view(&view("pending-123", 9.0, None)).is_none());
    let mut invalid = view("s", 0.0, None);
    invalid.started_at = "not a timestamp".into();
    assert!(from_view(&invalid).is_none());
}

// Drive the observer at its actual wakeups, so a queue refusal is processed
// while the actor remains stalled, without a delay or scheduler-speed assumption.
#[tokio::test(start_paused = true)]
async fn full_actor_queue_keeps_dirty_summary_for_retry() {
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    };
    use tokio::sync::{mpsc, watch, Semaphore};
    struct Signal(Semaphore);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.0.add_permits(1);
        }
    }
    let signal = Arc::new(Signal(Semaphore::new(0)));
    let waker = Waker::from(signal.clone());
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(0));
    let (committed, _) = broadcast::channel(1024);
    let (entered, entry) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let cancel = CancellationToken::new();
    let mut seq = 0;
    let actor = tokio::spawn(super::super::actor::run(
        OrgState::default(),
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |_| {
            seq += 1;
            let seq = seq;
            let entered = entered.take();
            let gate = gate.clone();
            async move {
                if let Some(entered) = entered {
                    entered.send(()).unwrap();
                    gate.acquire().await.unwrap().forget();
                }
                Ok(seq)
            }
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    };
    let registry = crate::registry::spawn_registry();
    let (events, receive) = broadcast::channel(8);
    let stop = CancellationToken::new();
    let mut observer = Box::pin(run(registry.clone(), org.clone(), receive, stop.clone()));
    assert!(observer
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    signal.0.acquire().await.unwrap().forget(); // startup snapshot reply
    assert!(observer
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let caller = org.clone();
    let stalled = tokio::spawn(async move {
        caller
            .execute(
                OrgCommand::ControlSent(atm_org::ControlAudit {
                    pane: "%1".into(),
                    action: serde_json::json!({}),
                }),
                now(),
            )
            .await
    });
    entry.await.unwrap();
    // Keep every queue slot occupied until after the observer handles refusal.
    let permits: Vec<_> = org.requests.reserve_many(64).await.unwrap().collect();
    assert_eq!(org.requests.capacity(), 0);
    let live = view("retry", 0.01, Some("original"));
    events
        .send(SessionEvent::Updated {
            session: Box::new(live),
        })
        .unwrap();
    while let Ok(permit) = signal.0.try_acquire() {
        permit.forget();
    }
    assert!(observer
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    signal.0.acquire().await.unwrap().forget(); // rejected execute task completed
    assert!(
        matches!(
            observer.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ),
        "one full-queue refusal stopped the summary observer"
    );
    drop(permits);
    release.add_permits(1);
    stalled.await.unwrap().unwrap();
    // Flush is the next attempt; no time is spent waiting for the retry clock.
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(60), observer)
        .await
        .unwrap();
    let state = org.snapshot().await.unwrap();
    let summary = state.session_summary(&SessionId::new("retry")).unwrap();
    assert_eq!(summary.first_prompt.as_deref(), Some("original"));
    assert_eq!(summary.cost, Some(Money::from_microdollars(10_000)));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn observer_lag_resync_and_shutdown_flush_exact_cost_without_disabling_org() {
    let dir = tempfile::tempdir().unwrap();
    let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
    let registry = crate::registry::spawn_registry();
    registry
        .register_discovered(
            SessionId::new("live"),
            std::process::id(),
            PathBuf::from("/fixture"),
            Some("%10".into()),
            Harness::Pi,
        )
        .await
        .unwrap();
    let (tx, rx) = broadcast::channel(1);
    // Force lag before the observer starts, then require a real registry snapshot.
    for _ in 0..3 {
        tx.send(SessionEvent::Registered {
            session_id: SessionId::new("lost"),
            agent_type: AgentType::GeneralPurpose,
        })
        .unwrap();
    }
    let stop = CancellationToken::new();
    let task = tokio::spawn(run(registry.clone(), org.clone(), rx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if org
                .snapshot()
                .await
                .unwrap()
                .session_summary(&SessionId::new("live"))
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(org.status().is_ok());
    // Publish a below-threshold update through the real registry.
    let mut domain = SessionDomain::new(
        SessionId::new("precise"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    domain.cost = Money::from_usd(0.01065);
    domain.first_prompt = Some("Read note.txt and reply.".into());
    registry.register(domain).await.unwrap();
    stop.cancel();
    task.await.unwrap();
    let state = org.snapshot().await.unwrap();
    let summary = state.session_summary(&SessionId::new("precise")).unwrap();
    assert_eq!(summary.cost, Some(Money::from_microdollars(10650)));
    assert_eq!(
        summary.first_prompt.as_deref(),
        Some("Read note.txt and reply.")
    );
    assert!(
        summary.ended.is_none(),
        "daemon shutdown is not session end"
    );
}
