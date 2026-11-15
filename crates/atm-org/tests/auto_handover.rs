#![allow(clippy::unwrap_used, clippy::expect_used)] // synthetic test histories
//! The policy for a handover without a command: one test per rule of the
//! role's switch, per row of the waits table and per row of `auto_step`'s
//! decision table, with the edges on both sides of every limit. Time is
//! always an argument.

use atm_core::SessionId;
use atm_org::auto_handover::{
    auto_step, handover_settings, waits, AutoInput, AutoStep, HandoverSettings, SettingsError,
    LOOK_AGAIN,
};
use atm_org::idle::{CacheLimits, HandoverLimits, HandoverReason, WaitsFor};
use atm_org::*;
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use std::time::Duration;

const MINUTE: i64 = 60;

/// The turn end every test measures from.
fn turn_end() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

/// `secs` after the turn end.
fn after(secs: i64) -> DateTime<Utc> {
    turn_end() + TimeDelta::seconds(secs)
}

fn agent(name: &str) -> AgentId {
    format!("p/{name}").parse().unwrap()
}

fn worker() -> AgentId {
    agent("worker")
}

fn session() -> SessionId {
    SessionId::new("s1")
}

/// A history under construction: events get the next sequence number.
struct History {
    state: OrgState,
    seq: u64,
}

impl History {
    /// Project `p` with `worker` (ready, `%1`, session `s1`) and `other`
    /// (ready, `%2`, session `s2`).
    fn new() -> Self {
        let mut history = Self {
            state: OrgState::default(),
            seq: 0,
        };
        history.push(OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        });
        for (name, pane, session) in [("worker", "%1", "s1"), ("other", "%2", "s2")] {
            history.push(OrgEvent::AgentUpRequested {
                agent_id: agent(name),
                role: parse_role(name, "+++\nharness='pi'\n+++\n").unwrap(),
            });
            history.push(OrgEvent::AgentPaneCreated {
                agent_id: agent(name),
                pane: pane.into(),
            });
            history.push(OrgEvent::AgentSessionBound {
                agent_id: agent(name),
                session_id: SessionId::new(session),
            });
        }
        assert_eq!(
            history.state.agent(&worker()).unwrap().status,
            AgentStatus::Ready
        );
        history
    }

    /// Applies `event` and returns its sequence number.
    fn push(&mut self, event: OrgEvent) -> u64 {
        self.seq += 1;
        self.state.apply(self.seq, turn_end(), &event).unwrap();
        self.seq
    }

    /// A task `creator` made, with `measure`.
    fn task(&mut self, creator: Address, measure: TaskMeasure) -> TaskId {
        let seq = self.push(OrgEvent::TaskCreated(TaskCreated {
            title: "Work".into(),
            kind: TaskKind::Work,
            creator,
            parent: None,
            judges: None,
            measure,
            budget: None,
            files: Vec::new(),
            rests_on: None,
        }));
        format!("t{seq}").parse().unwrap()
    }

    fn assign(&mut self, task: &TaskId, to: &AgentId) {
        self.push(OrgEvent::TaskAssigned(TaskAssigned {
            task_id: task.clone(),
            agent_id: to.clone(),
            by: Address::Owner,
        }));
    }

    fn set(&mut self, task: &TaskId, state: TaskState) {
        self.push(OrgEvent::TaskStateChanged(TaskStateChanged {
            task_id: task.clone(),
            state,
            reason: Some("why".into()),
            by: Address::Owner,
        }));
    }

    /// A task the owner made, held by the worker in `state`.
    fn held(&mut self, state: TaskState, measure: TaskMeasure) -> TaskId {
        let task = self.task(Address::Owner, measure);
        self.assign(&task, &worker());
        if state != TaskState::Assigned {
            self.set(&task, state);
        }
        assert_eq!(self.state.task(&task).unwrap().state, state);
        task
    }

    /// A pending decision `requester` asked for.
    fn decision(&mut self, requester: Address, deadline: Option<DateTime<Utc>>) -> DecisionId {
        let seq = self.push(OrgEvent::DecisionRequested(DecisionRequest {
            requester,
            kind: DecisionKind::Policy,
            title: "Which".into(),
            context: String::new(),
            files: Vec::new(),
            options: vec!["a".into(), "b".into()],
            recommended: Some("a".into()),
            may_be_undone: true,
            changes_intent: true,
            deadline,
        }));
        DecisionId::new(seq)
    }

    /// A queued message to `to`.
    fn mail(&mut self, to: &AgentId) -> MessageId {
        let seq = self.push(OrgEvent::MessageSent(MessageContent {
            from: Address::Owner,
            to: Address::Agent(to.clone()),
            kind: MessageKind::Note,
            body: "Read this".into(),
            attachments: Vec::new(),
            task: None,
        }));
        MessageId::new(seq)
    }
}

