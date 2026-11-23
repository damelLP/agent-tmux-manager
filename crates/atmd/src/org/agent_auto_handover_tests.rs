//! The handover watcher on the handover tests' rig: a real organisation
//! actor, a real registry, real role files, the recorder for the clear. The
//! test plays the message runtime and the agent, as there, and moves the
//! watcher's clock by hand. Every wait is on a committed event, on the
//! recorder, or on the clock being asked for a time; nothing sleeps.

use super::super::agent_auto_handover::{
    return_attempt, spawn_watch, start_lines, still_shows, why, Timer,
};
use super::super::start_line::start_line;
use super::*;
use atm_core::{BackgroundActivity, LifecycleContext, SessionView};
use atm_org::auto_handover::{handover_settings, HandoverSettings};
use atm_org::idle::HandoverReason;
use atm_org::{DecisionCommand, DecisionKind, DecisionRequest};
use atm_protocol::{ClientMessage, DaemonMessage};
use chrono::{DateTime, TimeDelta, TimeZone};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, RwLock};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

fn after(secs: i64) -> DateTime<Utc> {
    t0() + TimeDelta::seconds(secs)
}

/// A clock the test moves. Every time it is asked for is reported, so a test
/// can wait until the watcher has asked to be woken.
struct Clock {
    now: watch::Sender<DateTime<Utc>>,
    asked: mpsc::UnboundedSender<DateTime<Utc>>,
}

impl Timer for Clock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.borrow()
    }
    fn at(&self, at: DateTime<Utc>) -> Deadline {
        let _ = self.asked.send(at);
        let mut now = self.now.subscribe();
        Box::pin(async move {
            if now.wait_for(|now| *now >= at).await.is_err() {
                std::future::pending::<()>().await;
            }
        })
    }
}

const ON: &str = "auto_handover = ['context']";

struct Auto {
    rig: Rig,
    clock: Arc<Clock>,
    asked: mpsc::UnboundedReceiver<DateTime<Utc>>,
    /// A second reader of the committed events, for the probe's notes only.
    fence: broadcast::Receiver<Arc<Envelope>>,
    /// How many sessions the probe has had.
    probes: u32,
}

impl Auto {
    /// The rig with the watcher running and `keys` in the worker's role file.
    async fn new(keys: &str) -> Self {
        Self::with_rig(Rig::new().await, keys).await
    }

    async fn with_rig(rig: Rig, keys: &str) -> Self {
        let (asked, asks) = mpsc::unbounded_channel();
        let clock = Arc::new(Clock {
            now: watch::Sender::new(t0()),
            asked,
        });
        let mut auto = Self {
            fence: rig.org.committed.subscribe(),
            rig,
            clock,
            asked: asks,
            probes: 0,
        };
        auto.role("worker", keys);
        // A third agent, switched off and with no manager: see `settled`.
        auto.rig
            .agent("probe", "+++\nharness='pi'\n+++\n", "%9", None)
            .await;
        auto.rig.kinds();
        spawn_watch(
            auto.rig.service.clone(),
            auto.clock.clone(),
            auto.rig.cancel.clone(),
        );
        auto
    }

    /// Writes `name`'s role file with `keys` in its front matter.
    fn role(&self, name: &str, keys: &str) {
        let text = format!("+++\nharness = 'pi'\nmanager = 'lead'\n{keys}\n+++\nWork\n");
        self.file(name, &text);
    }

