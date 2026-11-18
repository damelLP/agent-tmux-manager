//! Deterministic registry input tests. Synthetic schedules, not harness captures.

use std::collections::VecDeque;
use std::path::PathBuf;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use atm_core::{AgentType, BackgroundActivity, Harness, LifecycleContext, LifecycleEvent, Model};
use chrono::TimeZone;

use super::*;
use crate::registry::inputs::{Clock, ProcessInfo};

struct TestClock {
    wall: Mutex<DateTime<Utc>>,
    step: chrono::Duration,
    instant: Instant,
}

impl TestClock {
    fn fixed() -> Arc<Self> {
        Arc::new(Self {
            wall: Mutex::new(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
            step: chrono::Duration::zero(),
            instant: Instant::now(),
        })
    }
}

impl Clock for TestClock {
    fn utc_now(&self) -> DateTime<Utc> {
        let mut wall = self.wall.lock().unwrap();
        let now = *wall;
        *wall += self.step;
        now
    }

    fn instant_now(&self) -> Instant {
        self.instant
    }
}

fn clock_actor(clock: Arc<TestClock>) -> (RegistryActor, broadcast::Receiver<SessionEvent>) {
    let (_, receiver) = mpsc::channel(16);
    let (events, subscriber) = broadcast::channel(128);
    let mut actor = RegistryActor::with_git_resolver(receiver, events, |_| (None, None, None));
    actor.clock = clock;
    (actor, subscriber)
}

#[derive(Default)]
struct ScriptedProcess(Mutex<HashMap<String, VecDeque<serde_json::Value>>>);

impl ScriptedProcess {
    fn add(&self, query: &str, pid: u32, answers: Vec<serde_json::Value>) {
        assert!(self
            .0
            .lock()
            .unwrap()
            .insert(format!("{query}:{pid}"), answers.into())
            .is_none());
    }

    fn answer<T: serde::de::DeserializeOwned>(&self, query: &str, pid: u32) -> T {
        let key = format!("{query}:{pid}");
        let value = self
            .0
            .lock()
            .unwrap()
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("unrecorded process query {key}"));
        serde_json::from_value(value).unwrap()
    }

    fn assert_consumed(&self) {
        assert!(self.0.lock().unwrap().values().all(VecDeque::is_empty));
    }
}

impl ProcessInfo for ScriptedProcess {
    fn start_time(&self, pid: u32) -> Option<u64> {
        self.answer("start", pid)
    }
    fn alive(&self, pid: u32, start: u64) -> bool {
        self.answer(&format!("alive:{start}"), pid)
    }
    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        self.answer("cwd", pid)
    }
    fn parent(&self, pid: u32, harness: Harness) -> Option<SessionId> {
        self.answer(&format!("parent:{harness:?}"), pid)
    }
}

fn status(id: &str) -> atm_core::StatusLineData {
    crate::server::parse_status_line(serde_json::json!({"session_id": id}))
        .unwrap()
        .1
}

fn lifecycle(actor: &mut RegistryActor, id: &str, pid: u32, event: LifecycleEvent) {
    actor
        .handle_apply_lifecycle_event(
            SessionId::new(id),
            event,
            Harness::Pi,
            Some(pid),
            Some("%1".into()),
            LifecycleContext::default(),
        )
        .unwrap();
}

