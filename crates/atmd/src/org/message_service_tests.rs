//! The message service over an in-memory organisation actor: no log files, no
//! tmux, no harness. Synchronization is by returned calls, the append gate and
//! the fold cursor. Time tests use a paused clock and only in-memory I/O;
//! discovery and cross-agent inbox checks use the ordinary clock for real
//! registry/procfs/reporting-role reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    message_evidence::MessageEvidence, message_history::MessageHistoryHandle,
    message_scheduler::Effects, messages::*, OrgHandle, OrgServiceError, Request,
};
use crate::{
    eventlog::{Envelope, EventLogError},
    server::methods::{CallContext, CallerId, CallerPane, MethodError},
};
use atm_org::{OrgCommand, OrgEvent, OrgState};
use chrono::{DateTime, Utc};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{broadcast, mpsc, watch, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) fn utc() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

pub(super) fn owner() -> CallContext {
    CallContext {
        client_id: CallerId("test".into()),
        from_pane: None,
    }
}

pub(super) fn pane(pane: &str) -> CallContext {
    CallContext {
        client_id: CallerId("test".into()),
        from_pane: Some(CallerPane(pane.into())),
    }
}

pub(super) fn worker() -> AgentId {
    "demo/worker".parse().unwrap()
}

pub(super) fn other() -> AgentId {
    "demo/other".parse().unwrap()
}

pub(super) fn note(to: Address, body: &str) -> SendParams {
    SendParams {
        to,
        kind: MessageKind::Note,
        body: body.into(),
        attachments: vec![],
        task: None,
    }
}

pub(super) fn page() -> InboxParams {
    InboxParams {
        recipient: None,
        unread_only: false,
        count_only: false,
        id: None,
        after: None,
        limit: 100,
    }
}

pub(super) fn code(error: MessageError) -> String {
    MethodError::from(error).0.code
}

pub(super) fn ids(result: &InboxResult) -> Vec<u64> {
    result.messages.iter().map(|m| m.message_id.seq()).collect()
}

/// A hang guard, not a wait: on the paused clock it only elapses once every
/// task is idle, so a step that can never complete fails at once.
pub(super) async fn guarded<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(60), work)
        .await
        .expect("hang guard: the awaited step never completed")
}

/// Poll the actual call before handing its remaining work to a task.
/// Callers check their specific admission state after this poll, not a yield.
pub(super) fn spawn_polled<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut work = Box::pin(work);
    let first = std::future::Future::poll(
        work.as_mut(),
        &mut std::task::Context::from_waker(std::task::Waker::noop()),
    );
    // A spawned task always polls once, so a wake between this probe and
    // spawning cannot be lost; that poll installs the task's own waker.
    tokio::spawn(async move {
        match first {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => work.await,
        }
    })
}

/// Probe the real held future once, without advancing the clock.
pub(super) async fn poll_pending<F: std::future::Future>(mut work: std::pin::Pin<&mut F>) {
    let polled = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(work.as_mut(), cx))
    })
    .await;
    assert!(polled.is_pending(), "held work completed during the probe");
}

/// Observe the actual raw-call poll while preserving the client's read,
/// effect and gate semantics. `true` means Pending, `false` means completed.
pub(super) struct ObservedTmux<C> {
    pub(super) inner: C,
    pub(super) on_raw: RawPollObserver,
}

pub(super) type RawPollObserver = Arc<dyn Fn(&str, bool) + Send + Sync>;

type ObservedTmuxCall<'a, T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<T, atm_tmux::TmuxError>> + Send + 'a>,
>;
macro_rules! observed_tmux_forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> ObservedTmuxCall<'async_trait, $result>
        where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
            Box::pin(atm_tmux::TmuxClient::$method(&self.inner, $($arg),*))
        }
    };
}

impl<C: atm_tmux::TmuxClient> atm_tmux::TmuxClient for ObservedTmux<C> {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        subcommand: &'c str,
        args: &'a [&'v str],
    ) -> ObservedTmuxCall<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut call = atm_tmux::TmuxClient::raw_command(&self.inner, subcommand, args);
            std::future::poll_fn(|cx| {
                let polled = std::future::Future::poll(call.as_mut(), cx);
                (self.on_raw)(subcommand, polled.is_pending());
                polled
            })
            .await
        })
    }
    observed_tmux_forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: atm_tmux::PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
    observed_tmux_forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    observed_tmux_forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    observed_tmux_forward!(kill_pane<'a>(pane: &'a str) -> ());
    observed_tmux_forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    observed_tmux_forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
    observed_tmux_forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
    observed_tmux_forward!(list_panes<>() -> Vec<atm_tmux::PaneInfo>);
    observed_tmux_forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    observed_tmux_forward!(select_pane<'a>(pane: &'a str) -> ());
    observed_tmux_forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    observed_tmux_forward!(new_session<'a>(name: &'a str) -> String);
}

#[tokio::test(start_paused = true)]
async fn first_poll_precedes_return_and_retains_an_already_entered_event() {
    let (entered, mut entry) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let call = spawn_polled(async move {
        entered.send(()).unwrap();
        released.await.unwrap();
        7
    });
    assert_eq!(entry.try_recv(), Ok(()), "the actual call was not polled");
    assert!(!call.is_finished(), "the dependency is still held");
    release.send(()).unwrap();
    assert_eq!(guarded(call).await.unwrap(), 7);
    // A call that already completed during its first poll keeps its result.
    assert_eq!(guarded(spawn_polled(async { 11 })).await.unwrap(), 11);
}

/// Deferred cases without a post-effect observation seam only. This bounded
/// yield poll is a known exception, not event synchronization; add no callers.
pub(super) async fn until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("never happened: {what}");
}

/// The pieces every rig shares: runtime dependencies that start no delivery.
pub(super) fn runtime(
    org: OrgHandle,
    high_water: u64,
    cancel: &CancellationToken,
) -> MessageRuntime {
    runtime_with_registry(org, high_water, cancel, crate::registry::spawn_registry())
}

