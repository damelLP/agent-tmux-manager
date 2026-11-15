//! The reminder's scheduler. The bookkeeping of turn ends is tested with
//! plain values. The loop runs on tokio's paused clock against the real
//! organisation actor with an in-memory log and registry events the test
//! sends itself: every test waits on a committed event and compares its time
//! with the armed limit, never on a sleep.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::eventlog::{Envelope, EventLogError};
use crate::registry::RemovalReason;
use atm_core::{ActivityDetail, AgentType, Model, SessionDomain, SessionView};
use atm_org::{
    parse_role, AgentId, MessageId, OrgEvent, TaskActor, TaskCreated, TaskId, TaskKind,
    TaskMeasure, TaskState,
};
use chrono::TimeDelta;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const DELAY: i64 = 60;
/// Virtual time: longer than any wait a test arms, so it only ends a hang.
const GUARD: Duration = Duration::from_secs(3600);

fn settings() -> ReminderSettings {
    ReminderSettings::default()
}

fn t0() -> DateTime<Utc> {
    "2026-11-15T12:00:00Z".parse().unwrap()
}

fn at(secs: i64) -> DateTime<Utc> {
    t0() + TimeDelta::seconds(secs)
}

fn agent(name: &str) -> AgentId {
    format!("p/{name}").parse().unwrap()
}

fn sid(text: &str) -> SessionId {
    SessionId::new(text)
}

/// The registry's update for `session` in `status`, showing `activity`.
fn row(session: &str, status: SessionStatus, activity: Option<&str>) -> SessionEvent {
    let mut domain = SessionDomain::new(sid(session), AgentType::GeneralPurpose, Model::Unknown);
    domain.status = status;
    domain.current_activity = activity.map(ActivityDetail::with_context);
    SessionEvent::Updated {
        session: Box::new(SessionView::from_domain(&domain)),
    }
}

fn working(session: &str) -> SessionEvent {
    row(session, SessionStatus::Working, None)
}

fn idle(session: &str) -> SessionEvent {
    row(session, SessionStatus::Idle, None)
}

fn armed(ended: i64) -> Pending {
    Pending {
        ended_at: at(ended),
        wake: at(ended + DELAY),
    }
}

// ---------------------------------------------------------------- the watch

#[test]
fn a_turn_end_arms_one_wait_from_the_moment_it_is_seen() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    assert_eq!(watch.next_wake(), None);
    watch.observe(&idle("s1"), &settings(), at(5));
    assert_eq!(watch.pending.get(&sid("s1")), Some(&armed(5)));
    assert_eq!(watch.next_wake(), Some(at(5 + DELAY)));
}

#[test]
fn a_dialog_that_ends_in_idle_is_a_turn_end() {
    let mut watch = Watch::default();
    watch.observe(
        &row("s1", SessionStatus::AttentionNeeded, None),
        &settings(),
        at(0),
    );
    watch.observe(&idle("s1"), &settings(), at(3));
    assert_eq!(watch.pending.get(&sid("s1")), Some(&armed(3)));
}

#[test]
fn an_idle_row_whose_turn_was_not_seen_arms_nothing() {
    let mut watch = Watch::default();
    watch.observe(&idle("s1"), &settings(), at(0));
    watch.observe(&idle("s1"), &settings(), at(1));
    assert_eq!(watch.next_wake(), None);
}

#[test]
fn later_idle_updates_keep_the_turn_end() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    watch.observe(&idle("s1"), &settings(), at(5));
    // A status line, a late child's stop, the harness's own idle notice.
    watch.observe(&idle("s1"), &settings(), at(6));
    watch.observe(&idle("s1"), &settings(), at(65));
    assert_eq!(watch.pending.get(&sid("s1")), Some(&armed(5)));
}

#[test]
fn a_turn_or_a_dialog_inside_the_wait_drops_it() {
    for status in [SessionStatus::Working, SessionStatus::AttentionNeeded] {
        let mut watch = Watch::default();
        watch.observe(&working("s1"), &settings(), at(0));
        watch.observe(&idle("s1"), &settings(), at(5));
        watch.observe(&row("s1", status, None), &settings(), at(30));
        assert_eq!(watch.next_wake(), None, "{status:?}");
        // The next end starts a fresh wait.
        watch.observe(&idle("s1"), &settings(), at(40));
        assert_eq!(watch.pending.get(&sid("s1")), Some(&armed(40)));
    }
}

#[test]
fn background_work_on_the_idle_row_drops_the_wait() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    // The registry publishes the turn end, then the same idle row with what
    // still runs in the background.
    watch.observe(&idle("s1"), &settings(), at(5));
    watch.observe(
        &row("s1", SessionStatus::Idle, Some("1 bg task")),
        &settings(),
        at(5),
    );
    assert_eq!(watch.next_wake(), None);
    // The turn that the finished background work starts ends quiet.
    watch.observe(&working("s1"), &settings(), at(20));
    watch.observe(&idle("s1"), &settings(), at(25));
    assert_eq!(watch.pending.get(&sid("s1")), Some(&armed(25)));
}

