//! SEALED REPLAY sidecars: original captured envelopes, owned stand-in process
//! and git/transcript files. These are not the historical harness environment.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use atm_core::{Harness, LifecycleContext, LifecycleEvent, SessionId};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::replay_tests::{drive, handler};
use super::*;
use crate::registry::{replay_support::Driver, RegistryCommand};

const SOURCES: [(&str, &str, &str, Harness); 7] = [
    (
        "claude_interleaved_2.1.310",
        "interleaved",
        include_str!("../../tests/fixtures/claude_tui_interleaved_2.1.310.jsonl"),
        Harness::ClaudeCode,
    ),
    (
        "claude_turn_boundaries_2.1.325",
        "hook",
        include_str!(
            "../../../atm-claude-adapter/tests/fixtures/claude_turn_boundaries_2.1.325.jsonl"
        ),
        Harness::ClaudeCode,
    ),
    (
        "pi_tool_run_0.73.1",
        "pi",
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_tool_run_0.73.1.jsonl"),
        Harness::Pi,
    ),
    (
        "pi_new_session_0.73.1",
        "pi",
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_new_session_0.73.1.jsonl"),
        Harness::Pi,
    ),
    (
        "pi_session_cost_0.73.1",
        "pi",
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl"),
        Harness::Pi,
    ),
    (
        "pi_reconnect_1.5.2",
        "pi",
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_reconnect_1.5.2.jsonl"),
        Harness::Pi,
    ),
    (
        "codex_tui_session_0.163.0",
        "codex",
        include_str!("../../../atm-codex-adapter/tests/fixtures/codex_tui_session_0.163.0.jsonl"),
        Harness::Codex,
    ),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/registry_replay")
        .join(format!("{name}.json"))
}

struct StandIn {
    dir: tempfile::TempDir,
    child: Child,
}
impl StandIn {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        // No inherited GIT_* variables or config can redirect this write.
        let git = |args: &[&str]| {
            let output = Command::new("timeout")
                .args(["30s", "git"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", std::env::var_os("HOME").unwrap())
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["-c", "init.defaultBranch=main", "init"]);
        let actual = PathBuf::from(git(&["rev-parse", "--absolute-git-dir"]));
        assert!(actual.starts_with(dir.path()));
        std::fs::write(
            dir.path().join("rollout.jsonl"),
            include_str!(
                "../../../atm-codex-adapter/tests/fixtures/codex_rollout_tokens_0.163.0.jsonl"
            ),
        )
        .unwrap();
        let child = Command::new("cat")
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Self { dir, child }
    }
    fn stop(&mut self) {
        drop(self.child.stdin.take());
        self.child.wait().unwrap();
    }
}
impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn normalize(v: &mut Value, root: &str, pid: u32) {
    let text = |s: &str| {
        // The view shortens a long cwd. Scrub that derived path too; changing
        // the domain path alone would leave a machine-specific suffix behind.
        let repo = format!("{root}/repo");
        let shortened = format!("...{}", repo.get(repo.len().saturating_sub(27)..).unwrap());
        let mut s = s
            .replace(&shortened, "$SEALED/repo")
            .replace(root, "$SEALED")
            .replace("/home/user", "$CAPTURE_HOME");
        s = s.replace(&format!("pending-{pid}"), "pending-900001");
        if s == pid.to_string() {
            s = "900001".into();
        }
        if let Some(prefix) = s.strip_suffix(&format!(":{pid}")) {
            s = format!("{prefix}:900001");
        }
        s
    };
    match v {
        Value::String(s) => *s = text(s),
        Value::Number(n) if n.as_u64() == Some(u64::from(pid)) => *v = json!(900001),
        Value::Array(a) => {
            for v in a {
                normalize(v, root, pid);
            }
        }
        Value::Object(o) => {
            let old = std::mem::take(o);
            for (key, mut value) in old {
                normalize(&mut value, root, pid);
                assert!(o.insert(text(&key), value).is_none());
            }
        }
        _ => {}
    }
}

fn synthetic(
    driver: &mut Driver,
    pid: u32,
    event: LifecycleEvent,
    context: LifecycleContext,
    id: &str,
) {
    driver.accept(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new(id),
        event,
        harness: Harness::Pi,
        pid: Some(pid),
        tmux_pane: Some("%1".into()),
        context,
        respond_to: oneshot::channel().0,
    });
}