fn mechanical() -> TaskMeasure {
    TaskMeasure::Mechanical {
        command: "cargo test".into(),
    }
}

fn role(keys: &str) -> Result<RoleSpec, ConfigError> {
    parse_role("worker", &format!("+++\nharness = 'pi'\n{keys}\n+++\n"))
}

fn settings(keys: &str) -> HandoverSettings {
    handover_settings(&role(keys).unwrap().config).unwrap()
}

fn refused(keys: &str) -> String {
    role(keys).unwrap_err().to_string()
}

/// `"context"` and `"idle"` on, the default line of 60, a one-hour cache, an idle limit of
/// fifty minutes.
fn both() -> HandoverSettings {
    settings("auto_handover = ['context', 'idle']\ncache_minutes = 60\nidle_minutes = 50")
}

const LIFETIME: i64 = 60 * MINUTE;
const IDLE_LIMIT: i64 = 50 * MINUTE;

/// The worker, idle since the turn end in a session that has worked, at
/// `share` percent, with nothing else in the way.
fn look<'a>(
    history: &'a History,
    settings: &'a HandoverSettings,
    agent: &'a AgentId,
    session: &'a SessionId,
    share: f64,
) -> AutoInput<'a> {
    AutoInput {
        state: &history.state,
        agent,
        session,
        settings,
        context_percent: Some(share),
        idle_since: Some(turn_end()),
        background: false,
        worked: true,
        attempted: false,
        away: false,
    }
}

const RUN_LINE: AutoStep = AutoStep::Run {
    reason: HandoverReason::ContextLine,
    give_up_at: None,
};

fn run_expiring() -> AutoStep {
    AutoStep::Run {
        reason: HandoverReason::CacheExpiring,
        give_up_at: Some(after(LIFETIME)),
    }
}

// The switch.

#[test]
fn a_role_without_the_keys_is_off_at_the_default_line() {
    let off = settings("");
    assert_eq!(off, HandoverSettings::default());
    assert!(!off.context && !off.idle);
    assert_eq!(off.limits, HandoverLimits::default());
    assert_eq!(settings("auto_handover = []"), off);
}

#[test]
fn each_reason_switches_on_only_itself() {
    let context = settings("auto_handover = ['context']");
    assert!(context.context && !context.idle);
    assert_eq!(context.limits, HandoverLimits::default());
    let idle = settings("auto_handover = ['idle']\ncache_minutes = 60");
    assert!(idle.idle && !idle.context);
}

#[test]
fn an_unknown_or_repeated_reason_makes_the_role_invalid() {
    assert!(refused("auto_handover = ['task']").contains("unknown reason \"task\""));
    assert!(refused("auto_handover = ['context', 'context']").contains("listed twice"));
    assert!(role("auto_handover = 'context'").is_err());
    assert_eq!(
        handover_settings(&RoleConfig {
            auto_handover: vec!["Context".into()],
            ..role("").unwrap().config
        }),
        Err(SettingsError::UnknownReason("Context".into()))
    );
}

#[test]
fn the_context_line_is_one_to_a_hundred() {
    assert_eq!(settings("context_line = 1").limits.context_line_percent, 1);
    assert_eq!(
        settings("context_line = 100").limits.context_line_percent,
        100
    );
    assert!(refused("context_line = 0").contains("1 to 100"));
    assert!(refused("context_line = 101").contains("1 to 100"));
    assert!(role("context_line = 300").is_err());
    assert!(role("context_line = -1").is_err());
}