fn runtime_with_registry(
    org: OrgHandle,
    high_water: u64,
    cancel: &CancellationToken,
    registry: crate::registry::RegistryHandle,
) -> MessageRuntime {
    let (control, _task) = crate::control::service::spawn_control(
        Box::new(atm_tmux::MockTmuxClient::new()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let clock = MessageClock::new(Instant::now(), utc());
    let policy = atm_org::delivery::DeliveryPolicy::default();
    let history = MessageHistoryHandle::new(high_water, utc(), policy.clone()).unwrap();
    MessageRuntime::new(
        org,
        registry,
        control,
        super::agents::AgentHandle::disabled("no agent service in this rig"),
        MessageEvidence::new(clock),
        history,
        policy,
        cancel.clone(),
    )
}

/// A real organisation actor whose appends stay in memory and pass a gate.
pub(super) struct Rig {
    pub(super) org: OrgHandle,
    pub(super) rt: MessageRuntime,
    pub(super) messages: MessageHandle,
    /// Zero permits hold every append after it was entered.
    gate: Arc<Semaphore>,
    /// The sequence of each append, sent when it is entered.
    entered: mpsc::UnboundedReceiver<u64>,
    /// Stops the organisation writer and, through its child, the messages.
    pub(super) cancel: CancellationToken,
    pub(super) task: tokio::task::JoinHandle<()>,
    /// Set when the test relays committed events to the service by hand.
    relay: Option<Relay>,
}

/// Committed events from the writer, and the service's source to feed.
type Relay = (
    broadcast::Receiver<Arc<Envelope>>,
    broadcast::Sender<Arc<Envelope>>,
);

impl Rig {
    async fn new() -> Self {
        Self::build(false, None).await
    }

    /// The service folds a committed event only when the test relays it.
    async fn relayed() -> Self {
        Self::build(true, None).await
    }

    /// The seeded rig with the recipient scheduler running on these effects.
    /// The messages stop on `rt.cancel` alone; the writer outlives them.
    pub(super) async fn scheduled(effects: Effects) -> Self {
        let rig = Self::build(false, Some(effects)).await;
        rig.seed().await;
        rig
    }

    fn relay(&mut self) {
        let (committed, service) = self.relay.as_mut().unwrap();
        while let Ok(envelope) = committed.try_recv() {
            service.send(envelope).unwrap();
        }
    }

    async fn build(relayed: bool, effects: Option<Effects>) -> Self {
        Self::build_registry(relayed, effects, crate::registry::spawn_registry()).await
    }

    async fn build_registry(
        relayed: bool,
        effects: Option<Effects>,
        registry: crate::registry::RegistryHandle,
    ) -> Self {
        let gate = Arc::new(Semaphore::new(1));
        let (enter, entered) = mpsc::unbounded_channel();
        let (requests, receive) = mpsc::channel::<Request>(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let next = Arc::new(AtomicU64::new(0));
        let held = gate.clone();
        tokio::spawn(super::actor::run(
            OrgState::default(),
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |_envelope: Envelope| {
                let seq = next.fetch_add(1, Ordering::SeqCst) + 1;
                let held = held.clone();
                let enter = enter.clone();
                async move {
                    let _ = enter.send(seq);
                    let _open = held.acquire().await.unwrap();
                    Ok::<u64, EventLogError>(seq)
                }
            },
        ));
        let org = OrgHandle {
            requests,
            health: status,
            committed: committed.clone(),
            reads: None,
        };
        let rt = runtime_with_registry(org.clone(), 0, &cancel.child_token(), registry);
        let relay = relayed.then(|| (committed.subscribe(), broadcast::channel(64).0));
        let source = match &relay {
            Some((_, service)) => service.subscribe(),
            None => committed.subscribe(),
        };
        let (messages, task) = spawn_service_for_test(rt.clone(), source, 0, effects);
        let rig = Self {
            org,
            rt,
            messages,
            gate,
            entered,
            cancel,
            task,
            relay,
        };
        guarded(rig.messages.initialized()).await.unwrap();
        rig
    }

    /// A project, `demo/worker` bound on %1 and `demo/other` bound on %2.
    pub(super) async fn seeded() -> Self {
        let rig = Self::new().await;
        rig.seed().await;
        rig
    }

    async fn seed(&self) {
        let rig = self;
        rig.run(OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        })
        .await;
        for (agent, pane, session) in [(worker(), "%1", "s1"), (other(), "%2", "s2")] {
            rig.run(OrgCommand::Adopt {
                agent_id: agent,
                pane: pane.into(),
                session_id: Some(session.into()),
            })
            .await;
        }
    }

    /// Execute directly on the writer and wait for the service's fold.
    pub(super) async fn run(&self, command: OrgCommand) -> u64 {
        let committed = guarded(self.org.execute(command, utc())).await.unwrap();
        let seq = committed.last().unwrap().seq;
        self.folded(seq).await;
        seq
    }

    pub(super) async fn folded(&self, seq: u64) {
        guarded(self.rt.history.folded(seq)).await.unwrap();
    }

    /// A write's actual future must reach Pending after its commit, while
    /// the relay still withholds that commit from the message fold.
    fn write_awaiting_fold<F>(
        &self,
        seq: u64,
        work: F,
    ) -> (
        tokio::task::JoinHandle<F::Output>,
        tokio::sync::oneshot::Receiver<()>,
    )
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (entered, entry) = tokio::sync::oneshot::channel();
        let committed = self.org.health.clone();
        let call = spawn_polled(async move {
            let mut work = Box::pin(work);
            let mut entered = Some(entered);
            std::future::poll_fn(|cx| {
                let polled = std::future::Future::poll(work.as_mut(), cx);
                if polled.is_pending()
                    && committed
                        .borrow()
                        .as_ref()
                        .is_ok_and(|cursor| *cursor >= seq)
                {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                }
                polled
            })
            .await
        });
        (call, entry)
    }

    /// The sequence of the next append the writer enters.
    pub(super) async fn entered(&mut self) -> u64 {
        guarded(self.entered.recv()).await.unwrap()
    }

    pub(super) async fn send(&self, caller: CallContext, to: Address, body: &str) -> MessageId {
        guarded(self.messages.send(note(to, body), caller))
            .await
            .unwrap()
            .message_id
    }

    pub(super) async fn inbox(&self, caller: CallContext, p: InboxParams) -> InboxResult {
        guarded(self.messages.inbox(p, caller)).await.unwrap()
    }

    /// Mark an agent message delivered, as an executor would.
    pub(super) async fn deliver(&self, id: MessageId, pane: &str, session: &str) {
        self.run(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: pane.into(),
        })
        .await;
        self.run(OrgCommand::Delivered {
            message_id: id,
            session_id: session.into(),
            confirmation: "prompt_submit_exact_reference".into(),
        })
        .await;
    }

    /// Hold every later append once it is entered; `entered` then yields the
    /// held append's sequence, not an earlier one's.
    pub(super) fn hold(&mut self) {
        self.gate.forget_permits(usize::MAX);
        while self.entered.try_recv().is_ok() {}
    }

    pub(super) fn release(&self) {
        self.gate.add_permits(1);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Broadening the predicate to other states/rows, or trusting a foreign real
// row to clear a pending refusal, must fail this literal decision matrix.
#[test]
fn discovery_identity_admission_matrix() {
    use atm_core::{
        AgentType, Harness, Model, SessionDomain, SessionId, SessionStatus, SessionView,
    };
    use atm_org::{Agent, AgentStatus};
    let agent = Agent {
        id: worker(),
        role: None,
        status: AgentStatus::Ready,
        pane: Some("%1".into()),
        session_id: Some(SessionId::new("s1")),
        excluded_session_id: None,
        reason: None,
        updated_at: utc(),
    };
    let row = |id: &str, pane: &str| {
        let mut domain = SessionDomain::new(
            SessionId::new(id),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        domain.harness = Harness::Pi;
        domain.tmux_pane = Some(pane.into());
        SessionView::from_domain(&domain)
    };
    let pending = row("pending-1", "%1");
    let real = row("s1", "%1");
    for status in [
        SessionStatus::Idle,
        SessionStatus::Working,
        SessionStatus::AttentionNeeded,
    ] {
        let mut pending = pending.clone();
        pending.status = status;
        assert!(unconfirmed_discovery(&agent, &[pending]), "{status:?}");
    }
    for status in [
        AgentStatus::Starting,
        AgentStatus::Clearing,
        AgentStatus::Down,
        AgentStatus::Stuck,
    ] {
        let mut other = agent.clone();
        other.status = status;
        assert!(
            !unconfirmed_discovery(&other, std::slice::from_ref(&pending)),
            "{status:?}"
        );
    }
    for binding in [None, Some(SessionId::new("pending-1"))] {
        let mut unbound = agent.clone();
        unbound.session_id = binding;
        assert!(!unconfirmed_discovery(
            &unbound,
            std::slice::from_ref(&pending)
        ));
    }
    let mut paneless = agent.clone();
    paneless.pane = None;
    assert!(!unconfirmed_discovery(
        &paneless,
        std::slice::from_ref(&pending)
    ));
    assert!(!unconfirmed_discovery(&agent, &[]));
    assert!(!unconfirmed_discovery(&agent, std::slice::from_ref(&real)));
    assert!(!unconfirmed_discovery(
        &agent,
        &[pending.clone(), real.clone()]
    ));
    assert!(!unconfirmed_discovery(&agent, &[row("other-real", "%1")]));
    let mut child = pending.clone();
    child.parent_session_id = Some(SessionId::new("parent"));
    assert!(!unconfirmed_discovery(&agent, &[child]));
    assert!(!unconfirmed_discovery(&agent, &[row("pending-1", "%2")]));
    let mut excluded = agent.clone();
    excluded.excluded_session_id = Some(pending.id.clone());
    assert!(!unconfirmed_discovery(
        &excluded,
        std::slice::from_ref(&pending)
    ));
    let mut child = real.clone();
    child.parent_session_id = Some(SessionId::new("parent"));
    for foreign in [child, row("s1", "%2")] {
        assert!(unconfirmed_discovery(&agent, &[pending.clone(), foreign]));
    }
    excluded.excluded_session_id = Some(real.id.clone());
    assert!(unconfirmed_discovery(&excluded, &[pending, real]));
}

// Real registry calls use the ordinary clock: discovery reads procfs and cwd.
async fn discovery_row(rig: &Rig) -> tempfile::TempDir {
    let cwd = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    guarded(rig.rt.registry.register_discovered(
        atm_core::SessionId::pending_from_pid(pid),
        pid,
        cwd.path().to_path_buf(),
        Some("%1".into()),
        atm_core::Harness::Pi,
    ))
    .await
    .unwrap();
    cwd
}

// Dropping the pre-admission fold fence must falsely refuse the delayed twin.
#[tokio::test]
async fn send_after_completed_clear_with_delayed_fold_enqueues_once() {
    send_after_completed_clear(true).await;
}

#[tokio::test]
async fn send_after_completed_clear_with_current_fold_enqueues_once() {
    send_after_completed_clear(false).await;
}

async fn send_after_completed_clear(delay_fold: bool) {
    let mut rig = Rig::build_registry(true, None, crate::registry::spawn_registry()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        },
    ] {
        guarded(rig.org.execute(command, utc())).await.unwrap();
    }
    rig.relay();
    rig.folded(rig.org.status().unwrap()).await;
    let _cwd = discovery_row(&rig).await;
    // A lazy-session clear has returned, but its message fold is held back.
    for command in [
        OrgCommand::Clear { agent_id: worker() },
        OrgCommand::EditorReady { agent_id: worker() },
    ] {
        guarded(rig.org.execute(command, utc())).await.unwrap();
    }
    let state = guarded(rig.org.snapshot()).await.unwrap();
    let current = state.agent(&worker()).unwrap();
    assert_eq!(current.status, atm_org::AgentStatus::Ready);
    assert_eq!(current.session_id, None);
    assert_eq!(current.excluded_session_id, Some("s1".into()));
    assert_eq!(
        rig.messages
            .service()
            .unwrap()
            .agent(&worker())
            .unwrap()
            .unwrap()
            .session_id,
        Some("s1".into())
    );
    let before = rig.org.status().unwrap();
    if !delay_fold {
        rig.relay();
        rig.folded(before).await;
    }
    let mut committed = rig.org.committed.subscribe();
    let handle = rig.messages.clone();
    let sending = handle.send(note(Address::Agent(worker()), "after clear"), owner());
    tokio::pin!(sending);
    if delay_fold {
        // Poll through admission before opening the fold barrier: the fixed
        // code waits for the cursor; the old code clones the stale binding.
        let first = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(sending.as_mut(), cx))
        })
        .await;
        assert!(first.is_pending(), "send bypassed the held fold: {first:?}");
        assert_eq!(rig.org.status().unwrap(), before);
        rig.relay();
        rig.folded(before).await;
    }
    let sent = guarded(async {
        tokio::select! {
            result = &mut sending => panic!("completed clear left Ready-unbound, but send returned before its append fold: {result:?}"),
            event = committed.recv() => {
                assert_eq!(event.unwrap().kind, "msg.sent");
                rig.relay();
                sending.await.unwrap()
            }
        }
    })
    .await;
    assert_eq!(rig.org.status().unwrap(), before + 1);
    let inbox = rig
        .inbox(
            owner(),
            InboxParams {
                recipient: Some(Address::Agent(worker())),
                ..page()
            },
        )
        .await;
    assert_eq!(ids(&inbox), [sent.message_id.seq()]);
    let entry = &inbox.messages[0];
    assert_eq!(entry.content.from, Address::Owner);
    assert_eq!(entry.content.body, "after clear");
    assert_eq!(entry.status, DeliveryStatus::Queued);
    assert_eq!(entry.delivery.attempt, 0);
}