// Catches any hidden real-clock constructor, activity, status or view read.
#[test]
fn all_registry_times_use_injected_clock() {
    let clock = TestClock::fixed();
    let t0 = *clock.wall.lock().unwrap();
    let (mut actor, mut events) = clock_actor(clock.clone());
    lifecycle(
        &mut actor,
        "parent",
        u32::MAX,
        LifecycleEvent::ToolCallEnd {
            name: atm_core::Tool::Bash,
            tool_use_id: None,
            is_error: false,
        },
    );
    let parent = &actor.sessions_by_pid.get(&u32::MAX).unwrap().0;
    assert_eq!((parent.started_at, parent.last_activity), (t0, t0));
    assert_eq!(parent.current_activity.as_ref().unwrap().started_at, t0);

    lifecycle(
        &mut actor,
        "parent",
        u32::MAX,
        LifecycleEvent::ChildSessionStart {
            id: Some("child".into()),
            role: None,
        },
    );
    let child_pid = *actor
        .session_id_to_pid
        .get(&SessionId::new("child"))
        .unwrap();
    actor.apply_to_session(
        child_pid,
        &LifecycleEvent::ToolCallEnd {
            name: atm_core::Tool::Bash,
            tool_use_id: None,
            is_error: false,
        },
        None,
    );
    let child = &actor.sessions_by_pid.get(&child_pid).unwrap().0;
    assert_eq!((child.started_at, child.last_activity), (t0, t0));
    assert_eq!(child.current_activity.as_ref().unwrap().started_at, t0);

    lifecycle(&mut actor, "parent", u32::MAX, LifecycleEvent::WorkingEnd);
    actor.apply_background_activity(
        u32::MAX,
        Some(BackgroundActivity {
            running: 1,
            scheduled: 0,
        }),
    );
    assert_eq!(
        actor
            .sessions_by_pid
            .get(&u32::MAX)
            .unwrap()
            .0
            .current_activity
            .as_ref()
            .unwrap()
            .started_at,
        t0
    );
    actor
        .handle_update_from_status_line(
            SessionId::new("status"),
            Some(u32::MAX - 1),
            status("status"),
            Harness::ClaudeCode,
        )
        .unwrap();
    *clock.wall.lock().unwrap() = t0 + chrono::Duration::seconds(90);
    actor
        .handle_update_from_status_line(
            SessionId::new("status"),
            None,
            status("status"),
            Harness::ClaudeCode,
        )
        .unwrap();
    let row = &actor.sessions_by_pid.get(&(u32::MAX - 1)).unwrap().0;
    assert_eq!(row.started_at, t0);
    assert_eq!(row.last_activity, t0 + chrono::Duration::seconds(90));

    for view in actor.handle_get_all_sessions() {
        assert_eq!(view.age_display, "1m ago");
    }
    assert_eq!(
        actor
            .handle_get_session(&SessionId::new("parent"))
            .unwrap()
            .last_activity_display,
        "1m ago"
    );
    assert!(actor
        .handle_pane_sessions("%1")
        .iter()
        .all(|row| row.view.age_display == "1m ago"));
    // Every view already has the injected stamps at publication: no observer
    // of Updated can see the discarded legacy constructor/status timestamps.
    while let Ok(event) = events.try_recv() {
        if let SessionEvent::Updated { session } = event {
            assert_eq!(session.started_at, t0.to_rfc3339());
            assert!(
                session.last_activity == t0.to_rfc3339()
                    || session.last_activity == (t0 + chrono::Duration::seconds(90)).to_rfc3339()
            );
        }
    }
}