    fn file(&self, name: &str, text: &str) {
        let roles = self.rig.root.join(".atm/roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join(format!("{name}.md")), text).unwrap();
    }

    fn set(&self, now: DateTime<Utc>) {
        self.clock.now.send_replace(now);
    }

    /// An independent idle wake, re-requested after every settled outcome.
    async fn admission_timer(&mut self) {
        self.role(
            "timer",
            "auto_handover = ['idle']\ncache_minutes = 60\nidle_minutes = 45",
        );
        self.rig
            .agent(
                "timer",
                "+++\nharness='pi'\n+++\nTimer",
                "%8",
                Some("timer-1"),
            )
            .await;
        self.rig.turn("timer-1").await;
        self.asked_for(after(2700)).await;
    }

    /// Returns once the watcher has asked the clock for `at`.
    async fn asked_for(&mut self, at: DateTime<Utc>) {
        tokio::time::timeout(GUARD, async {
            while self.asked.recv().await.unwrap() != at {}
        })
        .await
        .unwrap_or_else(|_| panic!("the watcher never asked for {at}"));
    }

    /// The committed events up to and including the next of `kind`, the
    /// probe's own left out.
    async fn until(&mut self, kind: &str) -> Vec<Arc<Envelope>> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(GUARD, self.rig.committed.recv())
                .await
                .unwrap_or_else(|_| panic!("no {kind} was committed after {events:?}"))
                .unwrap();
            if probes(&event) {
                continue;
            }
            let last = event.kind == kind;
            events.push(event);
            if last {
                return events;
            }
        }
    }

    /// The next message: no event comes before it but those of `between`.
    async fn message(&mut self, between: &[&str]) -> (String, String) {
        let events = self.until("msg.sent").await;
        assert_eq!(kinds(&events), [between, &["msg.sent"]].concat());
        let sent = events.last().unwrap();
        (
            sent.body["to"].as_str().unwrap().to_owned(),
            sent.body["body"].as_str().unwrap().to_owned(),
        )
    }

    /// The save request the watcher's handover queued for `name`, with no
    /// event before it but those of `between`; its audit follows.
    async fn save_requested(&mut self, name: &str, between: &[&str]) -> MessageId {
        let request = save_request(&id(name), &self.rig.memory(name));
        assert_eq!(
            self.message(between).await,
            (format!("p/{name}"), request.body)
        );
        let audit = self.until("agent.save_requested").await;
        assert_eq!(kinds(&audit), ["agent.save_requested"]);
        serde_json::from_value(audit[0].body["message_id"].clone()).unwrap()
    }

    /// Returns once the watcher has dealt with every registry event sent
    /// so far.
    ///
    /// The probe ends a turn at the line in a session of its own. The
    /// watcher takes registry events in order and sends a note at the line
    /// before it takes the next one, so the probe's note is behind
    /// everything the watcher did about the events before it: its notes are
    /// committed, and the clock has been asked for the deadline of every
    /// handover it started.
    async fn caught_up(&mut self) {
        self.probes += 1;
        let session = format!("probe-{}", self.probes);
        self.rig.bind("probe", &session, "%9").await;
        self.rig.share(&session, 600).await;
        self.rig.turn(&session).await;
        tokio::time::timeout(GUARD, async {
            while !noted(&self.fence.recv().await.unwrap()) {}
        })
        .await
        .expect("the probe was not noted");
    }

    /// How many handovers the watcher has started since the last call. It
    /// asks the clock for a handover's deadline before it starts one, ten
    /// minutes from now at the context line.
    fn started(&mut self) -> usize {
        let deadline = self.clock.now() + TimeDelta::seconds(600);
        std::iter::from_fn(|| self.asked.try_recv().ok())
            .filter(|at| *at == deadline)
            .count()
    }

    /// Up to now the watcher has sent nothing and started no handover; the
    /// only events committed are those of `between`.
    async fn quiet(&mut self, between: &[&str]) {
        self.caught_up().await;
        let committed: Vec<_> = std::iter::from_fn(|| self.rig.committed.try_recv().ok())
            .filter(|event| !probes(event))
            .collect();
        assert_eq!(kinds(&committed), between);
        assert_eq!(self.started(), 0);
    }

    /// The watcher has started exactly one handover since the last look.
    async fn one_started(&mut self) {
        self.caught_up().await;
        assert_eq!(self.started(), 1);
    }

    /// Queues `content` and reads its own event off the stream.
    async fn send(&mut self, content: MessageContent) -> MessageId {
        let sent = self.rig.run(OrgCommand::SendMessage(content)).await;
        let id = MessageId::new(sent[0].seq);
        let events = self.until("msg.sent").await;
        assert_eq!(events.last().unwrap().seq, id.seq());
        id
    }

    /// A queued message to `name` that is not a save request.
    async fn mail(&mut self, name: &str) -> MessageId {
        self.send(MessageContent {
            delivery_notice: None,
            from: Address::Owner,
            to: Address::Agent(id(name)),
            kind: MessageKind::Note,
            body: "Read this".into(),
            attachments: Vec::new(),
            task: None,
            post: None,
        })
        .await
    }

    /// `message_id` is confirmed in `session` without a turn starting.
    async fn confirm(&self, message_id: MessageId, session: &str, pane: &str) {
        self.rig
            .run(OrgCommand::DeliveryAttempted {
                message_id,
                attempt: 1,
                pane: pane.into(),
            })
            .await;
        self.rig
            .run(OrgCommand::Delivered {
                message_id,
                session_id: session.into(),
                confirmation: "prompt".into(),
            })
            .await;
    }
}

/// The note at the line for the probe.
fn noted(event: &Envelope) -> bool {
    event.kind == "msg.sent" && event.body["body"] == line_note("probe", 60)
}

/// An event of the probe's own: its sessions' bindings and its notes.
fn probes(event: &Envelope) -> bool {
    event.body["agent_id"] == "p/probe" || noted(event)
}