// Removing the preflight, or moving it after append, must fail this test.
#[tokio::test]
async fn send_to_discovery_only_bound_recipient_is_refused_before_append() {
    for caller in [owner(), pane("%2")] {
        let rig = Rig::seeded().await;
        let _cwd = discovery_row(&rig).await;
        let before = rig.org.status().unwrap();
        let error = guarded(
            rig.messages
                .send(note(Address::Agent(worker()), "must not enqueue"), caller),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, MessageError::Refused(text) if text == "agent demo/worker has a discovered process but its recorded session is not confirmed; message not queued; inspect atm list and atm agent list before retrying"),
            "{error:?}"
        );
        assert_eq!(code(error), "refused");
        assert_eq!(rig.org.status().unwrap(), before);
        assert!(rig.inbox(pane("%1"), page()).await.messages.is_empty());
    }
}

// A refusal is not a latched failure: the accepted real-id event restores send.
#[tokio::test]
async fn send_after_real_identity_event_enqueues_once() {
    let rig = Rig::seeded().await;
    let _cwd = discovery_row(&rig).await;
    let params = SendParams {
        attachments: vec!["literal/brief.md".into()],
        task: Some("task-1".parse().unwrap()),
        ..note(Address::Agent(worker()), "original body\nsecond line")
    };
    assert!(matches!(
        guarded(rig.messages.send(params.clone(), pane("%2"))).await,
        Err(MessageError::Refused(_))
    ));
    guarded(rig.rt.registry.apply_lifecycle_event(
        atm_core::SessionId::new("s1"),
        atm_core::LifecycleEvent::WorkingEnd,
        atm_core::Harness::Pi,
        Some(std::process::id()),
        Some("%1".into()),
    ))
    .await
    .unwrap();
    let before = rig.org.status().unwrap();
    let sent = guarded(rig.messages.send(params, pane("%2")))
        .await
        .unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
    let inbox = rig.inbox(pane("%1"), page()).await;
    assert_eq!(inbox.messages.len(), 1);
    let entry = &inbox.messages[0];
    assert_eq!(entry.message_id, sent.message_id);
    assert_eq!(entry.status, DeliveryStatus::Queued);
    assert_eq!(entry.delivery.attempt, 0);
    assert_eq!(entry.content.from, Address::Agent(other()));
    assert_eq!(entry.content.to, Address::Agent(worker()));
    assert_eq!(entry.content.body, "original body\nsecond line");
    assert_eq!(entry.content.attachments, ["literal/brief.md"]);
    assert_eq!(entry.content.task, Some("task-1".parse().unwrap()));
}

fn registry_channel() -> (
    crate::registry::RegistryHandle,
    mpsc::Receiver<crate::registry::RegistryCommand>,
) {
    let (send, receive) = mpsc::channel(4);
    let (events, _) = broadcast::channel(4);
    (crate::registry::RegistryHandle::new(send, events), receive)
}

// A held current-cursor fold uses the ordinary budget without any append.
#[tokio::test(start_paused = true)]
async fn send_admission_fold_timeout_never_appends() {
    let (registry, _receive) = registry_channel();
    let mut rig = Rig::build_registry(true, None, registry).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%1".into(),
            session_id: None,
        },
    ] {
        guarded(rig.org.execute(command, utc())).await.unwrap();
    }
    let before = rig.org.status().unwrap();
    let handle = rig.messages.clone();
    let sending = handle.send(note(Address::Agent(worker()), "held fold"), owner());
    tokio::pin!(sending);
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(sending.as_mut(), cx))
    })
    .await;
    assert!(first.is_pending(), "send bypassed the held fold: {first:?}");
    tokio::time::advance(std::time::Duration::from_secs(8)).await;
    let result = std::future::poll_fn(|cx| {
        let result = std::future::Future::poll(sending.as_mut(), cx);
        assert!(
            result.is_ready(),
            "admission fold exceeded the ordinary budget"
        );
        result
    })
    .await;
    assert!(matches!(result, Err(MessageError::Timeout)), "{result:?}");
    assert_eq!(rig.org.status().unwrap(), before);
    rig.relay();
    rig.folded(before).await;
    let state = guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(state.messages().count(), 0);
}

// A lost registry answer cannot turn into permission to enqueue.
#[tokio::test(start_paused = true)]
async fn send_identity_read_failure_never_appends() {
    let (registry, receive) = registry_channel();
    drop(receive);
    let rig = Rig::build_registry(false, None, registry).await;
    rig.seed().await;
    let before = rig.org.status().unwrap();
    let result = guarded(
        rig.messages
            .send(note(Address::Agent(worker()), "closed"), owner()),
    )
    .await;
    assert!(
        matches!(result, Err(MessageError::Unavailable(_))),
        "{result:?}"
    );
    assert_eq!(rig.org.status().unwrap(), before);

    let (registry, mut receive) = registry_channel();
    let rig = Rig::build_registry(false, None, registry).await;
    rig.seed().await;
    let before = rig.org.status().unwrap();
    let sending = rig
        .messages
        .send(note(Address::Agent(worker()), "held"), owner());
    tokio::pin!(sending);
    let request = tokio::select! {
        result = &mut sending => panic!("send answered without its registry read: {result:?}"),
        request = receive.recv() => request.unwrap(),
    };
    let crate::registry::RegistryCommand::PaneSessions {
        pane: target,
        respond_to,
    } = request
    else {
        panic!("unexpected registry request");
    };
    assert_eq!(target, "%1");
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    let result = std::future::poll_fn(|cx| {
        let result = std::future::Future::poll(sending.as_mut(), cx);
        assert!(
            result.is_ready(),
            "registry read exceeded its five-second budget"
        );
        result
    })
    .await;
    assert!(matches!(result, Err(MessageError::Timeout)), "{result:?}");
    assert_eq!(rig.org.status().unwrap(), before);
    assert!(rig.inbox(pane("%1"), page()).await.messages.is_empty());
    drop(respond_to);
}

// Owner messages must still succeed without a functioning registry.
#[tokio::test(start_paused = true)]
async fn owner_message_bypasses_identity_read() {
    let (registry, receive) = registry_channel();
    drop(receive);
    let rig = Rig::build_registry(false, None, registry).await;
    rig.seed().await;
    let id = rig.send(pane("%2"), Address::Owner, "report").await;
    let inbox = rig.inbox(owner(), page()).await;
    assert_eq!(ids(&inbox), [id.seq()]);
    assert_eq!(inbox.messages[0].status, DeliveryStatus::Delivered);
    assert_eq!(inbox.messages[0].content.from, Address::Agent(other()));
}

#[tokio::test(start_paused = true)]
async fn sender_from_live_bound_pane() {
    let rig = Rig::seeded().await;
    let id = rig
        .send(pane("%1"), Address::Owner, "from the worker")
        .await;
    let inbox = rig.inbox(owner(), page()).await;
    assert_eq!(ids(&inbox), [id.seq()]);
    assert_eq!(inbox.messages[0].content.from, Address::Agent(worker()));
    assert_eq!(inbox.messages[0].content.body, "from the worker");
}

#[tokio::test(start_paused = true)]
async fn unknown_or_down_pane_is_owner() {
    let rig = Rig::seeded().await;
    rig.send(pane("%9"), Address::Agent(worker()), "unbound pane")
        .await;
    rig.run(OrgCommand::Down {
        agent_id: other(),
        reason: "stopped".into(),
    })
    .await;
    rig.send(
        pane("%2"),
        Address::Agent(worker()),
        "a down agent's old pane",
    )
    .await;
    let inbox = rig.inbox(pane("%1"), page()).await;
    assert_eq!(inbox.messages.len(), 2);
    for entry in &inbox.messages {
        assert_eq!(entry.content.from, Address::Owner);
    }
}