async fn capture(name: &str, mode: &str, source: &str, harness: Harness) -> Value {
    let mut standin = tokio::task::spawn_blocking(StandIn::new).await.unwrap();
    let root = standin.dir.path().to_str().unwrap().to_owned();
    let repo = standin.dir.path().join("repo");
    let pid = standin.child.id();
    let raw_dir = standin.dir.path().join("state");
    let raw = crate::org::RawHandle::open(raw_dir.clone()).await;
    let mut driver = Driver::recording();
    let mut handler = handler(&driver).with_raw(raw.clone());
    // Synthetic discovery attribution to the owned process, then original
    // envelope order. Offsets are not sleeps: clock answers are recorded.
    let registry = driver.handle.clone();
    drive(
        &mut driver,
        registry.register_discovered(
            SessionId::pending_from_pid(pid),
            pid,
            repo.clone(),
            Some("%1".into()),
            harness,
        ),
    )
    .await
    .unwrap();
    driver.capture_pending_git().await;
    let mut frames = 0;
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        let (kind, mut data) = match mode {
            "interleaved" => (
                if row["kind"] == "hook" {
                    "hook"
                } else {
                    "status"
                },
                row["payload"].clone(),
            ),
            "pi" => ("pi", row["data"].clone()),
            _ => (mode, row),
        };
        data["pid"] = json!(pid);
        data["tmux_pane"] = json!("%1");
        data["cwd"] = json!(repo);
        data["transcript_path"] = json!(standin.dir.path().join("rollout.jsonl"));
        if data.get("workspace").is_some() {
            data["workspace"]["current_dir"] = json!(repo);
            data["workspace"]["project_dir"] = json!(repo);
        }
        let message = match kind {
            "hook" => ClientMessage::hook_event(data),
            "status" => ClientMessage::status_update(data),
            "pi" => ClientMessage::pi_event(data),
            "codex" => ClientMessage::codex_event(data),
            _ => unreachable!(),
        };
        drive(
            &mut driver,
            handler.handle_message(message, &mut JoinSet::new()),
        )
        .await
        .unwrap();
        driver.capture_pending_git().await;
        raw.flush().await.unwrap();
        frames += 1;
    }
    assert_eq!(raw.dropped(), 0);
    let count = tokio::task::spawn_blocking(move || {
        crate::eventlog::read_from(&raw_dir, crate::eventlog::Stream::Raw, 0)
            .map(Result::unwrap)
            .count()
    })
    .await
    .unwrap();
    assert_eq!(count, frames);
    // Synthetic continuation: alias and tombstones are invisible to views but
    // indispensable to later routing/rejection. Preserve nonempty checkpoints.
    synthetic(
        &mut driver,
        pid,
        LifecycleEvent::SessionStart {
            source: Some("startup".into()),
        },
        LifecycleContext::default(),
        "continued",
    );
    synthetic(
        &mut driver,
        pid,
        LifecycleEvent::WorkingStart,
        LifecycleContext {
            child: Some(atm_core::ChildAgent {
                reference: atm_core::ChildRef::Name("worker".into()),
                agent_type: atm_core::AgentType::GeneralPurpose,
            }),
            ..Default::default()
        },
        "continued",
    );
    synthetic(
        &mut driver,
        pid,
        LifecycleEvent::ChildSessionStart {
            id: Some("child-id".into()),
            role: None,
        },
        LifecycleContext {
            child_alias: Some(atm_core::ChildAlias {
                name: "worker".into(),
                id: "child-id".into(),
            }),
            ..Default::default()
        },
        "continued",
    );
    driver.accept(RegistryCommand::GetSession {
        session_id: SessionId::new("continued"),
        respond_to: oneshot::channel().0,
    });
    driver.accept(RegistryCommand::GetAllSessions {
        respond_to: oneshot::channel().0,
    });
    driver.accept(RegistryCommand::PaneSessions {
        pane: "%1".into(),
        respond_to: oneshot::channel().0,
    });
    driver.turns.last_mut().unwrap()["continuation"] = json!("alias");
    synthetic(
        &mut driver,
        pid,
        LifecycleEvent::WorkingEnd,
        LifecycleContext {
            child: Some(atm_core::ChildAgent {
                reference: atm_core::ChildRef::Name("worker".into()),
                agent_type: atm_core::AgentType::GeneralPurpose,
            }),
            ..Default::default()
        },
        "continued",
    );
    synthetic(
        &mut driver,
        pid,
        LifecycleEvent::SessionStart {
            source: Some("clear".into()),
        },
        LifecycleContext::default(),
        "replacement",
    );
    driver.turns.last_mut().unwrap()["continuation"] = json!("ended");
    driver.accept(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("continued"),
        pid: Some(pid),
        data: crate::server::parse_status_line(json!({"session_id":"continued"}))
            .unwrap()
            .1,
        harness: Harness::Pi,
        respond_to: oneshot::channel().0,
    });
    driver.accept(RegistryCommand::RefreshGitInfo);
    driver.capture_pending_git().await;
    let standin = tokio::task::spawn_blocking(move || {
        standin.stop();
        standin
    })
    .await
    .unwrap();
    driver.accept(RegistryCommand::CleanupStale);
    assert!(driver.turns.last().unwrap()["checkpoint"]["rows"]
        .as_object()
        .unwrap()
        .is_empty());
    driver.accept(RegistryCommand::Register {
        session: Box::new(atm_core::SessionDomain::new(
            SessionId::new("supplied"),
            atm_core::AgentType::GeneralPurpose,
            atm_core::Model::Unknown,
        )),
        respond_to: oneshot::channel().0,
    });
    driver.accept(RegistryCommand::Remove {
        session_id: SessionId::new("supplied"),
        respond_to: oneshot::channel().0,
    });
    let mut tape = json!({"label":"SEALED REPLAY", "source":name, "frames":frames, "raw_dropped":raw.dropped(), "turns":driver.turns});
    normalize(&mut tape, &root, pid);
    drop(standin);
    tape
}