#[test]
fn the_idle_rule_needs_a_cache_life_from_the_role() {
    assert!(refused("auto_handover = ['idle']").contains("needs cache_minutes"));
    assert!(refused("idle_minutes = 4").contains("needs cache_minutes"));
    // A cache life alone switches nothing on.
    let stated = settings("cache_minutes = 60");
    assert!(!stated.idle && !stated.context);
}

#[test]
fn the_idle_limit_defaults_to_four_fifths_and_stays_below_the_cache_life() {
    let cache = |keys: &str| settings(keys).limits.cache.unwrap();
    let limits = |lifetime: u64, idle_limit: u64| CacheLimits {
        lifetime: Duration::from_secs(lifetime * 60),
        idle_limit: Duration::from_secs(idle_limit * 60),
    };
    assert_eq!(cache("cache_minutes = 60"), limits(60, 48));
    assert_eq!(cache("cache_minutes = 5"), limits(5, 4));
    assert_eq!(cache("cache_minutes = 2"), limits(2, 1));
    assert_eq!(
        cache("cache_minutes = 60\nidle_minutes = 59"),
        limits(60, 59)
    );
    assert_eq!(cache("cache_minutes = 60\nidle_minutes = 1"), limits(60, 1));
    assert_eq!(
        cache(&format!("cache_minutes = {}", u32::MAX)),
        limits(u64::from(u32::MAX), u64::from(u32::MAX) / 5 * 4)
    );
    assert!(refused("cache_minutes = 1").contains("at least 2"));
    assert!(refused("cache_minutes = 0").contains("at least 2"));
    for idle in [0, 60, 61] {
        let problem = refused(&format!("cache_minutes = 60\nidle_minutes = {idle}"));
        assert!(problem.contains("below cache_minutes (60)"), "{problem}");
    }
}

#[test]
fn a_role_without_the_keys_serializes_as_before() {
    let body = serde_json::to_value(role("").unwrap()).unwrap();
    for key in [
        "auto_handover",
        "context_line",
        "cache_minutes",
        "idle_minutes",
    ] {
        assert!(body.get(key).is_none(), "{key} must be left out");
    }
    let on = role("auto_handover = ['context']\ncontext_line = 30").unwrap();
    let body = serde_json::to_value(&on).unwrap();
    assert_eq!(body["auto_handover"], serde_json::json!(["context"]));
    assert_eq!(body["context_line"], 30);
    assert_eq!(serde_json::from_value::<RoleSpec>(body).unwrap(), on);
}

// What an agent waits for.

#[test]
fn an_agent_with_no_task_and_no_decision_waits_for_nothing() {
    let mut history = History::new();
    // Other agents' work is not the worker's wait.
    let task = history.task(Address::Owner, mechanical());
    history.assign(&task, &agent("other"));
    history.decision(Address::Agent(agent("other")), None);
    assert_eq!(waits(&history.state, &worker()), [WaitsFor::Nothing]);
}

#[test]
fn a_task_the_agent_holds_is_another_agents_move_in_every_open_state() {
    for state in [
        TaskState::Assigned,
        TaskState::Working,
        TaskState::Waiting,
        TaskState::Blocked,
        TaskState::ChangesRequested,
        TaskState::Submitted,
        TaskState::InReview,
        TaskState::Unknown,
    ] {
        let mut history = History::new();
        history.held(state, mechanical());
        assert_eq!(
            waits(&history.state, &worker()),
            [WaitsFor::Agent],
            "{state:?}"
        );
    }
}

#[test]
fn a_finished_task_is_no_wait() {
    for state in [TaskState::Accepted, TaskState::Cancelled] {
        let mut history = History::new();
        history.held(state, mechanical());
        assert_eq!(
            waits(&history.state, &worker()),
            [WaitsFor::Nothing],
            "{state:?}"
        );
    }
}

#[test]
fn a_handed_in_task_the_owner_judges_waits_for_the_owner() {
    for (state, expected) in [
        (TaskState::Submitted, WaitsFor::Owner),
        (TaskState::InReview, WaitsFor::Owner),
        // Before it is handed in, the next move is not the owner's.
        (TaskState::Working, WaitsFor::Agent),
        (TaskState::Waiting, WaitsFor::Agent),
    ] {
        let mut history = History::new();
        history.held(state, TaskMeasure::Owner);
        assert_eq!(waits(&history.state, &worker()), [expected], "{state:?}");
    }
}