#[test]
fn a_turn_that_ends_with_background_work_showing_arms_nothing() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    watch.observe(
        &row("s1", SessionStatus::Idle, Some("2 scheduled")),
        &settings(),
        at(5),
    );
    assert_eq!(watch.next_wake(), None);
}

#[test]
fn a_removed_session_is_forgotten() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    watch.observe(&idle("s1"), &settings(), at(5));
    let removed = SessionEvent::Removed {
        session_id: sid("s1"),
        reason: RemovalReason::SessionEnded,
    };
    watch.observe(&removed, &settings(), at(6));
    assert_eq!(watch.next_wake(), None);
    // Its status is forgotten too: a row with that id is new.
    watch.observe(&working("s1"), &settings(), at(7));
    watch.observe(&removed, &settings(), at(8));
    watch.observe(&idle("s1"), &settings(), at(9));
    assert_eq!(watch.next_wake(), None);
}

#[test]
fn missed_events_forget_everything_so_no_turn_end_is_made_up() {
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), at(0));
    watch.observe(&working("s2"), &settings(), at(0));
    watch.observe(&idle("s2"), &settings(), at(5));
    watch.forget();
    assert_eq!(watch.next_wake(), None);
    watch.observe(&idle("s1"), &settings(), at(6));
    assert_eq!(watch.next_wake(), None);
}

#[test]
fn the_earliest_wait_is_the_next_wake() {
    let mut watch = Watch::default();
    for session in ["s1", "s2", "s3"] {
        watch.observe(&working(session), &settings(), at(0));
    }
    watch.observe(&idle("s2"), &settings(), at(9));
    watch.observe(&idle("s1"), &settings(), at(4));
    watch.observe(&idle("s3"), &settings(), at(7));
    assert_eq!(watch.next_wake(), Some(at(4 + DELAY)));
}

#[test]
fn a_delay_the_calendar_cannot_hold_arms_nothing() {
    let mut watch = Watch::default();
    let never = ReminderSettings {
        delay: Duration::MAX,
    };
    watch.observe(&working("s1"), &never, at(0));
    watch.observe(&idle("s1"), &never, at(5));
    assert_eq!(watch.next_wake(), None);
}

// ------------------------------------------------------------------ the rig

/// Holds the first append of one kind until the test lets it go, so the
/// scheduler can be kept busy at a known point.
#[derive(Default)]
struct Gate {
    kind: Mutex<Option<&'static str>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// The organisation actor over a log kept in memory.
struct Rig {
    org: OrgHandle,
    gate: Arc<Gate>,
    log: Arc<Mutex<Vec<Envelope>>>,
    committed: broadcast::Receiver<Arc<Envelope>>,
    rows: broadcast::Sender<SessionEvent>,
    started: tokio::time::Instant,
    cancel: CancellationToken,
}

/// An actor that starts from `state`, whose log already holds `log`. An
/// append of `fail` fails, as a disk that stops taking writes would.
fn actor(
    state: OrgState,
    log: Arc<Mutex<Vec<Envelope>>>,
    fail: Option<&'static str>,
    gate: Arc<Gate>,
    cancel: CancellationToken,
) -> OrgHandle {
    let (requests, receive) = mpsc::channel(64);
    let seq = log.lock().unwrap().len() as u64;
    let (health, status) = watch::channel(Ok(seq));
    let (committed, _) = broadcast::channel(1024);
    tokio::spawn(super::super::actor::run(
        state,
        receive,
        health,
        committed.clone(),
        cancel,
        move |mut envelope: Envelope| {
            let log = log.clone();
            let gate = gate.clone();
            async move {
                let held = gate
                    .kind
                    .lock()
                    .unwrap()
                    .take_if(|kind| *kind == envelope.kind);
                if held.is_some() {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
                if fail == Some(envelope.kind.as_str()) {
                    return Err(EventLogError::Io {
                        path: "memory".into(),
                        source: std::io::Error::other("the log takes no more writes"),
                    });
                }
                let mut log = log.lock().unwrap();
                envelope.seq = log.len() as u64 + 1;
                log.push(envelope.clone());
                Ok(envelope.seq)
            }
        },
    ));
    OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    }
}

impl Rig {
    /// Project `p`; the lead adopted in `%1` on session `lead-1`; the builder
    /// up from a role whose manager is the lead, in `%2` on `s1`.
    async fn new() -> Self {
        Self::over(OrgState::default(), Arc::default(), None).await
    }

    async fn over(
        state: OrgState,
        log: Arc<Mutex<Vec<Envelope>>>,
        fail: Option<&'static str>,
    ) -> Self {
        let fresh = log.lock().unwrap().is_empty();
        let cancel = CancellationToken::new();
        let gate = Arc::<Gate>::default();
        let org = actor(state, log.clone(), fail, gate.clone(), cancel.clone());
        let rig = Self {
            gate,
            committed: org.committed.subscribe(),
            org,
            log,
            rows: broadcast::channel(64).0,
            started: tokio::time::Instant::now(),
            cancel,
        };
        if fresh {
            rig.run(OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: "/p".into(),
                move_root: false,
            })
            .await;
            rig.run(OrgCommand::Adopt {
                agent_id: agent("lead"),
                pane: "%1".into(),
                session_id: Some(sid("lead-1")),
            })
            .await;
            rig.seat("build", "%2", "s1").await;
        }
        rig
    }