#[tokio::test]
async fn captured_registry_replays_match_every_checkpoint_twice_and_continuation() {
    timeout(Duration::from_secs(60), async {
        for (name, mode, source, harness) in SOURCES {
            let tape = if std::env::var_os("ATM_RECORD_REGISTRY_REPLAY").is_some() {
                let tape = capture(name, mode, source, harness).await;
                let path = fixture(name);
                let serialized = serde_json::to_vec_pretty(&tape).unwrap();
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, serialized).unwrap();
                })
                .await
                .unwrap();
                tape
            } else {
                serde_json::from_str(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
            };
            let turns = tape["turns"].as_array().unwrap();
            assert!(turns.len() > source.lines().count());
            assert!(turns
                .iter()
                .any(|t| !t["checkpoint"]["rows"].as_object().unwrap().is_empty()));
            assert!(turns
                .iter()
                .any(|t| !t["checkpoint"]["aliases"].as_array().unwrap().is_empty()));
            assert!(turns
                .iter()
                .any(|t| !t["checkpoint"]["ended"].as_array().unwrap().is_empty()));
            assert!(turns.iter().any(|t| t["checkpoint"]["rows"]
                .as_object()
                .unwrap()
                .values()
                .any(|r| r["domain"]["worktree_branch"] == "main")));
            if harness == Harness::Codex {
                // Literal last token_count in the unchanged rollout fixture.
                assert!(turns
                    .iter()
                    .any(|t| t["action"]["command"]["event"]["tokens"] == 8830
                        && t["action"]["command"]["event"]["current_tokens"] == 7305));
            }
            let mut a = Driver::new();
            let mut b = Driver::new();
            let split = turns.len() / 2;
            // Prefix retained in a, then suffix, equals a fresh whole replay b.
            for turn in &turns[..split] {
                assert_eq!(
                    a.replay_turn(turn).await,
                    turn["checkpoint"],
                    "{name}: prefix"
                );
            }
            for turn in &turns[split..] {
                assert_eq!(
                    a.replay_turn(turn).await,
                    turn["checkpoint"],
                    "{name}: suffix"
                );
            }
            for turn in turns {
                assert_eq!(
                    b.replay_turn(turn).await,
                    turn["checkpoint"],
                    "{name}: whole"
                );
            }
        }
    })
    .await
    .unwrap();
}