#[test]
fn a_task_the_agent_created_counts_only_while_another_agent_holds_it() {
    let mut history = History::new();
    let task = history.task(Address::Agent(worker()), mechanical());
    // Nobody holds it: nothing is coming.
    assert_eq!(waits(&history.state, &worker()), [WaitsFor::Nothing]);
    history.assign(&task, &agent("other"));
    assert_eq!(waits(&history.state, &worker()), [WaitsFor::Agent]);
    history.set(&task, TaskState::Accepted);
    assert_eq!(waits(&history.state, &worker()), [WaitsFor::Nothing]);
}

#[test]
fn a_pending_decision_waits_for_the_owner_and_for_its_deadline() {
    let mut history = History::new();
    let open = history.decision(Address::Agent(worker()), None);
    assert_eq!(waits(&history.state, &worker()), [WaitsFor::Owner]);
    let deadline = after(30 * MINUTE);
    history.decision(Address::Agent(worker()), Some(deadline));
    assert_eq!(
        waits(&history.state, &worker()),
        [WaitsFor::Owner, WaitsFor::Owner, WaitsFor::Until(deadline)]
    );
    // An answered decision is no wait.
    let mut answered = History::new();
    let id = answered.decision(Address::Agent(worker()), None);
    assert_eq!(id, open);
    answered.push(OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
        decision_id: id,
        option: "a".into(),
        by: Address::System,
    }));
    assert_eq!(waits(&answered.state, &worker()), [WaitsFor::Nothing]);
}

// Rows 1 to 4: nothing is looked at.

#[test]
fn only_a_ready_agent_bound_to_the_session_is_judged() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    assert_eq!(
        auto_step(
            &look(&history, &settings, &worker, &session, 90.0),
            after(0)
        ),
        RUN_LINE
    );
    // Another session than the agent's, an unknown agent.
    let elsewhere = SessionId::new("s2");
    let unknown = agent("nobody");
    for input in [
        look(&history, &settings, &worker, &elsewhere, 90.0),
        look(&history, &settings, &unknown, &session, 90.0),
    ] {
        assert_eq!(auto_step(&input, after(LIFETIME - 1)), AutoStep::Nothing);
    }
    // Clearing: the agent has left its session.
    let mut clearing = History::new();
    clearing.push(OrgEvent::AgentClearRequested {
        agent_id: worker.clone(),
    });
    assert_eq!(
        auto_step(
            &look(&clearing, &settings, &worker, &session, 90.0),
            after(0)
        ),
        AutoStep::Nothing
    );
    // Stuck in the same session.
    let mut stuck = History::new();
    stuck.push(OrgEvent::AgentStuck {
        agent_id: worker.clone(),
        reason: "why".into(),
    });
    assert_eq!(
        stuck.state.agent(&worker).unwrap().session_id,
        Some(session.clone())
    );
    assert_eq!(
        auto_step(&look(&stuck, &settings, &worker, &session, 90.0), after(0)),
        AutoStep::Nothing
    );
}

#[test]
fn without_a_turn_end_on_record_nothing_is_done() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    let input = AutoInput {
        idle_since: None,
        ..look(&history, &settings, &worker, &session, 99.0)
    };
    assert_eq!(auto_step(&input, after(IDLE_LIMIT)), AutoStep::Nothing);
}

#[test]
fn background_work_and_an_earlier_attempt_each_stop_everything() {
    let history = History::new();
    let (worker, session) = (worker(), session());
    // Off, so that at the line the answer would otherwise be a flag.
    for settings in [both(), HandoverSettings::default()] {
        let base = look(&history, &settings, &worker, &session, 90.0);
        let background = AutoInput {
            background: true,
            ..base
        };
        let attempted = AutoInput {
            attempted: true,
            ..base
        };
        for input in [background, attempted] {
            for secs in [0, IDLE_LIMIT] {
                assert_eq!(auto_step(&input, after(secs)), AutoStep::Nothing);
            }
        }
    }
}

// Rows 6 and 10: the context line.