fn kinds(events: &[Arc<Envelope>]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

const DELIVERY: [&str; 2] = ["msg.delivery_attempted", "msg.delivered"];

// Break caught: queue/audit failure is mistaken for a silent preflight refusal,
// returning the automatic attempt or sending both manual and automatic notes.
#[tokio::test]
async fn admission_automatic_write_failures_keep_the_attempt_and_report_once() {
    for boundary in [AdmissionWrite::Queue, AdmissionWrite::Audit] {
        let mut rig = Rig::new().await;
        let mut gate = rig.gate(boundary);
        let mut auto = Auto::with_rig(rig, ON).await;
        auto.rig.share("s1", 630).await;
        auto.rig.turn("s1").await;
        let held = gate.take().await;
        let (reply, request, expected) = match boundary {
            AdmissionWrite::Queue => {
                let (reply, committed) = commit_held(&auto.rig.org, held).await;
                let request = MessageId::new(committed.unwrap().events[0].seq);
                (reply, request, "could not confirm the save request for p/worker: organisation request timed out; accepted work may still complete; a request may be queued or already delivered; nothing was cleared; inspect `atm inbox --for p/worker` before retrying".to_owned())
            }
            AdmissionWrite::Audit => {
                let super::super::Request::Execute { command, reply, .. } = held else {
                    panic!("not a write")
                };
                let OrgCommand::SaveRequested { message_id, .. } = command.as_ref() else {
                    panic!("not a save audit")
                };
                (reply, *message_id, audit_failure(*message_id))
            }
        };
        assert_eq!(auto.started(), 1);
        reply
            .send(Err(super::super::OrgServiceError::Timeout))
            .unwrap();
        // The retained shared handover sends its own notice; wait for that
        // actual commit, not an unrelated cycle through the policy watcher.
        tokio::time::timeout(GUARD, async {
            loop {
                let event = auto.fence.recv().await.unwrap();
                if event.kind == "msg.sent"
                    && event.body["body"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("automatic handover of p/worker"))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let state = auto.rig.state().await;
        let notes: Vec<_> = state
            .messages()
            .filter(|m| m.content.body.starts_with("automatic handover of p/worker"))
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "post-preflight failure was silently discarded"
        );
        assert_eq!(notes[0].content.to, Address::Agent(id("lead")));
        assert_eq!(
            notes[0].content.body,
            format!(
            "automatic handover of p/worker (context 63%, line 60%) did not complete: {expected}. \
             Nothing more is tried in this session. By hand: atm agent handover p/worker"
        )
        );
        assert_eq!(
            state.message(&request).unwrap().status,
            DeliveryStatus::Queued
        );
        assert!(!state
            .messages()
            .any(|m| m.content.body.starts_with("handover of ")));
        assert!(auto.rig.cleared.try_recv().is_err());
        auto.rig.kinds();
        auto.rig.turn("s1").await;
        auto.quiet(&[]).await;
        assert_eq!(
            auto.rig
                .state()
                .await
                .messages()
                .filter(|m| m.content.body.starts_with("automatic handover of p/worker"))
                .count(),
            1
        );
    }
}

// Break caught: an automatic preflight refusal is noted or consumes the attempt.
#[tokio::test]
async fn admission_automatic_preflight_refusal_stays_silent_and_allows_retry() {
    struct RefuseFirst {
        gate: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
        recorder: Arc<Recorder>,
    }
    impl Clearer for RefuseFirst {
        fn available(
            &self,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), AgentError>> + Send>> {
            let gate = self.gate.lock().unwrap().take();
            Box::pin(async move {
                if let Some((entered, release)) = gate {
                    entered.send(()).unwrap();
                    release.await.unwrap();
                    return Err(AgentError::Unavailable("preflight unavailable".into()));
                }
                Ok(())
            })
        }
        fn clear(
            &self,
            target: SavedSeat,
            retry_before: Option<tokio::time::Instant>,
            finish_by: tokio::time::Instant,
        ) -> Answer<ClearAttempt> {
            self.recorder.clear(target, retry_before, finish_by)
        }
    }
    let mut rig = Rig::new().await;
    let (entered, entry) = oneshot::channel();
    let (release, released) = oneshot::channel();
    rig.service = Handovers::new(
        rig.org.clone(),
        rig.registry.clone(),
        Arc::new(RefuseFirst {
            gate: Mutex::new(Some((entered, released))),
            recorder: rig.recorder.clone(),
        }),
        rig.service_stop.clone(),
    );
    let mut auto = Auto::with_rig(rig, ON).await;
    auto.admission_timer().await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    tokio::time::timeout(GUARD, entry).await.unwrap().unwrap();
    while auto.asked.try_recv().is_ok() {}
    release.send(()).unwrap();
    auto.asked_for(after(2700)).await;
    assert_eq!(
        auto.rig.state().await.messages().count(),
        0,
        "preflight refusal sent a request or note"
    );
    assert!(auto.rig.cleared.try_recv().is_err());
    auto.rig.kinds();
    auto.rig.turn("s1").await;
    auto.one_started().await;
    let request = auto.save_requested("worker", &[]).await;
    auto.rig.deliver(request, "s1", "%1").await;
    std::fs::write(auto.rig.memory("worker"), "retry saved").unwrap();
    auto.rig.report("s1", LifecycleEvent::Idle).await;
    auto.message(&["msg.delivery_attempted", "msg.delivered", "agent.saved"])
        .await;
    assert_eq!(auto.rig.cleared.try_recv().unwrap(), id("worker"));
    auto.rig.turn("s1").await;
    auto.quiet(&[]).await;
}

// The pure parts.

#[test]
fn a_handover_refused_at_admission_gives_only_its_attempt_back() {
    let mut attempted = BTreeMap::from([
        (id("worker"), SessionId::new("s1")),
        (id("lead"), SessionId::new("lead-1")),
    ]);
    return_attempt(&mut attempted, &id("worker"), &SessionId::new("s1"));
    assert_eq!(
        attempted,
        BTreeMap::from([(id("lead"), SessionId::new("lead-1"))])
    );
    // The agent has moved on to another session, whose attempt is its own.
    let mut later = BTreeMap::from([(id("worker"), SessionId::new("s2"))]);
    return_attempt(&mut later, &id("worker"), &SessionId::new("s1"));
    assert_eq!(later.get(&id("worker")), Some(&SessionId::new("s2")));
}

#[test]
fn the_note_says_why_in_the_seats_own_numbers() {
    let settings = |keys: &str| -> HandoverSettings {
        let role = format!("+++\nharness = 'pi'\n{keys}\n+++\n");
        handover_settings(&atm_org::parse_role("worker", &role).unwrap().config).unwrap()
    };
    let idle = Duration::from_secs(48 * 60 + 59);
    assert_eq!(
        why(
            HandoverReason::ContextLine,
            72.6,
            &settings("context_line = 70"),
            idle
        ),
        "context 73%, line 70%"
    );
    let cached = settings("auto_handover = ['idle']\ncache_minutes = 60");
    assert_eq!(
        why(HandoverReason::ContextLine, 61.0, &cached, idle),
        "context 61%, line 60%"
    );
    assert_eq!(
        why(HandoverReason::CacheExpiring, 12.0, &cached, idle),
        "idle 48 min; nothing it waits for comes before its cache expires at 60 min"
    );
}

#[tokio::test]
async fn only_start_lines_delivered_to_the_session_are_counted() {
    let mut auto = Auto::new("").await;
    let (worker, s1) = (id("worker"), SessionId::new("s1"));
    let count = |state: &OrgState| start_lines(state, &worker, &s1);
    assert_eq!(count(&auto.rig.state().await), 0);
    // Other mail, a start line still queued, one to another agent, and one
    // delivered to another session of the worker: none counts.
    let other = auto.mail("worker").await;
    auto.confirm(other, "s1", "%1").await;
    let queued = auto.send(start_line(&worker)).await;
    let leads = auto.send(start_line(&id("lead"))).await;
    auto.confirm(leads, "s1", "%0").await;
    assert_eq!(count(&auto.rig.state().await), 0);
    auto.confirm(queued, "s0", "%1").await;
    assert_eq!(count(&auto.rig.state().await), 0);
    let here = auto.send(start_line(&worker)).await;
    auto.confirm(here, "s1", "%1").await;
    assert_eq!(count(&auto.rig.state().await), 1);
}

#[test]
fn a_row_that_has_moved_on_is_not_the_row_that_was_judged() {
    let mut row = SessionDomain::new(
        SessionId::new("s1"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    row.apply_lifecycle_event(&LifecycleEvent::Idle);
    let idle = SessionView::from_domain(&row);
    assert_eq!(idle.status, SessionStatus::Idle);
    assert!(still_shows(&idle, idle.context_percentage, false));
    assert!(!still_shows(&idle, idle.context_percentage + 1.0, false));
    assert!(!still_shows(&idle, idle.context_percentage, true));
    let background = SessionView {
        activity_detail: Some("1 bg task".into()),
        ..idle.clone()
    };
    assert!(!still_shows(&background, idle.context_percentage, false));
    assert!(still_shows(&background, idle.context_percentage, true));
    row.apply_lifecycle_event(&LifecycleEvent::WorkingStart);
    let working = SessionView::from_domain(&row);
    assert!(!still_shows(&working, working.context_percentage, false));
}

// The watcher.

#[tokio::test]
async fn a_seat_that_is_switched_on_is_handed_over_at_the_line_with_one_note() {
    let rig = Rig::new().await;
    rig.register_chief().await;
    let mut auto = Auto::with_rig(rig, ON).await;
    // Below the line nothing is asked.
    auto.rig.share("s1", 599).await;
    auto.rig.turn("s1").await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    auto.one_started().await;
    // No command was given: the daemon asks the worker to save, and sends
    // no note at the line first.
    let request = auto.save_requested("worker", &[]).await;
    let (release, gate) = oneshot::channel();
    *auto.rig.recorder.clear_gate.lock().unwrap() = Some(gate);
    auto.rig.deliver(request, "s1", "%1").await;
    std::fs::write(auto.rig.memory("worker"), "state").unwrap();
    auto.rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(
        tokio::time::timeout(GUARD, auto.rig.cleared.recv())
            .await
            .unwrap(),
        Some(id("worker"))
    );
    // The outcome belongs to the current manager, not to the one used
    // at admission. A clear reply without a start-line id is not success.
    auto.file(
        "worker",
        &format!("+++\nharness = 'pi'\n{ON}\nmanager = 'org/chief'\n+++\nWork"),
    );
    release.send(()).unwrap();
    assert_eq!(
        auto.message(&[DELIVERY.as_slice(), &["agent.saved"]].concat())
            .await,
        (
            "org/chief".to_owned(),
            format!(
                "automatic handover of p/worker (context 63%, line 60%) did not complete: \
                 saved ({request}); clear confirmed; resume not confirmed: the start line could not be identified."
            )
        )
    );
    // The recorder cleared nothing, so the worker is still in that session:
    // another turn end above the line starts no second handover.
    auto.rig.share("s1", 700).await;
    auto.rig.turn("s1").await;
    auto.quiet(&[]).await;
}

#[tokio::test]
async fn a_seat_that_is_off_is_noted_at_its_own_line() {
    let mut auto = Auto::new("context_line = 70").await;
    auto.rig.share("s1", 650).await;
    auto.rig.turn("s1").await;
    auto.rig.share("s1", 700).await;
    assert_eq!(
        auto.message(&[]).await,
        (
            "p/lead".to_owned(),
            "agent p/worker is at 70% of its context (line 70%). \
             Hand it over: atm agent handover p/worker"
                .to_owned()
        )
    );
}

#[tokio::test]
async fn the_role_file_is_read_at_every_turn_end() {
    let mut auto = Auto::new(ON).await;
    auto.rig.share("s1", 500).await;
    auto.rig.turn("s1").await;
    // Switched off by an edit: the next turn end gets the note, no handover.
    auto.role("worker", "");
    auto.rig.share("s1", 650).await;
    auto.rig.turn("s1").await;
    assert_eq!(
        auto.message(&[]).await,
        ("p/lead".to_owned(), line_note("worker", 65))
    );
    // Switched on again, with no respawn: the session that was noted is
    // handed over at its next turn end.
    auto.role("worker", ON);
    auto.rig.turn("s1").await;
    auto.one_started().await;
    auto.save_requested("worker", &[]).await;
}

#[tokio::test]
async fn a_role_file_that_is_not_valid_is_off() {
    let mut auto = Auto::new(ON).await;
    // A session of its own for every file: a session is noted once.
    for (session, text) in [
        ("n1", "auto_handover = ['context']"),
        (
            "n2",
            "+++\nharness = 'pi'\nauto_handover = ['context', 'later']\n+++\n",
        ),
        ("n3", "+++\nharness = 'pi'\nauto_handover = ['idle']\n+++\n"),
    ] {
        auto.file("worker", text);
        auto.rig.bind("worker", session, "%1").await;
        auto.rig.share(session, 650).await;
        auto.rig.turn(session).await;
        assert_eq!(
            auto.message(&["agent.session_bound"]).await,
            ("p/lead".to_owned(), line_note("worker", 65)),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_failed_save_is_told_once_and_not_tried_again_in_that_session() {
    let mut auto = Auto::new(ON).await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    auto.one_started().await;
    let request = auto.save_requested("worker", &[]).await;
    // The worker ends its save turn and has written nothing.
    auto.rig.deliver(request, "s1", "%1").await;
    auto.rig.report("s1", LifecycleEvent::Idle).await;
    let memory = auto.rig.memory("worker");
    assert_eq!(
        auto.message(&DELIVERY).await,
        (
            "p/lead".to_owned(),
            format!(
                "automatic handover of p/worker (context 63%, line 60%) did not complete: \
                 p/worker ended its save turn and {} did not change; nothing was cleared. \
                 Nothing more is tried in this session. By hand: atm agent handover p/worker",
                memory.display()
            )
        )
    );
    assert!(auto.rig.cleared.try_recv().is_err());
    // More turn ends above the line in that session: no request, no note.
    auto.rig.share("s1", 700).await;
    auto.rig.turn("s1").await;
    auto.quiet(&[]).await;
    // A new session has an attempt of its own.
    auto.rig.bind("worker", "s2", "%1").await;
    auto.rig.share("s2", 640).await;
    auto.rig.turn("s2").await;
    auto.one_started().await;
    auto.save_requested("worker", &["agent.session_bound"])
        .await;
}

#[tokio::test]
async fn a_session_is_not_handed_over_while_it_shows_background_work() {
    let mut auto = Auto::new(ON).await;
    // The lead, switched on too, ends a turn at the line with a background
    // task still running: the registry reports the idle row, then the same
    // row with that work on it.
    auto.role("lead", ON);
    auto.rig.share("lead-1", 640).await;
    auto.rig
        .report("lead-1", LifecycleEvent::WorkingStart)
        .await;
    let background = LifecycleContext {
        background_activity: Some(BackgroundActivity {
            running: 1,
            scheduled: 0,
        }),
        ..LifecycleContext::default()
    };
    auto.rig
        .registry
        .apply_lifecycle_event_with_context(
            SessionId::new("lead-1"),
            LifecycleEvent::Idle,
            Harness::Pi,
            None,
            None,
            background,
        )
        .await
        .unwrap();
    let row = auto.rig.registry.get_session(SessionId::new("lead-1"));
    assert_eq!(
        row.await.unwrap().activity_detail.as_deref(),
        Some("1 bg task")
    );
    // A share that moves while that work runs changes nothing.
    auto.rig.share("lead-1", 650).await;
    auto.quiet(&[]).await;
    // The background work is over when the lead's next turn ends.
    auto.rig.turn("lead-1").await;
    auto.one_started().await;
    auto.save_requested("lead", &[]).await;
}

#[tokio::test]
async fn a_session_whose_only_turn_was_its_start_is_noted_and_not_handed_over() {
    let mut auto = Auto::new(ON).await;
    let line = auto.send(start_line(&id("worker"))).await;
    auto.confirm(line, "s1", "%1").await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    assert_eq!(
        auto.message(&DELIVERY).await,
        (
            "p/lead".to_owned(),
            format!(
                "{} This session started above its line, so it will not be handed over automatically.",
                line_note("worker", 63)
            )
        )
    );
    auto.quiet(&[]).await;
}

#[tokio::test]
async fn a_session_starting_at_its_line_is_not_handed_over_after_a_second_turn() {
    for keys in [ON, IDLE] {
        let mut auto = Auto::new(keys).await;
        let line = auto.send(start_line(&id("worker"))).await;
        auto.confirm(line, "s1", "%1").await;
        auto.rig.share("s1", 600).await;
        auto.rig.turn("s1").await;
        auto.message(&DELIVERY).await;
        auto.quiet(&[]).await;
        // A worked turn and the idle limit must not re-enable either rule
        // in this session, even when its share moves above the line.
        auto.rig.share("s1", 640).await;
        auto.rig.turn("s1").await;
        auto.caught_up().await;
        auto.set(after(IDLE_LIMIT));
        auto.rig.share("s1", 650).await;
        auto.caught_up().await;
        let asks: Vec<_> = std::iter::from_fn(|| auto.asked.try_recv().ok()).collect();
        assert!(asks.is_empty(), "{keys}: asks: {asks:?}");
        auto.quiet(&[]).await;
    }
}

#[tokio::test]
async fn mail_on_its_way_puts_the_handover_off_until_the_watcher_looks_again() {
    let mut auto = Auto::new(ON).await;
    let mail = auto.mail("worker").await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    // The watcher has looked and asks to be woken a minute later.
    auto.asked_for(after(60)).await;
    auto.quiet(&[]).await;
    // The mail is delivered and starts no turn. Until the minute is over
    // nothing is looked at; then the handover starts.
    auto.confirm(mail, "s1", "%1").await;
    auto.set(after(60));
    auto.save_requested("worker", &DELIVERY).await;
}

#[tokio::test]
async fn a_turn_that_starts_before_the_wake_drops_it() {
    let mut auto = Auto::new(ON).await;
    auto.mail("worker").await;
    auto.rig.share("s1", 630).await;
    auto.rig.turn("s1").await;
    auto.asked_for(after(60)).await;
    // A turn starts and the minute passes while it runs: nothing is due.
    auto.rig.report("s1", LifecycleEvent::WorkingStart).await;
    auto.set(after(60));
    auto.quiet(&[]).await;
    // Its end is a turn end like any other: the mail is still on its way,
    // so the watcher asks for a minute from now.
    auto.rig.report("s1", LifecycleEvent::Idle).await;
    auto.asked_for(after(120)).await;
}

#[tokio::test]
async fn a_session_that_was_removed_is_seen_anew() {
    let mut auto = Auto::new(ON).await;
    // The worker's session is working when its row leaves the registry.
    auto.rig.turn("s1").await;
    auto.rig.report("s1", LifecycleEvent::WorkingStart).await;
    auto.rig
        .registry
        .remove(SessionId::new("s1"))
        .await
        .unwrap();
    // It comes back idle and above the line. No turn was seen to end in it:
    // the status from before the removal is not this row's past.
    auto.rig.bind("worker", "s1", "%1").await;
    auto.rig.share("s1", 640).await;
    auto.quiet(&[]).await;
    // Its next turn end is one.
    auto.rig.turn("s1").await;
    auto.one_started().await;
    auto.save_requested("worker", &[]).await;
}

// The idle rule.

/// `"idle"` on with a one-hour cache and an idle limit of forty-five minutes,
/// so that the time left for the save is not the ten minutes of the line.
const IDLE: &str = "auto_handover = ['idle']\ncache_minutes = 60\nidle_minutes = 45";
const IDLE_LIMIT: i64 = 45 * 60;
const EXPIRY: i64 = 60 * 60;

#[tokio::test]
async fn an_idle_only_seat_above_its_line_is_noted_and_woken_at_its_idle_limit() {
    let mut auto = Auto::new(IDLE).await;
    auto.rig.share("s1", 700).await;
    auto.rig.turn("s1").await;
    assert_eq!(
        auto.message(&[]).await,
        ("p/lead".to_owned(), line_note("worker", 70))
    );
    auto.caught_up().await;
    let asks: Vec<_> = std::iter::from_fn(|| auto.asked.try_recv().ok()).collect();
    assert!(asks.contains(&after(IDLE_LIMIT)), "asks: {asks:?}");
    // A later turn still schedules its limit even though the session was
    // already noted: the once-only note must not swallow the new wake.
    auto.set(after(7));
    auto.rig.turn("s1").await;
    auto.caught_up().await;
    let asks: Vec<_> = std::iter::from_fn(|| auto.asked.try_recv().ok()).collect();
    assert!(asks.contains(&after(IDLE_LIMIT + 7)), "asks: {asks:?}");
    auto.set(after(IDLE_LIMIT + 7));
    auto.asked_for(after(EXPIRY + 7)).await;
    auto.save_requested("worker", &[]).await;
}

#[tokio::test]
async fn an_idle_seat_that_waits_for_nothing_is_asked_to_save_until_its_cache_expires() {
    let mut auto = Auto::new(IDLE).await;
    auto.rig.share("s1", 100).await;
    auto.rig.turn("s1").await;
    // Nothing is due at the turn end: the watcher asks for the idle limit.
    auto.asked_for(after(IDLE_LIMIT)).await;
    auto.quiet(&[]).await;
    auto.set(after(IDLE_LIMIT));
    // The handover may wait for the save until the cache expires, no longer.
    auto.asked_for(after(EXPIRY)).await;
    let request = auto.save_requested("worker", &[]).await;
    // Nobody delivers the request. At the expiry the handover gives up, and
    // the manager is told why it was started.
    auto.set(after(EXPIRY));
    let (to, body) = auto.message(&[]).await;
    assert_eq!(to, "p/lead");
    let start = format!(
        "automatic handover of p/worker (idle 45 min; nothing it waits for comes before its \
         cache expires at 60 min) did not complete: the save request {request} was not \
         delivered to p/worker in 900 s;"
    );
    assert!(body.starts_with(&start), "{body}");
    assert!(body.ends_with(
        "nothing was cleared. Nothing more is tried in this session. \
         By hand: atm agent handover p/worker"
    ));
    assert!(auto.rig.cleared.try_recv().is_err());
}

#[tokio::test]
async fn an_idle_seat_that_waits_for_the_owners_answer_keeps_its_session() {
    let mut auto = Auto::new(IDLE).await;
    auto.rig
        .run(OrgCommand::Decision(DecisionCommand::Request(
            DecisionRequest {
                requester: Address::Agent(id("worker")),
                kind: DecisionKind::Policy,
                title: "Which".into(),
                context: String::new(),
                files: Vec::new(),
                options: vec!["a".into(), "b".into()],
                recommended: Some("a".into()),
                may_be_undone: true,
                changes_intent: true,
                deadline: None,
            },
        )))
        .await;
    auto.rig.turn("s1").await;
    auto.asked_for(after(IDLE_LIMIT)).await;
    auto.set(after(IDLE_LIMIT));
    // The watcher is woken at the limit; a share that moves makes it look
    // once more, in step with the registry. Neither look starts a handover:
    // the owner is taken to be present, so his answer comes in time.
    auto.rig.share("s1", 100).await;
    auto.quiet(&["decision.requested"]).await;
    // The lead waits for nothing: the same look at the limit hands it over.
    auto.role("lead", IDLE);
    auto.rig.turn("lead-1").await;
    auto.set(after(2 * IDLE_LIMIT));
    auto.asked_for(after(IDLE_LIMIT + EXPIRY)).await;
    auto.save_requested("lead", &[]).await;
}

#[tokio::test]
async fn a_new_turn_end_starts_the_idle_wait_again() {
    let mut auto = Auto::new(IDLE).await;
    auto.rig.turn("s1").await;
    auto.asked_for(after(IDLE_LIMIT)).await;
    // A turn runs across the first limit: it is measured from its own end.
    auto.rig.report("s1", LifecycleEvent::WorkingStart).await;
    auto.set(after(IDLE_LIMIT + 7));
    auto.rig.report("s1", LifecycleEvent::Idle).await;
    auto.asked_for(after(2 * IDLE_LIMIT + 7)).await;
    auto.quiet(&[]).await;
}

// A measured input Idle is not a completed turn. Counting it as one lets a
// start-line-only session hand itself over again after every new session.
async fn start_line_auto(keys: &str, harness: Harness) -> Auto {
    let mut auto = Auto::new(keys).await;
    if harness != Harness::Pi {
        auto.rig.registry.remove("s1".into()).await.unwrap();
        let mut row = SessionDomain::new("s1".into(), AgentType::GeneralPurpose, Model::Unknown);
        row.harness = harness;
        row.tmux_pane = Some("%1".into());
        auto.rig.registry.register(row).await.unwrap();
        auto.file(
            "worker",
            &format!(
                "+++\nharness = '{}'\nmanager = 'lead'\n{keys}\n+++\nWork\n",
                harness.short_tag()
            ),
        );
    }
    let line = auto.send(start_line(&id("worker"))).await;
    auto.confirm(line, "s1", "%1").await;
    auto
}

async fn replay_turn(auto: &Auto, frames: Vec<ClientMessage>) {
    use crate::server::{methods::MethodRouter, ConnectionHandler};

    async fn send(peer: &mut BufReader<tokio::io::DuplexStream>, frame: ClientMessage) {
        let line = serde_json::to_string(&frame).unwrap() + "\n";
        peer.get_mut().write_all(line.as_bytes()).await.unwrap();
    }
    async fn receive(peer: &mut BufReader<tokio::io::DuplexStream>) -> DaemonMessage {
        let mut line = String::new();
        assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }

    tokio::time::timeout(GUARD, async {
        let (ours, peer) = tokio::io::duplex(65536);
        let (read, write) = tokio::io::split(ours);
        let cancel = auto.rig.cancel.child_token();
        let handler = ConnectionHandler::new(
            read,
            write,
            auto.rig.registry.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel.clone(),
            Arc::new(MethodRouter::default()),
        );
        let task = tokio::spawn(handler.run());
        let mut peer = BufReader::new(peer);
        send(&mut peer, ClientMessage::connect(None)).await;
        assert!(matches!(
            receive(&mut peer).await,
            DaemonMessage::Connected { .. }
        ));
        for frame in frames {
            send(&mut peer, frame).await;
            // The same-stream response fences application, including derived
            // registry entries. No socket polling or readiness delay.
            send(&mut peer, ClientMessage::list_sessions()).await;
            assert!(matches!(
                receive(&mut peer).await,
                DaemonMessage::SessionList { .. }
            ));
        }
        assert_eq!(
            auto.rig
                .registry
                .get_session("s1".into())
                .await
                .unwrap()
                .status,
            SessionStatus::Idle
        );
        cancel.cancel();
        task.await.unwrap();
    })
    .await
    .unwrap();
}

fn pi_turn_frames(share: u64) -> Vec<ClientMessage> {
    // Captured: pi 1.5.2, 2026-11-16, sealed PTY/private-atmd recording proxy.
    // These historical frames include the reverted forwarder's idle snapshot;
    // already installed snapshot-sending forwarders must also remain safe.
    let frames: Vec<Value> =
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_turn_frames_1.5.2.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|frame| frame["run"] == "input2-skill-only" && frame["case"] == "skill")
            .map(|frame| frame["data"].clone())
            .collect();
    let names: Vec<_> = frames
        .iter()
        .map(|data| data["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "input",
            "agent_start",
            "context",
            "turn_end",
            "agent_settled"
        ]
    );
    assert_eq!(frames.first().unwrap()["payload"]["idle"], true);
    frames
        .into_iter()
        .map(|mut data| {
            // Synthetic: bind the real sequence to this seat and context line.
            data["session_id"] = json!("s1");
            data["tmux_pane"] = json!("%1");
            data.as_object_mut().unwrap().remove("pid");
            if data["event"] == "context" || data["event"] == "turn_end" {
                data["usage"]["contextTokens"] = json!(share * 1_000);
                data["model"]["contextWindow"] = json!(1_000_000);
            }
            ClientMessage::pi_event(data)
        })
        .collect()
}

fn text_only_frames(harness: Harness) -> Vec<ClientMessage> {
    // Captured: Claude Code 2.1.325 and Codex 0.163.0, dates/methods in the
    // respective adapter crate docs. Synthetic: prompt/Stop-only projection,
    // seat attribution, and omission of transcript/cwd paths; no tool or
    // independent WorkingStart is supplied to make the watcher count a turn.
    let (text, session) = match harness {
        Harness::ClaudeCode => (
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/claude_turn_boundaries_2.1.325.jsonl"
            ),
            "blocked-session",
        ),
        Harness::Codex => (
            include_str!(
                "../../../atm-codex-adapter/tests/fixtures/codex_tui_session_0.163.0.jsonl"
            ),
            "01a16f8c-ffa5-7303-87fa-051477b77d16",
        ),
        _ => panic!("text-only control needs Claude or Codex"),
    };
    let frames: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|data| {
            data["session_id"] == session
                && (data["hook_event_name"] == "UserPromptSubmit"
                    || (data["hook_event_name"] == "Stop" && data["stop_hook_active"] == false))
        })
        .collect();
    let names: Vec<_> = frames
        .iter()
        .map(|data| data["hook_event_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["UserPromptSubmit", "Stop"]);
    frames
        .into_iter()
        .map(|mut data| {
            data["session_id"] = json!("s1");
            data["tmux_pane"] = json!("%1");
            for field in ["pid", "cwd", "transcript_path"] {
                data.as_object_mut().unwrap().remove(field);
            }
            match harness {
                Harness::ClaudeCode => ClientMessage::hook_event(data),
                Harness::Codex => ClientMessage::codex_event(data),
                _ => panic!("text-only control needs Claude or Codex"),
            }
        })
        .collect()
}

async fn only_start_line_is_not_idle_work(auto: &mut Auto) {
    auto.caught_up().await;
    auto.set(after(IDLE_LIMIT));
    auto.caught_up().await;
    let asks: Vec<_> = std::iter::from_fn(|| auto.asked.try_recv().ok()).collect();
    assert!(
        !asks.contains(&after(EXPIRY)),
        "unexpected handover deadline: {asks:?}"
    );
    auto.quiet(&DELIVERY).await;
}

async fn next_turn_enables_idle_handover(auto: &mut Auto, frames: Vec<ClientMessage>) {
    auto.set(after(IDLE_LIMIT + 1));
    replay_turn(auto, frames).await;
    auto.caught_up().await;
    auto.set(after(2 * IDLE_LIMIT + 1));
    auto.asked_for(after(IDLE_LIMIT + EXPIRY + 1)).await;
    auto.save_requested("worker", &[]).await;
}

#[tokio::test]
async fn pi_start_line_context_is_excluded() {
    let mut auto = start_line_auto(ON, Harness::Pi).await;
    replay_turn(&auto, pi_turn_frames(630)).await;
    let (to, body) = auto.message(&DELIVERY).await;
    assert_eq!(to, "p/lead");
    assert!(body.contains("started above its line"), "{body}");
    auto.quiet(&[]).await;
    // Once excluded at its line, later real work must not enable either rule.
    replay_turn(&auto, pi_turn_frames(650)).await;
    auto.set(after(IDLE_LIMIT));
    auto.quiet(&[]).await;
}

#[tokio::test]
async fn pi_start_line_idle_is_not_work() {
    let mut auto = start_line_auto(IDLE, Harness::Pi).await;
    replay_turn(&auto, pi_turn_frames(100)).await;
    only_start_line_is_not_idle_work(&mut auto).await;
    next_turn_enables_idle_handover(&mut auto, pi_turn_frames(100)).await;
}

#[tokio::test]
async fn text_only_start_line_context_is_excluded() {
    for harness in [Harness::ClaudeCode, Harness::Codex] {
        let mut auto = start_line_auto(ON, harness).await;
        auto.rig.share("s1", 630).await;
        replay_turn(&auto, text_only_frames(harness)).await;
        let (to, body) = auto.message(&DELIVERY).await;
        assert_eq!(to, "p/lead");
        assert!(body.contains("started above its line"), "{body}");
        auto.quiet(&[]).await;
    }
}

#[tokio::test]
async fn text_only_start_line_idle_is_not_work() {
    for harness in [Harness::ClaudeCode, Harness::Codex] {
        let mut auto = start_line_auto(IDLE, harness).await;
        auto.rig.share("s1", 100).await;
        replay_turn(&auto, text_only_frames(harness)).await;
        only_start_line_is_not_idle_work(&mut auto).await;
        next_turn_enables_idle_handover(&mut auto, text_only_frames(harness)).await;
    }
}