    /// A Ready agent from a role that names the lead as its manager.
    async fn seat(&self, name: &str, pane: &str, session: &str) {
        let role = parse_role(name, "+++\nharness='pi'\nmanager='lead'\n+++\n").unwrap();
        self.run(OrgCommand::Up {
            agent_id: agent(name),
            role,
        })
        .await;
        self.run(OrgCommand::PaneCreated {
            agent_id: agent(name),
            pane: pane.into(),
        })
        .await;
        self.run(OrgCommand::SessionBound {
            agent_id: agent(name),
            session_id: sid(session),
        })
        .await;
    }

    /// The test's wall clock: `t0` plus tokio's (paused) time.
    fn now(&self) -> DateTime<Utc> {
        t0() + TimeDelta::from_std(self.started.elapsed()).unwrap()
    }

    fn clock(&self) -> impl Fn() -> DateTime<Utc> + Send + 'static {
        let started = self.started;
        move || t0() + TimeDelta::from_std(started.elapsed()).unwrap()
    }

    /// Start the scheduler on this rig's registry events.
    fn schedule(&self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(
            self.org.clone(),
            self.rows.subscribe(),
            settings(),
            self.clock(),
            self.cancel.clone(),
        ))
    }

    async fn run(&self, command: OrgCommand) -> Vec<Envelope> {
        self.org.execute(command, self.now()).await.unwrap()
    }

    /// A work task `creator` made, assigned to `to`.
    async fn task(&self, creator: Address, to: &str) -> TaskId {
        let by = TaskActor {
            who: creator.clone(),
            lead: true,
        };
        let made = self
            .run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
                title: "Do the work".into(),
                kind: TaskKind::Work,
                creator,
                parent: None,
                judges: None,
                measure: TaskMeasure::Owner,
                budget: None,
                files: vec![],
                rests_on: None,
            })))
            .await;
        let task_id: TaskId = format!("t{}", made[0].seq).parse().unwrap();
        self.run(OrgCommand::Task(TaskCommand::Assign {
            task_id: task_id.clone(),
            agent_id: agent(to),
            by,
        }))
        .await;
        task_id
    }

    async fn set(&self, task: &TaskId, state: TaskState, reason: Option<&str>, who: &str) {
        self.run(OrgCommand::Task(TaskCommand::SetState {
            task_id: task.clone(),
            state,
            reason: reason.map(Into::into),
            by: TaskActor {
                who: Address::Agent(agent(who)),
                lead: false,
            },
        }))
        .await;
    }

    /// One registry update, read by the scheduler before this returns: it
    /// stamps a turn end with the time it reads it at. The scheduler reads
    /// and books a row in one step, so an empty channel means it is booked.
    async fn report(&self, row: SessionEvent) {
        self.rows.send(row).unwrap();
        read(&self.rows).await;
    }

    /// One whole turn of `session`, ending now.
    async fn turn(&self, session: &str) {
        self.report(working(session)).await;
        self.report(idle(session)).await;
    }

    /// Let virtual time pass. The paused clock moves from timer to timer, so
    /// a wait that ends on the way is settled at its own limit.
    async fn pass(&self, secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    /// The next committed event of `kind`, with its body decoded.
    async fn next(&mut self, kind: &str) -> (Arc<Envelope>, OrgEvent) {
        let found = tokio::time::timeout(GUARD, async {
            loop {
                let envelope = self.committed.recv().await.unwrap();
                if envelope.kind == kind {
                    return envelope;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} was committed"));
        let event = OrgEvent::from_parts(&found.kind, &found.body)
            .unwrap()
            .unwrap();
        (found, event)
    }

    /// The next message the daemon sent, as (id, content, when).
    async fn sent(&mut self) -> (MessageId, MessageContent, DateTime<Utc>) {
        let (envelope, event) = self.next("msg.sent").await;
        let OrgEvent::MessageSent(content) = event else {
            panic!("{event:?}")
        };
        (MessageId::new(envelope.seq), content, envelope.ts)
    }

    /// The sequence number of the last event in the log.
    fn last_seq(&self) -> u64 {
        self.log.lock().unwrap().len() as u64
    }

    /// Confirm `message` as typed into `session`, so it is no longer on its way.
    async fn deliver(&self, message: MessageId, session: &str) {
        self.run(OrgCommand::DeliveryAttempted {
            message_id: message,
            attempt: 1,
            pane: "%2".into(),
        })
        .await;
        self.run(OrgCommand::Delivered {
            message_id: message,
            session_id: sid(session),
            confirmation: "prompt token".into(),
        })
        .await;
    }

    /// How many events of `kind` the log holds.
    fn count(&self, kind: &str) -> usize {
        let log = self.log.lock().unwrap();
        log.iter().filter(|envelope| envelope.kind == kind).count()
    }

    async fn state(&self) -> OrgState {
        self.org.snapshot().await.unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Until every receiver has read what was sent.
async fn read(rows: &broadcast::Sender<SessionEvent>) {
    while !rows.is_empty() {
        tokio::task::yield_now().await;
    }
}

fn reminder_text(task: &TaskId, state: &str, told: &str) -> String {
    format!(
        "Your turn ended and task {task} (\"Do the work\") is still {state}.\n\
         Before you stop, say where it stands: atm task submit {task}, or atm task set-state \
         {task} waiting --reason '...' (or blocked). If you are still working, carry on; if you \
         have updated it since, ignore this. You get this once: if your next turn also ends this \
         way, {told} is told."
    )
}

// ----------------------------------------------------------------- the loop

#[tokio::test(start_paused = true)]
async fn a_silent_turn_end_is_answered_after_the_delay_audit_first() {
    let mut rig = Rig::new().await;
    let task = rig.task(Address::Agent(agent("lead")), "build").await;
    rig.schedule();
    rig.pass(7).await;
    rig.turn("s1").await;

    let (reminded, event) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(7 + DELAY), "not before the wait is over");
    assert_eq!(
        event,
        OrgEvent::TaskReminded(atm_org::TaskReminded {
            task_id: task.clone(),
            agent_id: agent("build"),
        })
    );
    let (_, note, when) = rig.sent().await;
    assert_eq!(
        rig.last_seq(),
        reminded.seq + 1,
        "the audit event comes first: it is the latch"
    );
    assert_eq!(when, at(7 + DELAY));
    assert_eq!(
        note,
        MessageContent {
            from: Address::System,
            to: Address::Agent(agent("build")),
            kind: MessageKind::Note,
            body: reminder_text(&task, "assigned", "p/lead"),
            attachments: vec![],
            task: Some(task),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_starts_inside_the_wait_gets_no_reminder() {
    let mut rig = Rig::new().await;
    rig.task(Address::Agent(agent("lead")), "build").await;
    rig.schedule();
    rig.turn("s1").await;
    rig.pass(59).await;
    // A long turn: it runs past the first wait's limit and ends at 200 s.
    rig.report(working("s1")).await;
    rig.pass(141).await;
    rig.report(idle("s1")).await;

    let (reminded, _) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(200 + DELAY));
    assert_eq!(rig.count("task.reminded"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_task_that_says_why_inside_the_wait_gets_no_reminder() {
    let mut rig = Rig::new().await;
    let task = rig.task(Address::Agent(agent("lead")), "build").await;
    rig.schedule();
    rig.turn("s1").await;
    rig.pass(30).await;
    rig.set(&task, TaskState::Waiting, Some("an answer"), "build")
        .await;
    // Well past the first limit, the agent takes the task up and stops again.
    rig.pass(270).await;
    rig.set(&task, TaskState::Working, None, "build").await;
    rig.turn("s1").await;

    let (reminded, _) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(300 + DELAY));
    assert_eq!(rig.count("task.reminded"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_next_silent_turn_end_is_told_to_the_creator() {
    let mut rig = Rig::new().await;
    let task = rig.task(Address::Agent(agent("lead")), "build").await;
    rig.schedule();
    rig.turn("s1").await;
    let (message, _, reminded_at) = rig.sent().await;
    // The reminder is typed and starts a turn that ends silent too.
    rig.pass(11).await;
    rig.deliver(message, "s1").await;
    rig.pass(20).await;
    rig.turn("s1").await;
    let ended = DELAY + 31;

    let (flagged, event) = rig.next("task.flagged").await;
    assert_eq!(flagged.ts, at(ended + DELAY));
    assert_eq!(
        event,
        OrgEvent::TaskFlagged(atm_org::TaskFlagged {
            task_id: task.clone(),
            to: Address::Agent(agent("lead")),
        })
    );
    let (_, note, _) = rig.sent().await;
    assert_eq!(
        note,
        MessageContent {
            from: Address::System,
            to: Address::Agent(agent("lead")),
            kind: MessageKind::Note,
            body: format!(
                "Task {task} (\"Do the work\"): p/build ended two turns without moving it from \
                 assigned; reminded at {}. atm task get {task}",
                reminded_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            attachments: vec![],
            task: Some(task.clone()),
        }
    );
    assert_eq!(rig.count("task.reminded"), 1);

    // A third silent turn end: the task is flagged and nothing more is said
    // about it. A second task shows that this turn end was judged.
    let other = rig.task(Address::Owner, "build").await;
    rig.pass(5).await;
    rig.turn("s1").await;
    let (reminded, event) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(ended + DELAY + 5 + DELAY));
    assert!(matches!(event, OrgEvent::TaskReminded(fact) if fact.task_id == other));
    assert_eq!(rig.count("task.flagged"), 1);
}

#[tokio::test(start_paused = true)]
async fn mail_on_its_way_holds_the_reminder_until_it_is_typed() {
    let mut rig = Rig::new().await;
    rig.task(Address::Agent(agent("lead")), "build").await;
    let mail = rig
        .run(OrgCommand::SendMessage(MessageContent {
            from: Address::Agent(agent("lead")),
            to: Address::Agent(agent("build")),
            kind: MessageKind::Note,
            body: "An answer".into(),
            attachments: vec![],
            task: None,
        }))
        .await;
    // A second seat with a task of its own, and no mail.
    rig.seat("other", "%3", "s3").await;
    rig.task(Address::Agent(agent("lead")), "other").await;
    rig.committed = rig.org.committed.subscribe();
    rig.schedule();
    rig.turn("s1").await;
    rig.pass(30).await;
    rig.turn("s3").await;
    // At 60 s the builder's mail is still queued: look again one delay
    // later, at 120 s. The other seat's wake at 90 s does not move that. The
    // mail fails for good at 100 s, which starts no turn.
    rig.pass(70).await;
    rig.run(OrgCommand::DeliveryFailed {
        message_id: MessageId::new(mail[0].seq),
        reason: "given up".into(),
        final_failure: true,
    })
    .await;

    let (first, event) = rig.next("task.reminded").await;
    assert_eq!(first.ts, at(30 + DELAY));
    assert!(matches!(event, OrgEvent::TaskReminded(fact) if fact.agent_id == agent("other")));
    let (second, event) = rig.next("task.reminded").await;
    assert_eq!(second.ts, at(2 * DELAY));
    assert!(matches!(event, OrgEvent::TaskReminded(fact) if fact.agent_id == agent("build")));
}

/// Registry rows are read before a wake is taken. While the scheduler is
/// busy writing one agent's reminder, the wakes of many seats fall due and
/// each seat starts a new turn. When the scheduler looks again, every seat
/// has both a row to read and a wake that is over: reading the rows first
/// drops every wait. A scheduler that took a wake first even once would
/// remind the seats still waiting, and with this many seats it would.
#[tokio::test(start_paused = true)]
async fn a_turn_that_started_while_the_scheduler_was_busy_is_read_before_its_wake() {
    const SEATS: usize = 32;
    let mut rig = Rig::new().await;
    let lead = Address::Agent(agent("lead"));
    rig.task(lead.clone(), "build").await;
    let seats: Vec<String> = (0..SEATS).map(|seat| format!("seat{seat}")).collect();
    for (number, seat) in seats.iter().enumerate() {
        rig.seat(seat, &format!("%{}", 10 + number), seat).await;
        rig.task(lead.clone(), seat).await;
    }
    rig.seat("last", "%9", "last").await;
    rig.task(lead, "last").await;
    rig.committed = rig.org.committed.subscribe();
    *rig.gate.kind.lock().unwrap() = Some("task.reminded");
    rig.schedule();
    rig.turn("s1").await;
    rig.pass(3).await;
    for seat in &seats {
        rig.turn(seat).await;
    }

    // The builder's wait is over at 60 s; its audit event is held in the log.
    rig.gate.entered.notified().await;
    assert_eq!(rig.now(), at(DELAY));
    // The seats' waits are over at 63 s. Each starts a turn at 64 s, which
    // the busy scheduler cannot read yet.
    rig.pass(4).await;
    for seat in &seats {
        rig.rows.send(working(seat)).unwrap();
    }
    rig.gate.release.notify_one();
    // One more seat's turn end shows that the scheduler has moved on.
    rig.turn("last").await;

    let (first, event) = rig.next("task.reminded").await;
    assert_eq!(first.ts, at(DELAY));
    assert!(matches!(event, OrgEvent::TaskReminded(fact) if fact.agent_id == agent("build")));
    let (second, event) = rig.next("task.reminded").await;
    assert!(
        matches!(&event, OrgEvent::TaskReminded(fact) if fact.agent_id == agent("last")),
        "{event:?}"
    );
    assert_eq!(second.ts, at(DELAY + 4 + DELAY));
    assert_eq!(rig.count("task.reminded"), 2);
}

#[tokio::test]
async fn a_task_that_moved_since_the_snapshot_gets_no_event_and_no_note() {
    let rig = Rig::new().await;
    let lead = Address::Agent(agent("lead"));
    let moved = rig.task(lead.clone(), "build").await;
    let marked = rig.task(lead, "build").await;
    rig.run(OrgCommand::Task(TaskCommand::Reminded {
        task_id: marked.clone(),
        agent_id: agent("build"),
    }))
    .await;
    // What the rule judged: one task to remind of, one to flag.
    let judged = rig.state().await;
    let builder = judged.agent(&agent("build")).unwrap();
    // Before the answer is written, the agent hands the first in and says
    // why it stopped on the second, which clears its mark.
    rig.run(OrgCommand::Task(TaskCommand::Submit {
        task_id: moved.clone(),
        what: atm_org::TaskSubmission::Result,
        files: vec![],
        by: TaskActor {
            who: Address::Agent(agent("build")),
            lead: false,
        },
    }))
    .await;
    rig.set(&marked, TaskState::Waiting, Some("an answer"), "build")
        .await;
    let before = rig.last_seq();

    act(&rig.org, &judged, builder, &[moved], &[marked], rig.now()).await;
    assert_eq!(rig.last_seq(), before, "the task rules refuse both");
}

#[tokio::test(start_paused = true)]
async fn an_agent_that_left_the_session_or_is_not_ready_is_left_alone() {
    let mut rig = Rig::new().await;
    rig.task(Address::Agent(agent("lead")), "build").await;
    rig.task(Address::Agent(agent("lead")), "lead").await;
    rig.schedule();
    // The builder is cleared after its turn: the turn end belongs to a
    // session the agent has left.
    rig.turn("s1").await;
    rig.pass(10).await;
    rig.run(OrgCommand::Clear {
        agent_id: agent("build"),
    })
    .await;
    rig.run(OrgCommand::SessionBound {
        agent_id: agent("build"),
        session_id: sid("s2"),
    })
    .await;
    // The lead is stuck when its wait is over.
    rig.turn("lead-1").await;
    rig.run(OrgCommand::Stuck {
        agent_id: agent("lead"),
        reason: "a dialog".into(),
    })
    .await;
    // A session no agent is bound to, and the builder's new one, end a turn.
    rig.pass(100).await;
    rig.turn("nobody").await;
    rig.turn("s2").await;

    let (reminded, event) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(110 + DELAY));
    assert!(matches!(event, OrgEvent::TaskReminded(fact) if fact.agent_id == agent("build")));
    assert_eq!(rig.count("task.reminded"), 1);
}

#[tokio::test(start_paused = true)]
async fn several_tasks_get_one_reminder_and_one_note_per_addressee() {
    let mut rig = Rig::new().await;
    let lead = Address::Agent(agent("lead"));
    let first = rig.task(lead.clone(), "build").await;
    let second = rig.task(Address::Owner, "build").await;
    // The builder's own task and the daemon's go to its manager.
    let own = rig.task(Address::Agent(agent("build")), "build").await;
    let systems = rig.task(Address::System, "build").await;
    rig.set(&second, TaskState::Working, None, "build").await;
    rig.schedule();
    rig.turn("s1").await;

    let (message, note, _) = rig.sent().await;
    assert_eq!(rig.count("task.reminded"), 4);
    assert_eq!(note.task, Some(first.clone()));
    assert_eq!(
        note.body,
        format!(
            "Your turn ended and task {first} (\"Do the work\") is still assigned.\n\
             Your turn ended and task {second} (\"Do the work\") is still working.\n\
             Your turn ended and task {own} (\"Do the work\") is still assigned.\n\
             Your turn ended and task {systems} (\"Do the work\") is still assigned.\n\
             Before you stop, say where it stands: atm task submit <task>, or atm task \
             set-state <task> waiting --reason '...' (or blocked). If you are still working, \
             carry on; if you have updated it since, ignore this. You get this once: if your \
             next turn also ends this way, its creator is told."
        )
    );
    assert_eq!(rig.count("msg.sent"), 1);

    rig.deliver(message, "s1").await;
    rig.pass(1).await;
    rig.turn("s1").await;
    let (_, to_lead, _) = rig.sent().await;
    let (_, to_owner, _) = rig.sent().await;
    assert_eq!(rig.count("task.flagged"), 4);
    assert_eq!(rig.count("msg.sent"), 3);
    assert_eq!(to_lead.to, lead);
    assert_eq!(to_lead.task, Some(first.clone()));
    let lines: Vec<&str> = to_lead.body.lines().collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    for (line, task) in lines.iter().zip([&first, &own, &systems]) {
        assert!(line.starts_with(&format!("Task {task} (")), "{line}");
    }
    assert_eq!(to_owner.to, Address::Owner);
    assert_eq!(to_owner.task, Some(second.clone()));
    assert!(to_owner.body.starts_with(&format!("Task {second} (")));
    assert!(to_owner.body.contains("from working;"));
}

#[tokio::test(start_paused = true)]
async fn a_manager_that_is_missing_or_the_agent_itself_means_the_owner() {
    let mut rig = Rig::new().await;
    // The lead was adopted and has no role; `solo` names itself, `stray` an
    // agent that does not exist.
    for (name, manager, pane) in [("solo", "solo", "%3"), ("stray", "ghost", "%4")] {
        let role = parse_role(
            name,
            &format!("+++\nharness='pi'\nmanager='{manager}'\n+++\n"),
        );
        rig.run(OrgCommand::Up {
            agent_id: agent(name),
            role: role.unwrap(),
        })
        .await;
        rig.run(OrgCommand::PaneCreated {
            agent_id: agent(name),
            pane: pane.into(),
        })
        .await;
        rig.run(OrgCommand::SessionBound {
            agent_id: agent(name),
            session_id: sid(name),
        })
        .await;
    }
    for name in ["lead", "solo", "stray"] {
        rig.task(Address::System, name).await;
    }
    rig.schedule();
    for (name, session) in [("lead", "lead-1"), ("solo", "solo"), ("stray", "stray")] {
        rig.turn(session).await;
        let (message, note, _) = rig.sent().await;
        assert_eq!(note.to, Address::Agent(agent(name)));
        assert!(note.body.ends_with("owner is told."), "{}", note.body);
        rig.deliver(message, session).await;
        rig.pass(1).await;
        rig.turn(session).await;
        let (_, flag, _) = rig.sent().await;
        assert_eq!(flag.to, Address::Owner, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_restart_never_reminds_twice_and_the_flag_still_follows() {
    let mut rig = Rig::new().await;
    rig.task(Address::Agent(agent("lead")), "build").await;
    let scheduler = rig.schedule();
    rig.turn("s1").await;
    let (message, _, _) = rig.sent().await;
    rig.deliver(message, "s1").await;
    // A turn end the old daemon saw is lost with it.
    rig.turn("s1").await;
    rig.pass(30).await;
    let state = rig.state().await;
    rig.cancel.cancel();
    scheduler.await.unwrap();

    let mut after = Rig::over(state, rig.log.clone(), None).await;
    after.started = rig.started;
    after.schedule();
    after.pass(500).await;
    // The first idle row after a start has no turn before it.
    after.report(idle("s1")).await;
    after.pass(500).await;
    after.turn("s1").await;
    let (flagged, _) = after.next("task.flagged").await;
    assert_eq!(flagged.ts, at(DELAY + 30 + 1000 + DELAY));
    assert_eq!(after.count("task.reminded"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_send_that_fails_loses_the_reminder_and_never_doubles_it() {
    let rig = Rig::new().await;
    let task = rig.task(Address::Agent(agent("lead")), "build").await;
    // The same log, on a disk that takes the audit event and then no more.
    let mut failing = Rig::over(rig.state().await, rig.log.clone(), Some("msg.sent")).await;
    let mut health = failing.org.health.clone();
    failing.schedule();
    failing.turn("s1").await;
    failing.next("task.reminded").await;
    health.wait_for(Result::is_err).await.unwrap();
    assert_eq!(failing.count("task.reminded"), 1);
    assert_eq!(failing.count("msg.sent"), 0);

    // After the restart the task is marked, so the next silent turn end is
    // told to the creator: a person hears of the lost reminder.
    let state = OrgState::default();
    let replayed = failing
        .log
        .lock()
        .unwrap()
        .iter()
        .fold(state, |mut state, e| {
            let event = OrgEvent::from_parts(&e.kind, &e.body).unwrap().unwrap();
            state.apply(e.seq, e.ts, &event).unwrap();
            state
        });
    assert!(replayed.task(&task).unwrap().reminded_at.is_some());
    let mut after = Rig::over(replayed, failing.log.clone(), None).await;
    after.started = failing.started;
    after.schedule();
    after.pass(100).await;
    after.turn("s1").await;
    after.next("task.flagged").await;
    assert_eq!(after.count("task.reminded"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_scheduler_stops_with_its_token_and_with_the_registry() {
    let rig = Rig::new().await;
    let by_token = rig.schedule();
    rig.cancel.cancel();
    by_token.await.unwrap();

    let (rows, receiver) = broadcast::channel(4);
    let by_registry = tokio::spawn(run(
        rig.org.clone(),
        receiver,
        settings(),
        rig.clock(),
        CancellationToken::new(),
    ));
    drop(rows);
    by_registry.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn missed_registry_events_cost_the_pending_waits_only() {
    let mut rig = Rig::new().await;
    rig.task(Address::Agent(agent("lead")), "build").await;
    let (rows, receiver) = broadcast::channel(2);
    tokio::spawn(run(
        rig.org.clone(),
        receiver,
        settings(),
        rig.clock(),
        rig.cancel.clone(),
    ));
    rows.send(working("s1")).unwrap();
    read(&rows).await;
    // A channel of two: the third unread update pushes the turn end out.
    for _ in 0..3 {
        rows.send(idle("s1")).unwrap();
    }
    rig.pass(500).await;
    rows.send(working("s1")).unwrap();
    rows.send(idle("s1")).unwrap();
    let (reminded, _) = rig.next("task.reminded").await;
    assert_eq!(reminded.ts, at(500 + DELAY));
}

// ----------------------------------------------------------------- the texts

#[tokio::test]
async fn long_titles_are_cut_and_many_tasks_wait_for_the_next_turn_end() {
    let rig = Rig::new().await;
    let by = TaskActor {
        who: Address::Owner,
        lead: false,
    };
    let made = rig
        .run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
            title: "é".repeat(200),
            kind: TaskKind::Work,
            creator: Address::Owner,
            parent: None,
            judges: None,
            measure: TaskMeasure::Owner,
            budget: None,
            files: vec![],
            rests_on: None,
        })))
        .await;
    let long: TaskId = format!("t{}", made[0].seq).parse().unwrap();
    rig.run(OrgCommand::Task(TaskCommand::Assign {
        task_id: long.clone(),
        agent_id: agent("build"),
        by,
    }))
    .await;
    for _ in 0..MAX_TASKS + 4 {
        rig.task(Address::Owner, "build").await;
    }
    let state = rig.state().await;
    let builder = state.agent(&agent("build")).unwrap();
    let held: Vec<&atm_org::Task> = state.tasks().collect();
    let note = reminder_note(&state, builder, &held[..1]).unwrap();
    let title = format!("(\"{}…\")", "é".repeat(TITLE_CHARS));
    assert!(note.body.contains(&title), "{}", note.body);

    // The policy names every due task; one wake answers the first MAX_TASKS.
    let mut committed = rig.org.committed.subscribe();
    let mut watch = Watch::default();
    watch.observe(&working("s1"), &settings(), rig.now());
    watch.observe(&idle("s1"), &settings(), rig.now());
    settle(
        &rig.org,
        &mut watch,
        &settings(),
        rig.now() + TimeDelta::seconds(DELAY),
    )
    .await;
    assert_eq!(rig.count("task.reminded"), MAX_TASKS);
    assert_eq!(watch.next_wake(), None, "the turn end is used up");
    let sent = loop {
        let envelope = committed.recv().await.unwrap();
        if envelope.kind == "msg.sent" {
            break envelope;
        }
    };
    let lines = sent.body["body"].as_str().unwrap().lines().count();
    assert_eq!(lines, MAX_TASKS + 1);
}

// ---------------------------------------------------------------- the config

#[test]
fn the_reminder_section_defaults_to_on_after_a_minute() {
    let on = |secs| {
        Ok(Some(ReminderSettings {
            delay: Duration::from_secs(secs),
        }))
    };
    for text in [
        "",
        "[notify]\ncommand=['true']\n",
        "[reminder]\n",
        "[reminder]\nenabled=true\n",
        // Another section's mistake is that section's.
        "[delivery]\nunknown=1\n[reminder]\ndelay_secs=60\n",
    ] {
        assert_eq!(parse_reminder_settings(text), on(60), "{text:?}");
    }
    assert_eq!(parse_reminder_settings("[reminder]\ndelay_secs=5"), on(5));
    assert_eq!(parse_reminder_settings("[reminder]\ndelay_secs=1"), on(1));
}

#[test]
fn the_reminder_section_can_turn_reminders_off() {
    for text in [
        "[reminder]\nenabled=false",
        "[reminder]\nenabled=false\ndelay_secs=0",
    ] {
        assert_eq!(parse_reminder_settings(text), Ok(None), "{text:?}");
    }
}

#[test]
fn an_invalid_reminder_section_is_an_error_and_never_a_guess() {
    for text in [
        "[reminder]\nunknown='SECRET'",
        "[reminder]\ndelay_secs='SECRET'",
        "[reminder]\ndelay_secs=-1",
        "[reminder]\ndelay_secs=0",
        "[reminder]\nenabled='SECRET'",
        "[reminder",
        "reminder='SECRET'",
    ] {
        let error = parse_reminder_settings(text).unwrap_err();
        assert!(error.contains("[reminder]"), "{text:?}: {error}");
        assert!(!error.contains("SECRET"), "{text:?}: {error}");
    }
}

// ------------------------------------------------------------ a real capture

/// One whole Claude Code session replayed through the registry, as the
/// connection does for each hook call (translate, then apply with the
/// adapter's context), with the watch reading what the registry publishes.
/// Status lines are left out: they change neither status nor activity.
///
/// Captured: Claude Code 2.1.310, 2026-10-24, the daemon's interleaved
/// fixture (see `tests/server_integration.rs`); the line numbers are its.
#[tokio::test]
async fn a_captured_claude_session_arms_a_wait_at_its_turn_ends_only() {
    let registry = crate::registry::spawn_registry();
    let mut rows = registry.subscribe();
    let mut watch = Watch::default();
    let mut waits = Vec::new();
    let capture = include_str!("../../tests/fixtures/claude_tui_interleaved_2.1.310.jsonl");
    for (index, line) in capture.lines().enumerate() {
        let mut call: serde_json::Value = serde_json::from_str(line).unwrap();
        if call["kind"] != "hook" {
            continue;
        }
        let mut payload = call["payload"].take();
        // atm-hook adds the harness's pid; any live one will do.
        payload["pid"] = std::process::id().into();
        let raw: atm_claude_adapter::RawHookEvent = serde_json::from_value(payload).unwrap();
        let Some(event) = raw.to_lifecycle_event() else {
            continue;
        };
        let session = raw.session_id();
        registry
            .apply_lifecycle_event_with_context(
                session.clone(),
                event,
                atm_core::Harness::ClaudeCode,
                raw.pid,
                None,
                raw.lifecycle_context(),
            )
            .await
            .unwrap();
        // The call has answered, so all it published is in the channel.
        let before = watch.pending.contains_key(&session);
        while let Ok(row) = rows.try_recv() {
            watch.observe(&row, &settings(), at(index as i64));
        }
        let after = watch.pending.contains_key(&session);
        if before != after {
            let what = if after { "armed" } else { "dropped" };
            waits.push((index + 1, raw.hook_event_name.clone(), what));
        }
    }
    let expected = [
        // Two turns, each with a permission dialog, each ended by its Stop
        // and followed by the next prompt.
        (10, "Stop", "armed"),
        (12, "UserPromptSubmit", "dropped"),
        (18, "Stop", "armed"),
        (20, "UserPromptSubmit", "dropped"),
        // Line 25 is a Stop while a background agent runs: no wait. The
        // turn its task notification starts (line 28) ends at line 29.
        (29, "Stop", "armed"),
        // The idle notice a minute later (line 31) keeps that wait.
        // `/compact` at the idle prompt reads as a turn of its own.
        (32, "PreCompact", "dropped"),
        (35, "SessionStart", "armed"),
        // `/clear`: the row goes, and the new session's first row is idle
        // with no turn before it. So is the resumed session's.
        (37, "SessionEnd", "dropped"),
    ];
    let waits: Vec<(usize, &str, &str)> = waits
        .iter()
        .map(|(line, name, what)| (*line, name.as_str(), *what))
        .collect();
    assert_eq!(waits, expected);
    assert_eq!(watch.next_wake(), None);
}