// Catches collapsed constructor/status samples and accidental Register re-stamps.
#[test]
fn status_samples_stay_separate_and_register_is_untouched() {
    let clock = Arc::new(TestClock {
        step: chrono::Duration::seconds(1),
        ..Arc::try_unwrap(TestClock::fixed()).ok().unwrap()
    });
    let t0 = *clock.wall.lock().unwrap();
    let (mut actor, _) = clock_actor(clock);
    actor
        .handle_update_from_status_line(
            SessionId::new("status"),
            Some(u32::MAX),
            status("status"),
            Harness::ClaudeCode,
        )
        .unwrap();
    let row = &actor.sessions_by_pid.get(&u32::MAX).unwrap().0;
    // First sample checks the tombstone; the next two initialize the row.
    assert_eq!(row.started_at, t0 + chrono::Duration::seconds(1));
    assert_eq!(row.last_activity, t0 + chrono::Duration::seconds(2));
    let mut supplied = SessionDomain::new(
        SessionId::new("supplied"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    supplied.started_at = t0 - chrono::Duration::days(1);
    supplied.last_activity = t0 - chrono::Duration::hours(1);
    actor.handle_register(supplied.clone(), None).unwrap();
    let pid = actor.session_id_to_pid.get(&supplied.id).unwrap();
    let row = &actor.sessions_by_pid.get(pid).unwrap().0;
    assert_eq!(
        (row.started_at, row.last_activity),
        (supplied.started_at, supplied.last_activity)
    );
}

// Catches lost PID/starttime pairing, a read on zero/unchanged PID, ignored
// process observations, and a fallback from scripted inputs to host /proc.
#[test]
fn scripted_process_answers_preserve_pairing() {
    use serde_json::json;
    let process = Arc::new(ScriptedProcess::default());
    process.add("start", 41, vec![json!(100)]);
    process.add("start", 42, vec![json!(null)]);
    process.add("start", 43, vec![json!(200)]);
    process.add("alive:100", 41, vec![json!(true), json!(false)]);
    process.add("alive:200", 43, vec![json!(false)]);
    let (mut actor, _) = clock_actor(TestClock::fixed());
    actor.process_info = process.clone();
    let supplied = SessionDomain::new(
        SessionId::new("process"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    actor.handle_register(supplied, Some(41)).unwrap();
    let infra = &mut actor.sessions_by_pid.get_mut(&41).unwrap().1;
    assert_eq!(infra.pid, Some(41));
    for pid in [0, 41, 42] {
        infra.set_pid_with(pid, process.as_ref());
        assert_eq!(infra.pid, Some(41));
    }
    assert!(infra.is_process_alive_with(process.as_ref()));
    assert!(!infra.is_process_alive_with(process.as_ref()));
    infra.set_pid_with(43, process.as_ref());
    assert_eq!(infra.pid, Some(43));
    actor.handle_cleanup_stale();
    assert_eq!(actor.session_count(), 0);
    assert!(SessionInfrastructure::new().is_process_alive_with(process.as_ref()));
    process.assert_consumed();
}

// Catches cwd/parent read failures being replaced by host answers, and parent
// re-reads on rediscovery. Each query is keyed by PID/occurrence, not map order.
#[test]
fn scripted_cwd_and_parent_observations() {
    use serde_json::json;
    let process = Arc::new(ScriptedProcess::default());
    for pid in [51, 52, 53] {
        process.add("start", pid, vec![json!(pid)]);
    }
    process.add("cwd", 51, vec![json!("project")]);
    process.add("cwd", 52, vec![json!(null)]);
    process.add("parent:Pi", 51, vec![json!("ancestor")]);
    process.add("parent:Pi", 52, vec![json!(null)]);
    process.add("parent:Pi", 53, vec![json!("ancestor")]);
    let (mut actor, _) = clock_actor(TestClock::fixed());
    actor.process_info = process.clone();
    lifecycle(&mut actor, "event", 51, LifecycleEvent::WorkingStart);
    lifecycle(&mut actor, "unreadable", 52, LifecycleEvent::WorkingStart);
    actor
        .handle_register_discovered(
            SessionId::new("discovered"),
            53,
            PathBuf::from("elsewhere"),
            None,
            Harness::Pi,
        )
        .unwrap();
    actor
        .handle_register_discovered(
            SessionId::new("renamed"),
            53,
            PathBuf::from("other"),
            None,
            Harness::Pi,
        )
        .unwrap();
    for pid in [51, 53] {
        assert_eq!(
            actor.sessions_by_pid.get(&pid).unwrap().0.parent_session_id,
            Some(SessionId::new("ancestor"))
        );
    }
    let row = &actor.sessions_by_pid.get(&51).unwrap().0;
    assert_eq!(row.working_directory.as_deref(), Some("project"));
    let missing = &actor.sessions_by_pid.get(&52).unwrap().0;
    assert_eq!(
        (&missing.working_directory, &missing.parent_session_id),
        (&None, &None)
    );
    assert_eq!(
        actor
            .sessions_by_pid
            .get(&53)
            .unwrap()
            .0
            .working_directory
            .as_deref(),
        Some("other")
    );
    process.assert_consumed();
}

// Catches applying ready git work out of the recorded order, freeing a stuck
// slot early, failing to free a failed lookup, or waiting for workers on Drop.
#[tokio::test]
async fn ordered_git_completion_and_drop() {
    use super::replay_support::Driver;
    use serde_json::json;
    let clock = TestClock::fixed();
    let origin = clock.instant;
    let t0 = *clock.wall.lock().unwrap();
    let mut driver = Driver::new();
    driver.actor.clock = clock;
    let process = Arc::new(ScriptedProcess::default());
    process.add("start", u32::MAX, vec![json!(null)]);
    process.add("start", u32::MAX - 1, vec![json!(null)]);
    driver.actor.process_info = process;
    for (pid, cwd) in [(u32::MAX, "alpha"), (u32::MAX - 1, "beta")] {
        let mut session = SessionDomain::new(
            SessionId::new(cwd),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        session.started_at = t0;
        session.last_activity = t0;
        session.working_directory = Some(cwd.into());
        driver.actor.handle_register(session, Some(pid)).unwrap();
    }
    let initial = driver.checkpoint(origin, t0);
    assert_eq!(initial["git"]["alpha"]["stuck"], false);
    assert_eq!(initial["git"]["alpha"]["offset_ns"], "0");
    driver.actor.clock = Arc::new(TestClock {
        instant: origin + std::time::Duration::from_secs(5),
        ..Arc::try_unwrap(TestClock::fixed()).ok().unwrap()
    });
    driver.actor.handle_refresh_git_info();
    assert_eq!(driver.checkpoint(origin, t0)["git"]["alpha"]["stuck"], true);
    driver
        .step_git(
            "beta",
            Some((Some("root".into()), None, Some("main".into()))),
        )
        .await;
    assert_eq!(
        driver
            .actor
            .handle_get_session(&SessionId::new("beta"))
            .unwrap()
            .worktree_branch
            .as_deref(),
        Some("main")
    );
    assert!(driver
        .actor
        .handle_get_session(&SessionId::new("alpha"))
        .unwrap()
        .worktree_branch
        .is_none());
    assert_eq!(
        driver.checkpoint(origin, t0)["git"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    driver.step_git("alpha", None).await;
    assert_eq!(driver.checkpoint(origin, t0)["git"], json!({}));
    driver.actor.handle_refresh_git_info();
    assert_eq!(
        driver.checkpoint(origin, t0)["git"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    drop(driver);

    let (release, held) = std::sync::mpsc::channel();
    let held = Mutex::new(held);
    let (entered, mut arrivals) = mpsc::unbounded_channel();
    let (exited, done) = tokio::sync::oneshot::channel();
    let exited = Mutex::new(Some(exited));
    let (commands, receiver) = mpsc::channel(16);
    let (events, _) = broadcast::channel(16);
    let mut actor = RegistryActor::with_git_resolver(receiver, events, move |_| {
        entered.send(()).unwrap();
        held.lock().unwrap().recv().unwrap();
        exited.lock().unwrap().take().unwrap().send(()).unwrap();
        (None, None, None)
    });
    let mut supplied = SessionDomain::new(
        SessionId::new("held"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    supplied.working_directory = Some("held".into());
    actor.handle_register(supplied, None).unwrap();
    arrivals.recv().await.unwrap();
    let actor = tokio::spawn(actor.run());
    drop(commands);
    tokio::time::timeout(std::time::Duration::from_secs(30), actor)
        .await
        .unwrap()
        .unwrap();
    // Dependency is still stalled when shutdown completes; no duration claim.
    release.send(()).unwrap();
    done.await.unwrap();
}

// Catches replay/checkpoint code preserving only visible rows while dropping
// alias routing or ended-pair rejection. The same visible prefix must retain
// its hidden state for the captured continuation to route/reject correctly.
#[tokio::test]
async fn captured_continuations_require_hidden_aliases_and_tombstones() {
    use super::{replay_commands, replay_support::Driver};
    use serde_json::{json, Value};
    let tape: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/registry_replay/claude_interleaved_2.1.310.json"
    ))
    .unwrap();
    let turns = tape["turns"].as_array().unwrap();
    for marker in ["alias", "ended"] {
        let at = turns
            .iter()
            .position(|turn| turn["continuation"] == marker)
            .unwrap();
        let mut driver = Driver::new();
        for turn in &turns[..=at] {
            assert_eq!(driver.replay_turn(turn).await, turn["checkpoint"]);
        }
        let comparison = serde_json::from_value(turns[at]["comparison"].clone()).unwrap();
        let before = driver.checkpoint(driver.origin, comparison);
        match marker {
            "alias" => {
                assert_eq!(before["aliases"].as_array().unwrap().len(), 2);
                driver.actor.child_refs.clear();
            }
            _ => {
                assert!(!before["ended"].as_array().unwrap().is_empty());
                driver.actor.ended_sessions.clear();
            }
        }
        assert_eq!(
            driver.checkpoint(driver.origin, comparison)["rows"],
            before["rows"]
        );
        // Relax only the input tape here to inspect the misrouted state, not
        // to accept it as a valid replay. All answers remain host-free.
        driver.actor.clock = TestClock::fixed();
        let process = Arc::new(ScriptedProcess::default());
        process.add("start", 0x8000_0001, vec![json!(null)]);
        driver.actor.process_info = process;
        driver
            .actor
            .handle_command(replay_commands::decode(&turns[at + 1]["action"]["command"]));
        if marker == "alias" {
            assert_eq!(driver.actor.session_count(), 3);
            assert!(driver
                .actor
                .session_id_to_pid
                .contains_key(&SessionId::scoped("worker", &SessionId::new("continued"))));
        } else {
            assert!(driver
                .actor
                .session_id_to_pid
                .contains_key(&SessionId::new("continued")));
            assert!(!driver
                .actor
                .session_id_to_pid
                .contains_key(&SessionId::new("replacement")));
        }
    }
}

// Catches TTL inclusivity or backwards-wall-clock changes at the actor boundary.
#[test]
fn ended_marker_clock_boundaries() {
    for (millis, accepted) in [(-1, true), (0, false), (9999, false), (10000, true)] {
        let clock = TestClock::fixed();
        let t0 = *clock.wall.lock().unwrap();
        let (mut actor, _) = clock_actor(clock.clone());
        lifecycle(&mut actor, "ended", u32::MAX, LifecycleEvent::WorkingStart);
        lifecycle(
            &mut actor,
            "ended",
            u32::MAX,
            LifecycleEvent::SessionEnd { reason: None },
        );
        *clock.wall.lock().unwrap() = t0 + chrono::Duration::milliseconds(millis);
        actor
            .handle_update_from_status_line(
                SessionId::new("ended"),
                Some(u32::MAX),
                status("ended"),
                Harness::ClaudeCode,
            )
            .unwrap();
        assert_eq!(actor.session_count() == 1, accepted, "age={millis}");
        assert_eq!(
            actor
                .ended_sessions
                .get(&(SessionId::new("ended"), u32::MAX)),
            Some(&t0)
        );
    }
}