#[tokio::test(start_paused = true)]
async fn malformed_pane_is_invalid() {
    let rig = Rig::seeded().await;
    let before = rig.org.status().unwrap();
    for bad in ["7", "", "%", "%1 ", "demo/worker"] {
        let send = rig
            .messages
            .send(note(Address::Owner, "x"), pane(bad))
            .await;
        assert!(
            matches!(send, Err(MessageError::InvalidParams(_))),
            "{bad:?}"
        );
        let inbox = rig.messages.inbox(page(), pane(bad)).await;
        assert!(
            matches!(inbox, Err(MessageError::InvalidParams(_))),
            "{bad:?}"
        );
        let ack = rig
            .messages
            .ack(
                AckParams {
                    ids: vec![MessageId::new(1)],
                },
                pane(bad),
            )
            .await;
        assert!(
            matches!(ack, Err(MessageError::InvalidParams(_))),
            "{bad:?}"
        );
    }
    // Never the owner by fallback: nothing was committed.
    assert_eq!(rig.org.status().unwrap(), before);
}

#[tokio::test(start_paused = true)]
async fn system_target_refused() {
    let rig = Rig::seeded().await;
    let before = rig.org.status().unwrap();
    let send = rig.messages.send(note(Address::System, "x"), owner()).await;
    assert!(matches!(send, Err(MessageError::Refused(_))));
    let inbox = rig
        .messages
        .inbox(
            InboxParams {
                recipient: Some(Address::System),
                ..page()
            },
            owner(),
        )
        .await;
    assert!(matches!(inbox, Err(MessageError::Refused(_))));
    assert_eq!(rig.org.status().unwrap(), before);
}

#[tokio::test(start_paused = true)]
async fn agent_inbox_isolation_and_owner_inspection() {
    let rig = Rig::seeded().await;
    let mine = rig
        .send(owner(), Address::Agent(worker()), "for the worker")
        .await;
    let theirs = rig
        .send(owner(), Address::Agent(other()), "for the other")
        .await;
    // An agent reads its own inbox, by default and by name.
    assert_eq!(ids(&rig.inbox(pane("%1"), page()).await), [mine.seq()]);
    let named = InboxParams {
        recipient: Some(Address::Agent(worker())),
        ..page()
    };
    assert_eq!(
        ids(&rig.inbox(pane("%1"), named.clone()).await),
        [mine.seq()]
    );
    // Not another agent's, and not the owner's.
    for recipient in [Address::Agent(other()), Address::Owner] {
        // CS2 checks reporting roles on a detached thread for cross-agent reads.
        tokio::time::resume();
        let refused = rig
            .messages
            .inbox(
                InboxParams {
                    recipient: Some(recipient),
                    ..page()
                },
                pane("%1"),
            )
            .await;
        tokio::time::pause();
        assert!(matches!(refused, Err(MessageError::Refused(_))));
    }
    // The owner inspects any agent's inbox, and reads nothing as a side effect.
    assert_eq!(ids(&rig.inbox(owner(), named).await), [mine.seq()]);
    let theirs_page = InboxParams {
        recipient: Some(Address::Agent(other())),
        ..page()
    };
    let inspected = rig.inbox(owner(), theirs_page).await;
    assert_eq!(ids(&inspected), [theirs.seq()]);
    assert_eq!(inspected.unread_count, 1);
    assert!(inspected.messages[0].read_at.is_none());
}

#[tokio::test(start_paused = true)]
async fn owner_count_only_never_exposes_bodies() {
    let rig = Rig::seeded().await;
    rig.send(pane("%2"), Address::Owner, "a private report")
        .await;
    rig.send(pane("%2"), Address::Owner, "another one").await;
    let count = InboxParams {
        recipient: Some(Address::Owner),
        count_only: true,
        ..page()
    };
    // Any caller may learn how many messages the owner has not read.
    for caller in [pane("%1"), pane("%9"), owner()] {
        let result = rig.inbox(caller, count.clone()).await;
        assert_eq!(result.unread_count, 2);
        assert!(result.messages.is_empty());
        assert!(result.next_after.is_none());
    }
    // The exception is the owner's count only: not another agent's.
    let other_count = InboxParams {
        recipient: Some(Address::Agent(other())),
        count_only: true,
        ..page()
    };
    // CS2 checks reporting roles on a detached thread even for count-only reads.
    tokio::time::resume();
    let refused = rig.messages.inbox(other_count, pane("%1")).await;
    tokio::time::pause();
    assert!(matches!(refused, Err(MessageError::Refused(_))));
}