#[test]
fn at_the_line_a_seat_is_flagged_when_off_and_handed_over_when_on() {
    let history = History::new();
    let (worker, session) = (worker(), session());
    let off = HandoverSettings::default();
    let on = settings("auto_handover = ['context']");
    for (settings, at_line) in [(&off, AutoStep::Flag), (&on, RUN_LINE)] {
        let step = |share| {
            auto_step(
                &look(&history, settings, &worker, &session, share),
                after(0),
            )
        };
        assert_eq!(step(60.0), at_line);
        assert_eq!(step(59.99), AutoStep::Nothing);
    }
    // The idle switch alone does not hand over at the line.
    let idle = settings("auto_handover = ['idle']\ncache_minutes = 60");
    assert_eq!(
        auto_step(&look(&history, &idle, &worker, &session, 60.0), after(0)),
        AutoStep::Flag
    );
}

#[test]
fn the_line_is_the_seats_own() {
    let history = History::new();
    let (worker, session) = (worker(), session());
    let on = settings("auto_handover = ['context']\ncontext_line = 70");
    let off = settings("context_line = 70");
    let step = |settings, share| {
        auto_step(
            &look(&history, settings, &worker, &session, share),
            after(0),
        )
    };
    assert_eq!(step(&on, 69.9), AutoStep::Nothing);
    assert_eq!(step(&on, 70.0), RUN_LINE);
    assert_eq!(step(&off, 69.9), AutoStep::Nothing);
    assert_eq!(step(&off, 70.0), AutoStep::Flag);
}

#[test]
fn an_unknown_share_never_reaches_the_line() {
    let history = History::new();
    let on = settings("auto_handover = ['context']\ncontext_line = 1");
    let (worker, session) = (worker(), session());
    for share in [None, Some(f64::NAN), Some(0.0)] {
        let input = AutoInput {
            context_percent: share,
            ..look(&history, &on, &worker, &session, 0.0)
        };
        assert_eq!(auto_step(&input, after(0)), AutoStep::Nothing);
    }
}

#[test]
fn the_context_rule_does_not_wait_for_the_idle_limit_or_ask_what_comes() {
    let mut history = History::new();
    history.held(TaskState::Working, mechanical());
    let on = settings("auto_handover = ['context']");
    let (worker, session) = (worker(), session());
    for secs in [0, 10 * LIFETIME] {
        assert_eq!(
            auto_step(&look(&history, &on, &worker, &session, 65.0), after(secs)),
            RUN_LINE
        );
    }
}

// Rows 7 and 11: the idle rule.

#[test]
fn before_the_idle_limit_the_step_asks_to_look_again_at_the_limit() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    let step = |secs| {
        auto_step(
            &look(&history, &settings, &worker, &session, 10.0),
            after(secs),
        )
    };
    assert_eq!(step(0), AutoStep::LookAgainAt(after(IDLE_LIMIT)));
    assert_eq!(
        step(IDLE_LIMIT - 1),
        AutoStep::LookAgainAt(after(IDLE_LIMIT))
    );
    assert_eq!(step(IDLE_LIMIT), run_expiring());
    assert_eq!(step(LIFETIME - 1), run_expiring());
}

#[test]
fn without_the_idle_switch_nothing_is_scheduled() {
    let history = History::new();
    let (worker, session) = (worker(), session());
    for settings in [
        settings("auto_handover = ['context']\ncache_minutes = 60\nidle_minutes = 50"),
        settings("cache_minutes = 60"),
        HandoverSettings::default(),
    ] {
        for secs in [0, IDLE_LIMIT, 10 * LIFETIME] {
            assert_eq!(
                auto_step(
                    &look(&history, &settings, &worker, &session, 10.0),
                    after(secs)
                ),
                AutoStep::Nothing
            );
        }
    }
}

#[test]
fn a_wait_that_comes_in_time_keeps_the_session_and_is_not_asked_about_again() {
    let mut history = History::new();
    history.held(TaskState::Waiting, mechanical());
    let settings = both();
    let (worker, session) = (worker(), session());
    let step = |secs| {
        auto_step(
            &look(&history, &settings, &worker, &session, 10.0),
            after(secs),
        )
    };
    assert_eq!(
        step(IDLE_LIMIT - 1),
        AutoStep::LookAgainAt(after(IDLE_LIMIT))
    );
    assert_eq!(step(IDLE_LIMIT), AutoStep::Nothing);
    assert_eq!(step(LIFETIME - 1), AutoStep::Nothing);
}