// The interleaving fixture is exactly the union of the unchanged original
// hook/status captures; no hand-invented substitute is presented as a capture.
#[test]
fn interleaved_capture_preserves_original_envelopes() {
    let mut original: Vec<Value> =
        include_str!("../../../atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl")
            .lines()
            .chain(
                include_str!(
                    "../../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl"
                )
                .lines(),
            )
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let mut interleaved: Vec<Value> = SOURCES[0]
        .2
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["payload"].clone())
        .collect();
    original.sort_by_key(Value::to_string);
    interleaved.sort_by_key(Value::to_string);
    assert_eq!(interleaved, original);
}

#[test]
fn capture_git_ignores_inherited_repo_and_index() {
    if std::env::var_os("ATM_REPLAY_GIT_DECOY_CHILD").is_some() {
        let mut standin = StandIn::new();
        standin.stop();
        return;
    }
    let decoy = tempfile::tempdir().unwrap();
    let repo = decoy.path().join("decoy.git");
    std::fs::create_dir(&repo).unwrap();
    let sentinel = repo.join("config");
    let index = decoy.path().join("index");
    std::fs::write(&sentinel, b"sentinel config").unwrap();
    std::fs::write(&index, b"sentinel index").unwrap();
    let child = Command::new("timeout")
        .arg("30s")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::connection::replay_corpus::capture_git_ignores_inherited_repo_and_index",
            "--nocapture",
        ])
        .env("ATM_REPLAY_GIT_DECOY_CHILD", "1")
        .env("GIT_DIR", &repo)
        .env("GIT_INDEX_FILE", &index)
        .output()
        .unwrap();
    assert!(child.status.success(), "{child:?}");
    assert_eq!(std::fs::read(sentinel).unwrap(), b"sentinel config");
    assert_eq!(std::fs::read(index).unwrap(), b"sentinel index");
    assert_eq!(std::fs::read_dir(repo).unwrap().count(), 1);
}

#[tokio::test]
async fn incomplete_mismatched_and_unused_tapes_are_refused() {
    let original: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture(SOURCES[0].0)).unwrap()).unwrap();
    for mutation in [
        "missing-clock",
        "wrong-clock",
        "unused-clock",
        "missing-process",
        "unused-process",
    ] {
        let mut turn = original["turns"][0].clone();
        match mutation {
            "missing-clock" => {
                turn["inputs"]["clocks"].as_array_mut().unwrap().remove(0);
            }
            "wrong-clock" => turn["inputs"]["clocks"][0]["kind"] = json!("other"),
            "unused-clock" => turn["inputs"]["clocks"]
                .as_array_mut()
                .unwrap()
                .push(json!({"kind":"utc","value":"2020-01-01T00:00:00Z"})),
            "missing-process" => {
                turn["inputs"]["processes"].as_object_mut().unwrap().clear();
            }
            _ => turn["inputs"]["processes"]["start:unrequested"] = json!([null]),
        }
        let failure = tokio::spawn(async move { Driver::new().replay_turn(&turn).await })
            .await
            .unwrap_err();
        assert!(failure.is_panic(), "{mutation}");
    }
}