#[tokio::test(start_paused = true)]
async fn unknown_agent_or_message_not_found() {
    let rig = Rig::seeded().await;
    let ghost: AgentId = "demo/ghost".parse().unwrap();
    let inbox = rig
        .messages
        .inbox(
            InboxParams {
                recipient: Some(Address::Agent(ghost.clone())),
                ..page()
            },
            owner(),
        )
        .await;
    assert!(matches!(inbox, Err(MessageError::NotFound(_))));
    let by_id = rig
        .messages
        .inbox(
            InboxParams {
                id: Some(MessageId::new(999)),
                ..page()
            },
            owner(),
        )
        .await
        .unwrap_err();
    assert_eq!(by_id.to_string(), "message m999 was not found");
    assert!(matches!(by_id, MessageError::NotFound(_)));
    let send = rig
        .messages
        .send(note(Address::Agent(ghost), "x"), owner())
        .await
        .unwrap_err();
    assert_eq!(code(send), "not_found");
    let ack = rig
        .messages
        .ack(
            AckParams {
                ids: vec![MessageId::new(999)],
            },
            owner(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(ack), "not_found");
}

#[tokio::test(start_paused = true)]
async fn foreign_message_refused() {
    let rig = Rig::seeded().await;
    let theirs = rig
        .send(owner(), Address::Agent(other()), "for the other")
        .await;
    let refused = rig
        .messages
        .inbox(
            InboxParams {
                id: Some(theirs),
                ..page()
            },
            pane("%1"),
        )
        .await
        .unwrap_err();
    assert!(matches!(refused, MessageError::Refused(_)));
    assert_eq!(
        refused.to_string(),
        format!("message {theirs} is not addressed to demo/worker")
    );
    // The owner asking for its own inbox by that ID is refused the same way.
    let owner_view = rig
        .messages
        .inbox(
            InboxParams {
                id: Some(theirs),
                ..page()
            },
            owner(),
        )
        .await;
    assert!(matches!(owner_view, Err(MessageError::Refused(_))));
}

#[tokio::test(start_paused = true)]
async fn ack_batch_is_atomic() {
    let rig = Rig::seeded().await;
    let a = rig.send(pane("%1"), Address::Owner, "a").await;
    let b = rig.send(pane("%1"), Address::Owner, "b").await;
    let foreign = rig
        .send(owner(), Address::Agent(worker()), "not the owner's")
        .await;
    for (batch, expected) in [
        (vec![a, MessageId::new(999), b], "not_found"),
        (vec![a, foreign, b], "refused"),
    ] {
        let error = rig
            .messages
            .ack(AckParams { ids: batch }, owner())
            .await
            .unwrap_err();
        assert_eq!(code(error), expected);
        let inbox = rig.inbox(owner(), page()).await;
        assert_eq!(inbox.unread_count, 2);
        assert!(inbox.messages.iter().all(|m| m.read_at.is_none()));
    }
}

#[tokio::test(start_paused = true)]
async fn ack_mixed_read_unread_counts_only_transitions() {
    let rig = Rig::seeded().await;
    let a = rig.send(pane("%1"), Address::Owner, "a").await;
    let b = rig.send(pane("%1"), Address::Owner, "b").await;
    let c = rig.send(pane("%1"), Address::Owner, "c").await;
    let ack = |ids: Vec<MessageId>| rig.messages.ack(AckParams { ids }, owner());
    assert_eq!(ack(vec![a]).await.unwrap().count, 1);
    // The committed event names all three IDs; only two became read.
    assert_eq!(ack(vec![a, b, c]).await.unwrap().count, 2);
    assert_eq!(rig.inbox(owner(), page()).await.unread_count, 0);
}

#[tokio::test(start_paused = true)]
async fn repeated_ack_returns_zero() {
    let rig = Rig::seeded().await;
    let a = rig.send(pane("%1"), Address::Owner, "a").await;
    let ack = || rig.messages.ack(AckParams { ids: vec![a] }, owner());
    assert_eq!(ack().await.unwrap().count, 1);
    let cursor = rig.org.status().unwrap();
    let first_read = rig.inbox(owner(), page()).await.messages[0].read_at;
    assert_eq!(ack().await.unwrap().count, 0);
    // No second event, and the first read time stands.
    assert_eq!(rig.org.status().unwrap(), cursor);
    assert_eq!(
        rig.inbox(owner(), page()).await.messages[0].read_at,
        first_read
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_ack_counts_once() {
    let mut rig = Rig::seeded().await;
    let a = rig.send(pane("%1"), Address::Owner, "a").await;
    rig.hold();
    let first = tokio::spawn({
        let messages = rig.messages.clone();
        async move { messages.ack(AckParams { ids: vec![a] }, owner()).await }
    });
    // The first acknowledgement's append is in progress; the second request
    // is admitted behind it and decided against the state the first leaves.
    rig.entered().await;
    let second = spawn_polled({
        let messages = rig.messages.clone();
        async move { messages.ack(AckParams { ids: vec![a] }, owner()).await }
    });
    assert_eq!(
        rig.org.requests.capacity(),
        63,
        "second acknowledgement admitted"
    );
    rig.release();
    let counts = [
        guarded(first).await.unwrap().unwrap().count,
        guarded(second).await.unwrap().unwrap().count,
    ];
    assert_eq!(counts, [1, 0]);
}

#[tokio::test(start_paused = true)]
async fn ack_open_batch_is_recipient_only_and_counts_folded_reads() {
    let mut rig = Rig::seeded().await;
    let delivered = rig
        .send(owner(), Address::Agent(worker()), "delivered")
        .await;
    rig.deliver(delivered, "%1", "s1").await;
    let queued = rig
        .send(pane("%2"), Address::Agent(worker()), "queued")
        .await;
    let failed = rig.send(owner(), Address::Agent(worker()), "failed").await;
    rig.run(OrgCommand::DeliveryFailed {
        message_id: failed,
        reason: "owner gave up on delivery".into(),
        final_failure: true,
    })
    .await;
    let foreign = rig.send(owner(), Address::Agent(other()), "foreign").await;
    let cursor = rig.org.status().unwrap();
    for (caller, ids, want) in [
        (owner(), vec![queued], "refused"),
        (pane("%2"), vec![queued], "refused"),
        (pane("%1"), vec![queued, foreign], "refused"),
        (pane("%1"), vec![queued, MessageId::new(999)], "not_found"),
    ] {
        let error = rig
            .messages
            .ack(AckParams { ids }, caller)
            .await
            .unwrap_err();
        if want == "refused" {
            assert!(error.to_string().contains("is not addressed to"), "{error}");
        }
        assert_eq!(code(error), want);
        assert_eq!(rig.org.status().unwrap(), cursor);
    }
    let only = InboxParams {
        id: Some(queued),
        ..page()
    };
    let peek = rig.inbox(pane("%1"), only.clone()).await;
    assert_eq!(peek.unread_count, 3);
    assert_eq!(peek.messages[0].status, DeliveryStatus::Queued);
    assert!(peek.messages[0].read_at.is_none());
    assert_eq!(rig.org.status().unwrap(), cursor);

    let ids = vec![queued, delivered, failed];
    let ack = |messages: MessageHandle, ids| async move {
        messages.ack(AckParams { ids }, pane("%1")).await
    };
    rig.hold();
    let mut first = spawn_polled(ack(rig.messages.clone(), ids.clone()));
    tokio::select! {
        _ = rig.entered() => {},
        result = &mut first => panic!("ack returned instead of writing: {result:?}"),
    }
    let second = spawn_polled(ack(rig.messages.clone(), ids.clone()));
    rig.release();
    assert_eq!(guarded(first).await.unwrap().unwrap().count, 3);
    assert_eq!(guarded(second).await.unwrap().unwrap().count, 0);
    assert_eq!(rig.org.status().unwrap(), cursor + 2);
    let inbox = rig.inbox(pane("%1"), page()).await;
    assert_eq!(inbox.unread_count, 0);
    let row = |id| inbox.messages.iter().find(|m| m.message_id == id).unwrap();
    assert_eq!(row(queued).status, DeliveryStatus::Failed);
    assert_eq!(
        row(queued).delivery.reason.as_deref(),
        Some("recipient acknowledged; automatic delivery cancelled")
    );
    assert_eq!(
        row(failed).delivery.reason.as_deref(),
        Some("owner gave up on delivery")
    );
    assert_eq!(
        row(delivered)
            .delivery
            .session_id
            .as_ref()
            .unwrap()
            .as_str(),
        "s1"
    );
    assert!(row(queued).read_at.is_some());
    assert_eq!(ack(rig.messages.clone(), ids).await.unwrap().count, 0);
    assert_eq!(rig.org.status().unwrap(), cursor + 2);
}

#[tokio::test(start_paused = true)]
async fn inbox_sequence_pagination_and_filters() {
    let rig = Rig::seeded().await;
    let mut sent = Vec::new();
    for n in 0..5 {
        sent.push(
            rig.send(pane("%1"), Address::Owner, &format!("report {n}"))
                .await,
        );
    }
    let seqs: Vec<u64> = sent.iter().map(|id| id.seq()).collect();
    let limited = |after: Option<MessageId>| InboxParams {
        after,
        limit: 2,
        ..page()
    };
    let first = rig.inbox(owner(), limited(None)).await;
    assert_eq!(ids(&first), seqs[..2]);
    assert_eq!(first.next_after, Some(sent[1]));
    let second = rig.inbox(owner(), limited(first.next_after)).await;
    assert_eq!(ids(&second), seqs[2..4]);
    assert_eq!(second.next_after, Some(sent[3]));
    let last = rig.inbox(owner(), limited(second.next_after)).await;
    assert_eq!(ids(&last), seqs[4..]);
    assert_eq!(last.next_after, None);
    // A page that ends exactly at the last match names no continuation.
    let exact = rig.inbox(owner(), InboxParams { limit: 5, ..page() }).await;
    assert_eq!(ids(&exact), seqs);
    assert_eq!(exact.next_after, None);

    rig.messages
        .ack(
            AckParams {
                ids: vec![sent[0], sent[2]],
            },
            owner(),
        )
        .await
        .unwrap();
    let unread = InboxParams {
        unread_only: true,
        ..page()
    };
    assert_eq!(
        ids(&rig.inbox(owner(), unread.clone()).await),
        [seqs[1], seqs[3], seqs[4]]
    );
    // An ID is one entry, and still respects the unread filter.
    let by_id = |id, unread_only| InboxParams {
        id: Some(id),
        unread_only,
        ..page()
    };
    assert_eq!(
        ids(&rig.inbox(owner(), by_id(sent[2], false)).await),
        [seqs[2]]
    );
    assert!(rig
        .inbox(owner(), by_id(sent[2], true))
        .await
        .messages
        .is_empty());
    assert_eq!(
        ids(&rig.inbox(owner(), by_id(sent[3], true)).await),
        [seqs[3]]
    );
}

#[tokio::test(start_paused = true)]
async fn unread_count_is_independent_of_page() {
    let rig = Rig::seeded().await;
    let mut sent = Vec::new();
    for n in 0..4 {
        sent.push(
            rig.send(pane("%1"), Address::Owner, &format!("report {n}"))
                .await,
        );
    }
    rig.messages
        .ack(AckParams { ids: vec![sent[0]] }, owner())
        .await
        .unwrap();
    for p in [
        page(),
        InboxParams { limit: 1, ..page() },
        InboxParams {
            after: Some(sent[3]),
            ..page()
        },
        InboxParams {
            id: Some(sent[0]),
            ..page()
        },
        InboxParams {
            unread_only: true,
            limit: 1,
            ..page()
        },
        InboxParams {
            count_only: true,
            ..page()
        },
    ] {
        assert_eq!(rig.inbox(owner(), p).await.unread_count, 3);
    }
    // Queued and failed messages count too: they are unread.
    let queued = rig.send(owner(), Address::Agent(worker()), "queued").await;
    assert_eq!(rig.inbox(pane("%1"), page()).await.unread_count, 1);
    rig.run(OrgCommand::DeliveryFailed {
        message_id: queued,
        reason: "owner gave up on delivery".into(),
        final_failure: true,
    })
    .await;
    let failed = rig.inbox(pane("%1"), page()).await;
    assert_eq!(failed.unread_count, 1);
    // The durable failure reason is what the inbox shows for it.
    assert_eq!(failed.messages[0].delivery.phase, DeliveryPhase::Failed);
    assert_eq!(
        failed.messages[0].delivery.reason.as_deref(),
        Some("owner gave up on delivery")
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_byte_cap_never_truncates_body() {
    let rig = Rig::seeded().await;
    // 40 bodies of 16 KiB: more than one 512 KiB page.
    let body = "é".repeat(8192);
    let mut sent = Vec::new();
    for _ in 0..40 {
        sent.push(rig.send(pane("%1"), Address::Owner, &body).await);
    }
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let result = rig.inbox(owner(), InboxParams { after, ..page() }).await;
        assert!(!result.messages.is_empty());
        assert!(serde_json::to_vec(&result).unwrap().len() <= 512 * 1024);
        for entry in &result.messages {
            assert_eq!(entry.content.body, body);
            seen.push(entry.message_id);
        }
        after = result.next_after;
        if after.is_none() {
            break;
        }
        // The cap, not the limit of 100, ended this page.
        assert!(result.messages.len() < 40);
    }
    assert_eq!(seen, sent);
}

#[tokio::test(start_paused = true)]
async fn oversized_legacy_entry_refused_with_id() {
    let rig = Rig::seeded().await;
    let small = rig.send(pane("%1"), Address::Owner, "small").await;
    // Committed by the writer directly, past the router's bounds.
    let huge = MessageId::new(
        rig.run(OrgCommand::SendMessage(MessageContent {
            delivery_notice: None,
            from: Address::System,
            to: Address::Owner,
            kind: MessageKind::Note,
            body: "x".repeat(600 * 1024),
            attachments: vec![],
            task: None,
        }))
        .await,
    );
    let later = rig.send(pane("%1"), Address::Owner, "later").await;
    // The page stops before the entry that does not fit.
    let first = rig.inbox(owner(), page()).await;
    assert_eq!(ids(&first), [small.seq()]);
    assert_eq!(first.next_after, Some(small));
    // Asked for next, it is refused by ID rather than cut short.
    let refused = rig
        .messages
        .inbox(
            InboxParams {
                after: Some(small),
                ..page()
            },
            owner(),
        )
        .await
        .unwrap_err();
    assert!(matches!(refused, MessageError::Refused(_)));
    assert!(refused.to_string().contains(&huge.to_string()), "{refused}");
    // The caller can step over it.
    let rest = rig
        .inbox(
            owner(),
            InboxParams {
                after: Some(huge),
                ..page()
            },
        )
        .await;
    assert_eq!(ids(&rest), [later.seq()]);
    assert_eq!(rest.unread_count, 3);
}

#[tokio::test(start_paused = true)]
async fn delivery_view_uses_attempt_and_receipt_not_current_binding() {
    let rig = Rig::seeded().await;
    let id = rig.send(owner(), Address::Agent(worker()), "brief").await;
    let fresh = &rig.inbox(pane("%1"), page()).await.messages[0];
    assert_eq!(fresh.delivery.phase, DeliveryPhase::Queued);
    assert_eq!(fresh.delivery.attempt, 0);
    assert!(fresh.delivery.pane.is_none() && fresh.delivery.session_id.is_none());
    rig.deliver(id, "%1", "s1").await;
    // The recipient moves to another pane and session afterwards.
    rig.run(OrgCommand::Down {
        agent_id: worker(),
        reason: "moved".into(),
    })
    .await;
    rig.run(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%5".into(),
        session_id: Some("s9".into()),
    })
    .await;
    let entry = &rig.inbox(pane("%5"), page()).await.messages[0];
    assert_eq!(entry.status, DeliveryStatus::Delivered);
    assert_eq!(entry.delivery.phase, DeliveryPhase::Delivered);
    assert_eq!(entry.delivery.attempt, 1);
    assert_eq!(entry.delivery.pane.as_deref(), Some("%1"));
    assert_eq!(entry.delivery.session_id.as_ref().unwrap().as_str(), "s1");
    assert_eq!(
        entry.delivery.confirmation.as_deref(),
        Some("prompt_submit_exact_reference")
    );
    // A message to the owner is delivered by the fold, with no session receipt.
    rig.send(pane("%5"), Address::Owner, "report").await;
    let report = &rig.inbox(owner(), page()).await.messages[0];
    assert_eq!(report.delivery.phase, DeliveryPhase::Delivered);
    assert!(report.delivery.session_id.is_none() && report.delivery.pane.is_none());
}

#[tokio::test(start_paused = true)]
async fn blocked_predecessor_marks_future_follower() {
    let rig = Rig::seeded().await;
    let head = rig.send(owner(), Address::Agent(worker()), "first").await;
    let follower = rig.send(owner(), Address::Agent(worker()), "second").await;
    let elsewhere = rig
        .send(owner(), Address::Agent(other()), "another recipient")
        .await;
    let blocked_by = |inbox: &InboxResult| -> Vec<Option<MessageId>> {
        inbox
            .messages
            .iter()
            .map(|m| m.delivery.blocked_by)
            .collect()
    };
    // FIFO per recipient: the follower names its predecessor, not the reverse,
    // and another recipient's queue is its own.
    assert_eq!(
        blocked_by(&rig.inbox(pane("%1"), page()).await),
        [None, Some(head)]
    );
    assert_eq!(blocked_by(&rig.inbox(pane("%2"), page()).await), [None]);
    // A message sent after the head was attempted is marked the same way.
    rig.run(OrgCommand::DeliveryAttempted {
        message_id: head,
        attempt: 1,
        pane: "%1".into(),
    })
    .await;
    let later = rig.send(owner(), Address::Agent(worker()), "third").await;
    assert_eq!(
        blocked_by(&rig.inbox(pane("%1"), page()).await),
        [None, Some(head), Some(head)]
    );
    // Once the head is terminal the next one leads; terminal entries name nobody.
    rig.run(OrgCommand::DeliveryFailed {
        message_id: head,
        reason: "owner gave up on delivery".into(),
        final_failure: true,
    })
    .await;
    assert_eq!(
        blocked_by(&rig.inbox(pane("%1"), page()).await),
        [None, None, Some(follower)]
    );
    assert!(later > follower && elsewhere > follower);
}

#[tokio::test(start_paused = true)]
async fn owner_message_is_never_a_pane_job() {
    let rig = Rig::seeded().await;
    let report = rig.send(pane("%1"), Address::Owner, "report").await;
    let brief = rig.send(owner(), Address::Agent(worker()), "brief").await;
    let entry = &rig.inbox(owner(), page()).await.messages[0];
    assert_eq!(entry.status, DeliveryStatus::Delivered);
    // Nothing expects a reference for it, and it has no delivery snapshot.
    assert!(matches!(
        rig.rt.evidence.snapshot(report),
        Err(MessageError::Unavailable(_))
    ));
    assert!(matches!(
        rig.rt.history.snapshot(report, utc()),
        Err(MessageError::Refused(_))
    ));
    // An agent message is expected by evidence from the moment it is folded.
    let facts = rig.rt.evidence.snapshot(brief).unwrap();
    assert!(facts.receipt.is_none() && !facts.probably_landed);
    assert!(rig.rt.history.snapshot(brief, utc()).is_ok());
}

#[tokio::test(start_paused = true)]
async fn client_disconnect_after_enqueue_still_commits() {
    let mut rig = Rig::seeded().await;
    rig.hold();
    let call = tokio::spawn({
        let messages = rig.messages.clone();
        async move {
            messages
                .send(note(Address::Owner, "kept"), pane("%1"))
                .await
        }
    });
    let seq = rig.entered().await;
    // The caller goes away while its append is in progress.
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    rig.release();
    rig.folded(seq).await;
    let inbox = rig.inbox(owner(), page()).await;
    assert_eq!(ids(&inbox), [seq]);
    assert_eq!(inbox.messages[0].content.body, "kept");
}

#[tokio::test(start_paused = true)]
async fn append_stalled_inbox_read_completes() {
    let mut rig = Rig::seeded().await;
    let before = rig.send(pane("%1"), Address::Owner, "before").await;
    rig.hold();
    let stalled = tokio::spawn({
        let messages = rig.messages.clone();
        async move {
            messages
                .send(note(Address::Owner, "stalled"), pane("%1"))
                .await
        }
    });
    rig.entered().await;
    // The writer is stuck in an append. A read needs neither it nor the fold.
    let inbox = rig.inbox(owner(), page()).await;
    assert_eq!(ids(&inbox), [before.seq()]);
    rig.release();
    let sent = guarded(stalled).await.unwrap().unwrap().message_id;
    assert_eq!(
        ids(&rig.inbox(owner(), page()).await),
        [before.seq(), sent.seq()]
    );
}

#[tokio::test(start_paused = true)]
async fn stalled_write_times_out_typed_and_still_commits() {
    let mut rig = Rig::seeded().await;
    rig.hold();
    let call = tokio::spawn({
        let messages = rig.messages.clone();
        async move {
            messages
                .send(note(Address::Owner, "late"), pane("%1"))
                .await
        }
    });
    let seq = rig.entered().await;
    // The paused clock runs the service's budget out while the append stalls.
    let error = guarded(call).await.unwrap().unwrap_err();
    assert!(
        matches!(
            error,
            MessageError::Timeout | MessageError::Organisation(OrgServiceError::Timeout)
        ),
        "{error}"
    );
    assert_eq!(code(error), "timeout");
    // A timeout is not a rollback: the accepted command commits.
    rig.release();
    rig.folded(seq).await;
    assert_eq!(ids(&rig.inbox(owner(), page()).await), [seq]);
}

#[tokio::test(start_paused = true)]
async fn request_admission_is_bounded_without_waiters() {
    let mut rig = Rig::seeded().await;
    rig.hold();
    let mut calls = Vec::new();
    for n in 0..64 {
        let messages = rig.messages.clone();
        calls.push(spawn_polled(async move {
            messages
                .send(note(Address::Owner, &format!("{n}")), owner())
                .await
        }));
    }
    // The first one is in its append; the other 63 queue behind it.
    rig.entered().await;
    assert_eq!(
        rig.org.requests.capacity(),
        1,
        "63 sends admitted behind the append"
    );
    // The 65th is refused at once, and an inbox read is a request too.
    let refused = rig.messages.send(note(Address::Owner, "65"), owner()).await;
    assert!(matches!(refused, Err(MessageError::Unavailable(_))));
    let read = rig.messages.inbox(page(), owner()).await;
    assert!(matches!(read, Err(MessageError::Unavailable(_))));
    rig.release();
    for call in calls {
        guarded(call).await.unwrap().unwrap();
    }
    assert_eq!(rig.inbox(owner(), page()).await.messages.len(), 64);
}

#[tokio::test(start_paused = true)]
async fn write_reply_waits_for_its_fold() {
    let mut rig = Rig::relayed().await;
    let (send, pending) = rig.write_awaiting_fold(1, {
        let messages = rig.messages.clone();
        async move { messages.send(note(Address::Owner, "report"), owner()).await }
    });
    guarded(pending).await.unwrap();
    assert_eq!(rig.org.status().ok(), Some(1));
    // Committed, but the service has not folded it: the caller is not told
    // yet, because its next read would not find its own write.
    assert!(!send.is_finished());
    rig.relay();
    let id = guarded(send).await.unwrap().unwrap().message_id;
    assert_eq!(ids(&rig.inbox(owner(), page()).await), [id.seq()]);

    // An acknowledgement's count is taken at its fold, so it waits for it too.
    let (ack, pending) = rig.write_awaiting_fold(2, {
        let messages = rig.messages.clone();
        async move { messages.ack(AckParams { ids: vec![id] }, owner()).await }
    });
    guarded(pending).await.unwrap();
    assert_eq!(rig.org.status().ok(), Some(2));
    assert!(!ack.is_finished());
    rig.relay();
    assert_eq!(guarded(ack).await.unwrap().unwrap().count, 1);
}

/// A service fed by hand: the test decides which envelopes arrive, in order.
struct Replay {
    events: broadcast::Sender<Arc<Envelope>>,
    rt: MessageRuntime,
    messages: MessageHandle,
    cancel: CancellationToken,
}

impl Replay {
    fn new(high_water: u64) -> Self {
        let (events, source) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let rt = runtime(OrgHandle::disabled("replay only"), high_water, &cancel);
        let (messages, _task) = spawn_service_for_test(rt.clone(), source, high_water, None);
        Self {
            events,
            rt,
            messages,
            cancel,
        }
    }

    fn feed(&self, seq: u64, event: &OrgEvent) {
        let envelope = Envelope {
            seq,
            ts: utc(),
            kind: event.kind().into(),
            body: event.to_body(),
        };
        self.events.send(Arc::new(envelope)).unwrap();
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn project() -> OrgEvent {
    OrgEvent::ProjectRegistered {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
    }
}

fn adopted() -> OrgEvent {
    OrgEvent::AgentAdopted {
        agent_id: worker(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    }
}

fn sent(to: Address, body: &str) -> OrgEvent {
    OrgEvent::MessageSent(MessageContent {
        delivery_notice: None,
        from: Address::Owner,
        to,
        kind: MessageKind::Note,
        body: body.into(),
        attachments: vec![],
        task: None,
    })
}

#[tokio::test(start_paused = true)]
async fn replay_live_overlap_applies_once() {
    let replay = Replay::new(3);
    let events = [
        project(),
        adopted(),
        sent(Address::Agent(worker()), "one"),
        sent(Address::Agent(worker()), "two"),
    ];
    // The replay delivers 1-3; the live stream overlaps it with 2-4.
    for seq in [1, 2, 3, 2, 3, 4] {
        replay.feed(seq, &events[seq as usize - 1]);
    }
    guarded(replay.rt.history.folded(4)).await.unwrap();
    guarded(replay.messages.initialized()).await.unwrap();
    let inbox = replay.messages.inbox(page(), pane("%1")).await.unwrap();
    // A second apply of 3 would be refused as a duplicate message and stop
    // the service; a skipped 4 would hide the second message.
    assert_eq!(ids(&inbox), [3, 4]);
    assert_eq!(inbox.unread_count, 2);
}

#[tokio::test(start_paused = true)]
async fn keyed_notice_replay_live_overlap_keeps_one_key() {
    let replay = Replay::new(4);
    let mut content = match sent(Address::Owner, "retained note") {
        OrgEvent::MessageSent(c) => c,
        _ => unreachable!(),
    };
    content.from = Address::System;
    content.delivery_notice = Some(atm_org::DeliveryNotice {
        source_message_id: MessageId::new(3),
        reason: "unobserved".into(),
    });
    let events = [
        project(),
        adopted(),
        sent(Address::Agent(worker()), "source"),
        OrgEvent::MessageSent(content.clone()),
    ];
    for seq in [1, 2, 3, 4, 3, 4] {
        replay.feed(seq, &events[seq as usize - 1]);
    }
    guarded(replay.rt.history.folded(4)).await.unwrap();
    guarded(replay.messages.initialized()).await.unwrap();
    let inbox = replay.messages.inbox(page(), owner()).await.unwrap();
    assert_eq!(ids(&inbox), [4]);
    assert_eq!(inbox.messages[0].content, content);
}

#[tokio::test(start_paused = true)]
async fn duplicate_notice_zero_events_waits_for_the_existing_fold() {
    let mut rig = Rig::relayed().await;
    rig.org
        .execute(
            OrgCommand::SendMessage(match sent(Address::Owner, "source") {
                OrgEvent::MessageSent(c) => c,
                _ => unreachable!(),
            }),
            utc(),
        )
        .await
        .unwrap();
    rig.relay();
    rig.folded(1).await;
    let mut content = match sent(Address::Owner, "retained note") {
        OrgEvent::MessageSent(c) => c,
        _ => unreachable!(),
    };
    content.from = Address::System;
    content.delivery_notice = Some(atm_org::DeliveryNotice {
        source_message_id: MessageId::new(1),
        reason: "unobserved".into(),
    });
    rig.org
        .execute(OrgCommand::SendMessage(content.clone()), utc())
        .await
        .unwrap();
    let service = rig.messages.service().unwrap();
    let mut duplicate = Box::pin(service.execute(OrgCommand::SendMessage(content)));
    poll_pending(duplicate.as_mut()).await;
    // The FIFO snapshot proves the duplicate command was already answered;
    // its only remaining dependency must be the withheld existing fold.
    guarded(rig.org.snapshot_at()).await.unwrap();
    poll_pending(duplicate.as_mut()).await;
    rig.relay();
    assert!(guarded(duplicate).await.unwrap().is_empty());
    assert_eq!(rig.org.status().unwrap(), 2);
    assert_eq!(rig.inbox(owner(), page()).await.messages.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn unknown_kind_advances_startup_cursor() {
    let replay = Replay::new(3);
    replay.feed(1, &project());
    let unknown = Envelope {
        seq: 2,
        ts: utc(),
        kind: "future.kind".into(),
        body: serde_json::json!({"anything": [1, 2, 3]}),
    };
    replay.events.send(Arc::new(unknown)).unwrap();
    replay.feed(3, &adopted());
    // The fence is reached through a kind this build cannot read.
    guarded(replay.messages.initialized()).await.unwrap();
    guarded(replay.rt.history.folded(3)).await.unwrap();
    let inbox = replay.messages.inbox(page(), pane("%1")).await.unwrap();
    assert!(inbox.messages.is_empty());
}

#[tokio::test(start_paused = true)]
async fn requests_wait_for_the_replay_and_then_answer() {
    let replay = Replay::new(3);
    replay.feed(1, &project());
    replay.feed(2, &adopted());
    let read = spawn_polled({
        let messages = replay.messages.clone();
        async move { messages.inbox(page(), pane("%1")).await }
    });
    // The actual request reached its startup wait; it must not answer from a
    // projection that is still behind the startup fence.
    assert!(!read.is_finished());
    replay.feed(3, &sent(Address::Agent(worker()), "one"));
    assert_eq!(ids(&guarded(read).await.unwrap().unwrap()), [3]);
}

#[tokio::test(start_paused = true)]
async fn interrupted_startup_disables_mutations() {
    // The stream ends before the fence, then a gap, then an unreadable body.
    for failure in ["closed", "gap", "corrupt"] {
        let mut replay = Replay::new(4);
        replay.feed(1, &project());
        match failure {
            "closed" => {
                let (closed, _) = broadcast::channel(1);
                replay.events = closed;
            }
            "gap" => replay.feed(3, &adopted()),
            _ => {
                let corrupt = Envelope {
                    seq: 2,
                    ts: utc(),
                    kind: adopted().kind().into(),
                    body: serde_json::json!({"not": "an adoption"}),
                };
                replay.events.send(Arc::new(corrupt)).unwrap();
            }
        }
        let started = guarded(replay.messages.initialized()).await;
        assert!(
            matches!(started, Err(MessageError::Unavailable(_))),
            "{failure}"
        );
        let send = replay
            .messages
            .send(note(Address::Owner, "x"), owner())
            .await;
        assert!(
            matches!(send, Err(MessageError::Unavailable(_))),
            "{failure}"
        );
        let read = replay.messages.inbox(page(), owner()).await;
        assert!(
            matches!(read, Err(MessageError::Unavailable(_))),
            "{failure}"
        );
        let ack = replay
            .messages
            .ack(
                AckParams {
                    ids: vec![MessageId::new(1)],
                },
                owner(),
            )
            .await;
        assert!(
            matches!(ack, Err(MessageError::Unavailable(_))),
            "{failure}"
        );
        // History never passed its replay gate either.
        assert!(replay
            .rt
            .history
            .snapshot(MessageId::new(1), utc())
            .is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn replayed_undelivered_agent_messages_are_expected_by_evidence() {
    let replay = Replay::new(6);
    replay.feed(1, &project());
    replay.feed(2, &adopted());
    replay.feed(3, &sent(Address::Agent(worker()), "queued"));
    replay.feed(4, &sent(Address::Agent(worker()), "delivered"));
    replay.feed(
        5,
        &OrgEvent::MessageDeliveryAttempted {
            message_id: MessageId::new(4),
            attempt: 1,
            pane: "%1".into(),
        },
    );
    replay.feed(
        6,
        &OrgEvent::MessageDelivered {
            message_id: MessageId::new(4),
            session_id: "s1".into(),
            confirmation: "prompt_submit_exact_reference".into(),
        },
    );
    guarded(replay.messages.initialized()).await.unwrap();
    // Only what can still be delivered is known to evidence.
    assert!(replay.rt.evidence.snapshot(MessageId::new(3)).is_ok());
    assert!(replay.rt.evidence.snapshot(MessageId::new(4)).is_err());
}

#[tokio::test(start_paused = true)]
async fn request_during_an_endless_replay_times_out_typed() {
    // The fence is never reached. The paused clock runs the budget out.
    let replay = Replay::new(5);
    replay.feed(1, &project());
    let read = replay.messages.inbox(page(), owner()).await;
    assert!(matches!(read, Err(MessageError::Timeout)));
    let send = replay
        .messages
        .send(note(Address::Owner, "x"), owner())
        .await;
    assert!(matches!(send, Err(MessageError::Timeout)));
}

#[tokio::test(start_paused = true)]
async fn cancelled_service_reports_unavailable() {
    let mut rig = Rig::seeded().await;
    rig.cancel.cancel();
    guarded(&mut rig.task).await.unwrap();
    assert!(matches!(
        rig.messages.initialized().await,
        Err(MessageError::Unavailable(_))
    ));
    assert!(matches!(
        rig.messages.send(note(Address::Owner, "x"), owner()).await,
        Err(MessageError::Unavailable(_))
    ));
}

/// A capture as U8c's reserved or read-only observation returns it.
pub(super) fn capture(
    pane: &str,
    dead: bool,
    in_mode: bool,
    focused: Option<bool>,
) -> crate::control::reservation::PaneCapture {
    use crate::control::reservation::{CaptureFormat, PaneCapture, PaneInspection};
    let info = atm_tmux::PaneInfo {
        pane_id: pane.into(),
        session_name: "private".into(),
        window_index: 0,
        pane_pid: 1,
        width: 80,
        height: 24,
        is_active: false,
    };
    let inspection = PaneInspection::new(info, None, dead, in_mode, None, 0, 0);
    PaneCapture::new("screen text".into(), inspection, CaptureFormat::Plain).with_focus(focused)
}

pub(super) fn idle_session() -> super::message_history::SessionObservation {
    super::message_history::SessionObservation {
        id: "s1".into(),
        harness: atm_core::Harness::Pi,
        status: atm_core::SessionStatus::Idle,
        last_event_at: Some(utc()),
    }
}

/// An empty editor, no running turn, no dialog: what an adapter reads on an
/// idle screen.
pub(super) fn empty_editor() -> atm_core::ScreenFacts {
    let mut facts = atm_core::ScreenFacts::default();
    facts.input = atm_core::InputState::Empty;
    facts.turn = atm_core::TurnState::NotRunning;
    facts.dialog = atm_core::DialogState::None;
    facts
}

#[test]
fn converter_maps_capture_metadata_and_passes_session_and_facts_through() {
    use super::message_history::PanePresence;
    use crate::control::reservation::PaneId;
    let observation = recipient_observation(
        &capture("%7", false, false, Some(false)),
        Some(idle_session()),
        empty_editor(),
    )
    .unwrap();
    assert_eq!(observation.pane, Some(PaneId::parse("%7").unwrap()));
    assert_eq!(observation.presence, PanePresence::Alive);
    assert_eq!(observation.copy_mode, Some(false));
    assert_eq!(observation.focused, Some(false));
    assert_eq!(observation.session, Some(idle_session()));
    assert_eq!(observation.facts, empty_editor());
    // No row is no session, not a dead pane; the facts are the caller's.
    let bare = recipient_observation(
        &capture("%7", false, true, Some(true)),
        None,
        atm_core::ScreenFacts::default(),
    )
    .unwrap();
    assert_eq!(bare.session, None);
    assert_eq!(bare.presence, PanePresence::Alive);
    assert_eq!(bare.copy_mode, Some(true));
    assert_eq!(bare.focused, Some(true));
    assert_eq!(bare.facts, atm_core::ScreenFacts::default());
}

#[test]
fn converter_never_guesses_focus() {
    for focused in [None, Some(true), Some(false)] {
        let observation =
            recipient_observation(&capture("%7", false, false, focused), None, empty_editor())
                .unwrap();
        assert_eq!(observation.focused, focused);
    }
}

#[test]
fn converter_reads_a_dead_pane_as_gone() {
    use super::message_history::PanePresence;
    // The pane exists and its process has exited: positive absence of
    // anything that could take input, not an unknown.
    let observation = recipient_observation(
        &capture("%7", true, false, Some(false)),
        Some(idle_session()),
        empty_editor(),
    )
    .unwrap();
    assert_eq!(observation.presence, PanePresence::Gone);
    assert!(observation.pane.is_some());
}

#[test]
fn converter_refuses_a_capture_without_a_canonical_pane() {
    for pane in ["7", "", "%", "%7 ", "demo/worker"] {
        let refused = recipient_observation(
            &capture(pane, false, false, Some(false)),
            None,
            empty_editor(),
        );
        assert!(refused.is_err(), "{pane:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn converted_observation_drives_the_real_policy() {
    use super::message_history::HistoryUpdate;
    use atm_org::delivery::{next_delivery, DeliveryStep, WaitReason};
    // One queued message for demo/worker, bound to s1 on %1.
    let step = |shot: crate::control::reservation::PaneCapture, later: i64| {
        let replay = Replay::new(3);
        replay.feed(1, &project());
        replay.feed(2, &adopted());
        replay.feed(3, &sent(Address::Agent(worker()), "brief"));
        async move {
            guarded(replay.messages.initialized()).await.unwrap();
            let history = &replay.rt.history;
            let observed = |now| {
                let observation =
                    recipient_observation(&shot, Some(idle_session()), empty_editor()).unwrap();
                history.record(
                    HistoryUpdate::Observed {
                        recipient: worker(),
                        observation,
                    },
                    now,
                )
            };
            let now = utc() + chrono::TimeDelta::seconds(later);
            observed(utc())?;
            observed(now)?;
            let snapshot = history.snapshot(MessageId::new(3), now)?;
            Ok::<_, MessageError>(
                next_delivery(&snapshot.message, &snapshot.live, now, &replay.rt.policy).unwrap(),
            )
        }
    };
    // An idle, unfocused, empty editor for the stable-idle time: paste.
    let ready = step(capture("%1", false, false, Some(false)), 10)
        .await
        .unwrap();
    assert_eq!(ready, DeliveryStep::PasteNow { attempt: 1 });
    // The same screen with the owner on it, or with focus unknown: wait.
    for focused in [Some(true), None] {
        let waiting = step(capture("%1", false, false, focused), 10)
            .await
            .unwrap();
        assert!(
            matches!(
                waiting,
                DeliveryStep::Wait {
                    reason: WaitReason::OwnerFocused,
                    ..
                }
            ),
            "{focused:?}: {waiting:?}"
        );
    }
    // Copy mode holds, and a dead pane is on its way to a failure, not a paste.
    let copying = step(capture("%1", false, true, Some(false)), 10)
        .await
        .unwrap();
    assert!(
        matches!(
            copying,
            DeliveryStep::Wait {
                reason: WaitReason::CopyMode,
                ..
            }
        ),
        "{copying:?}"
    );
    let dead = step(capture("%1", true, false, Some(false)), 1)
        .await
        .unwrap();
    assert!(
        matches!(
            dead,
            DeliveryStep::Wait {
                reason: WaitReason::PaneGoneGrace,
                ..
            }
        ),
        "{dead:?}"
    );
    // A capture of another pane is not this recipient's observation.
    let elsewhere = step(capture("%9", false, false, Some(false)), 10).await;
    assert!(matches!(elsewhere, Err(MessageError::Conflict(_))));
}