#[test]
fn one_wait_that_comes_in_time_is_enough_to_keep_the_session() {
    let settings = both();
    let (worker, session) = (worker(), session());
    // The owner is away, so his answer does not come; the deadline decides.
    let step = |deadline_secs: Option<i64>, task: bool| {
        let mut history = History::new();
        history.decision(Address::Agent(worker.clone()), deadline_secs.map(after));
        if task {
            history.held(TaskState::Working, mechanical());
        }
        let input = AutoInput {
            away: true,
            ..look(&history, &settings, &worker, &session, 10.0)
        };
        auto_step(&input, after(IDLE_LIMIT))
    };
    assert_eq!(step(None, false), run_expiring());
    assert_eq!(step(Some(LIFETIME), false), run_expiring());
    assert_eq!(step(Some(LIFETIME - 1), false), AutoStep::Nothing);
    assert_eq!(step(None, true), AutoStep::Nothing);
}

#[test]
fn the_owner_answers_in_time_unless_he_is_away() {
    let mut history = History::new();
    history.decision(Address::Agent(worker()), None);
    let settings = both();
    let (worker, session) = (worker(), session());
    let here = look(&history, &settings, &worker, &session, 10.0);
    let away = AutoInput { away: true, ..here };
    assert_eq!(auto_step(&here, after(IDLE_LIMIT)), AutoStep::Nothing);
    assert_eq!(auto_step(&away, after(IDLE_LIMIT)), run_expiring());
}

#[test]
fn the_idle_limits_are_the_roles() {
    let history = History::new();
    let short = settings("auto_handover = ['idle']\ncache_minutes = 5");
    let (worker, session) = (worker(), session());
    let step = |secs| {
        auto_step(
            &look(&history, &short, &worker, &session, 10.0),
            after(secs),
        )
    };
    assert_eq!(
        step(4 * MINUTE - 1),
        AutoStep::LookAgainAt(after(4 * MINUTE))
    );
    assert_eq!(
        step(4 * MINUTE),
        AutoStep::Run {
            reason: HandoverReason::CacheExpiring,
            give_up_at: Some(after(5 * MINUTE)),
        }
    );
}

#[test]
fn the_line_wins_over_the_idle_rule_when_both_are_due() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    assert_eq!(
        auto_step(
            &look(&history, &settings, &worker, &session, 90.0),
            after(IDLE_LIMIT)
        ),
        RUN_LINE
    );
}

#[test]
fn at_the_line_with_only_the_idle_switch_the_idle_rule_still_runs() {
    let history = History::new();
    let idle = settings("auto_handover = ['idle']\ncache_minutes = 60\nidle_minutes = 50");
    let (worker, session) = (worker(), session());
    let step = |secs| auto_step(&look(&history, &idle, &worker, &session, 90.0), after(secs));
    assert_eq!(step(IDLE_LIMIT - 1), AutoStep::Flag);
    assert_eq!(step(IDLE_LIMIT), run_expiring());
}

// Row 5: a cold cache.

#[test]
fn a_cold_session_is_never_run_and_never_cleared() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    let step = |share, secs| {
        auto_step(
            &look(&history, &settings, &worker, &session, share),
            after(secs),
        )
    };
    assert_eq!(step(10.0, LIFETIME - 1), run_expiring());
    assert_eq!(step(10.0, LIFETIME), AutoStep::Nothing);
    assert_eq!(step(90.0, LIFETIME - 1), RUN_LINE);
    assert_eq!(step(90.0, LIFETIME), AutoStep::Flag);
    assert_eq!(step(90.0, 10 * LIFETIME), AutoStep::Flag);
}

#[test]
fn a_seat_without_the_idle_switch_is_never_cold() {
    let history = History::new();
    let context = settings("auto_handover = ['context']\ncache_minutes = 60\nidle_minutes = 50");
    let (worker, session) = (worker(), session());
    assert_eq!(
        auto_step(
            &look(&history, &context, &worker, &session, 90.0),
            after(10 * LIFETIME)
        ),
        RUN_LINE
    );
}

// Row 8: a session that has only started.

#[test]
fn a_session_that_has_not_worked_is_not_handed_over() {
    let history = History::new();
    let settings = both();
    let (worker, session) = (worker(), session());
    let step = |share, secs| {
        let input = AutoInput {
            worked: false,
            ..look(&history, &settings, &worker, &session, share)
        };
        auto_step(&input, after(secs))
    };
    assert_eq!(step(90.0, 0), AutoStep::Flag);
    assert_eq!(step(10.0, IDLE_LIMIT), AutoStep::Nothing);
    // Until the limit it is looked at again like any session.
    assert_eq!(step(10.0, 0), AutoStep::LookAgainAt(after(IDLE_LIMIT)));
}

// Row 9: mail on its way.

#[test]
fn mail_on_its_way_puts_the_handover_off_by_a_minute() {
    let settings = both();
    let (worker, session) = (worker(), session());
    let again =
        |secs: i64| AutoStep::LookAgainAt(after(secs) + TimeDelta::from_std(LOOK_AGAIN).unwrap());
    assert_eq!(LOOK_AGAIN, Duration::from_secs(60));

    let mut queued = History::new();
    let id = queued.mail(&worker);
    let step = |history: &History, share, secs| {
        auto_step(
            &look(history, &settings, &worker, &session, share),
            after(secs),
        )
    };
    assert_eq!(step(&queued, 90.0, 7), again(7));
    assert_eq!(step(&queued, 10.0, IDLE_LIMIT), again(IDLE_LIMIT));

    let mut delivering = History::new();
    let id2 = delivering.mail(&worker);
    assert_eq!(id, id2);
    delivering.push(OrgEvent::MessageDeliveryAttempted {
        message_id: id,
        attempt: 1,
        pane: "%1".into(),
    });
    assert_eq!(
        delivering.state.message(&id).unwrap().status,
        DeliveryStatus::Delivering
    );
    assert_eq!(step(&delivering, 90.0, 7), again(7));

    // Delivered or failed mail, and mail to someone else, is not on its way.
    let mut delivered = History::new();
    let id = delivered.mail(&worker);
    delivered.push(OrgEvent::MessageDelivered {
        message_id: id,
        session_id: session.clone(),
        confirmation: "prompt".into(),
    });
    let mut failed = History::new();
    let id = failed.mail(&worker);
    failed.push(OrgEvent::MessageDeliveryFailed {
        message_id: id,
        reason: "gone".into(),
        final_failure: true,
    });
    let mut others = History::new();
    others.mail(&agent("other"));
    for history in [&delivered, &failed, &others] {
        assert_eq!(step(history, 90.0, 7), RUN_LINE);
    }
}

#[test]
fn mail_does_not_hold_back_the_flag_of_a_seat_that_is_off() {
    let mut history = History::new();
    history.mail(&worker());
    let off = HandoverSettings::default();
    let (worker, session) = (worker(), session());
    assert_eq!(
        auto_step(&look(&history, &off, &worker, &session, 90.0), after(0)),
        AutoStep::Flag
    );
}

// Times the calendar cannot hold.

#[test]
fn a_time_the_calendar_cannot_hold_is_never_due() {
    let history = History::new();
    let (worker, session) = (worker(), session());
    let huge = HandoverSettings {
        context: false,
        idle: true,
        limits: HandoverLimits {
            context_line_percent: 40,
            cache: Some(CacheLimits {
                lifetime: Duration::MAX,
                idle_limit: Duration::from_secs(60),
            }),
        },
    };
    let input = look(&history, &huge, &worker, &session, 10.0);
    assert_eq!(auto_step(&input, after(60)), AutoStep::Nothing);
    let mut mailed = History::new();
    mailed.mail(&worker);
    let on = settings("auto_handover = ['context']");
    let late = AutoInput {
        idle_since: Some(DateTime::<Utc>::MAX_UTC),
        ..look(&mailed, &on, &worker, &session, 90.0)
    };
    assert_eq!(
        auto_step(&late, DateTime::<Utc>::MAX_UTC),
        AutoStep::Nothing
    );
}
