//! Synthetic day-away/rework rehearsal: real private atmd/tmux, model-free seats.
//! Every wait is a committed event, stand-in barrier, or process/readiness event.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[allow(dead_code)]
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)]
#[path = "support/message_delivery.rs"]
mod support;
use serde_json::{json, Value};
use stand_in::{StandInMode, StandInPane};
use std::path::{Path, PathBuf};
use support::{Confirmation, MessageRig, Recipient, Watch};

fn ready(rig: &mut MessageRig, project: &str, name: &str, manager: &str) -> Recipient {
    let root = rig.test.rig.root().join(project);
    std::fs::create_dir_all(&root).unwrap();
    if project != "gamma" {
        let mut command = rig.test.rig.command(Path::new("git"));
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("--git-dir")
            .arg(root.join(".git"))
            .arg("--work-tree")
            .arg(&root)
            .arg("init");
        assert!(root
            .canonicalize()
            .unwrap()
            .starts_with(rig.test.rig.root()));
        assert!(rig.test.rig.run(&mut command).status.success());
        let mut probe = rig.test.rig.command(Path::new("git"));
        probe
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("--git-dir")
            .arg(root.join(".git"))
            .arg("rev-parse")
            .arg("--absolute-git-dir");
        assert_eq!(
            Path::new(lifecycle::checked(rig.test.rig.run(&mut probe)).trim()),
            root.join(".git")
        );
    }
    rig.test.project = root;
    rig.test
        .role(name, StandInMode::Claude, &format!("manager='{manager}'\n"));
    let cap = match project {
        "alpha" => 1,
        "beta" => 2,
        _ => 3,
    };
    std::fs::write(
        rig.test.project.join(".atm/away.toml"),
        format!("stacking_cap = {cap}\n"),
    )
    .unwrap();
    let config = lifecycle::started_config(StandInMode::Claude);
    let log = rig
        .test
        .next_stand_in(&format!("{project}-{name}"), &config);
    // The new placement shell signals its startup; no shared shell-prompt poll.
    let channel = format!("digest-ready-{project}-{name}");
    lifecycle::checked(rig.test.rig.tmux(&[
        "set-option",
        "-g",
        "default-size",
        &format!("{}x{}", config.columns, config.rows * 2 + 1),
    ]));
    let shell=format!("env PROMPT_COMMAND='tmux -S \"${{TMUX%%,*}}\" wait-for -S {channel}' /bin/bash --noprofile --norc");
    let placement = lifecycle::checked(rig.test.rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        rig.test.project.to_str().unwrap(),
        &shell,
    ]));
    lifecycle::checked(rig.test.rig.tmux(&["wait-for", &channel]));
    let up = rig.test.up_from(name, &placement).unwrap();
    assert_eq!(up["state"], "ready", "{up}");
    eprintln!("synthetic agent.up: {up}");
    let pane = rig.test.rig.observe_pane(
        up["pane"].as_str().unwrap().into(),
        StandInMode::Claude,
        log,
    );
    let session = rig
        .test
        .rig
        .wait_for_row(&pane, |row| !row.id.is_pending())
        .id
        .to_string();
    rig.test.rig.wait_ready(&pane, Some(&session));
    Recipient {
        pane,
        id: up["agent_id"].as_str().unwrap().into(),
        session: Some(session),
    }
}
fn consume(
    rig: &MessageRig,
    watch: &mut Watch,
    seat: &Recipient,
    id: &str,
    cursor: u64,
    finish: bool,
) {
    let delivered = support::about("msg.delivered", id);
    if watch.all(&delivered).is_empty() {
        watch.wait("confirmed inbox reference", &delivered);
    }
    rig.test.rig.wait_event(&seat.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == support::reference(id)
    });
    if finish {
        rig.finish_turn(&seat.pane);
        rig.test.rig.wait_ready(&seat.pane, seat.session.as_deref());
    }
}
fn request(rig: &MessageRig, seat: &Recipient, changes: bool) -> String {
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let mut watch = rig.watch();
    let result = rig.atm_json(
        Some(&seat.pane),
        &[
            "decisions",
            "request",
            "--kind",
            "plan",
            "--title",
            "Choose records",
            "--option",
            "small",
            "--option",
            "large",
            "--recommended",
            "small",
            "--may-be-undone",
            "true",
            "--changes-intent",
            if changes { "true" } else { "false" },
            "--format",
            "json",
        ],
    );
    let id = result["decision_id"].as_str().unwrap().to_owned();
    watch.wait("provisional take", |e| {
        e["kind"] == "decision.taken_provisionally" && e["body"]["decision_id"] == id
    });
    let note = watch.wait("canonical away notice", |e| {
        e["kind"] == "msg.sent"
            && e["body"]["to"] == seat.id
            && e["body"]["body"]
                .as_str()
                .unwrap_or("")
                .starts_with(&format!("Decision {id} "))
    });
    consume(
        rig,
        &mut watch,
        seat,
        &format!("m{}", note["seq"]),
        cursor,
        true,
    );
    id
}
fn create(
    rig: &MessageRig,
    project: &str,
    title: &str,
    decision: Option<&str>,
    parent: Option<&str>,
) -> String {
    rig.call("task.create",json!({"project_id":project,"title":title,"measure":{"kind":"owner"},"rests_on":decision,"parent":parent}),None).unwrap()["task_id"].as_str().unwrap().into()
}
fn assign(rig: &MessageRig, id: &str, seat: &Recipient, finish: bool) {
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let mut watch = rig.watch();
    let result = rig
        .call(
            "task.assign",
            json!({"task_id":id,"agent_id":seat.id}),
            None,
        )
        .unwrap();
    consume(
        rig,
        &mut watch,
        seat,
        result["notification"]["message_id"].as_str().unwrap(),
        cursor,
        finish,
    );
}
fn task(rig: &MessageRig, id: &str) -> Value {
    rig.call("task.get", json!({"task_id":id}), None).unwrap()["task"].clone()
}
fn accept(rig: &MessageRig, id: &str, seat: &Recipient, report: &Path) {
    rig.call(
        "task.submit",
        json!({"task_id":id,"what":"result","files":[{"path":report,"kind":"report"}]}),
        Some(&seat.pane),
    )
    .unwrap();
    rig.call(
        "task.verdict",
        json!({"task_id":id,"verdict":"accepted"}),
        None,
    )
    .unwrap();
}
fn ack(rig: &MessageRig, seat: Option<&StandInPane>, id: &str) {
    assert!(rig.atm(seat, &["msg", "ack", id]).status.success());
}

fn scenario(old: Option<&Path>) {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let config = rig.test.rig.root().join("config/atm/config.toml");
    let mut settings = std::fs::read_to_string(&config).unwrap();
    settings.push_str("\n[reminder]\nenabled = false\n");
    std::fs::write(config, settings).unwrap();
    rig.test
        .rig
        .set_daemon_env("ATM_SPAWN_BIN", rig.test.rig.launcher(StandInMode::Claude));
    rig.test.rig.restart_daemon();
    let alpha = ready(&mut rig, "alpha", "lead", "gamma/chief");
    let beta = ready(&mut rig, "beta", "lead", "gamma/chief");
    let gamma = ready(&mut rig, "gamma", "chief", "owner");
    assert!(
        !rig.test.project.join(".git").exists(),
        "gamma must really be a non-repository folder"
    );
    assert_eq!(
        rig.atm_json(Some(&alpha.pane), &["me", "--format", "json"])["manager"],
        gamma.id
    );
    let away = rig.atm_json(None, &["away", "--format", "json"]);
    assert_eq!(away["stacking_caps"], json!({"alpha":1,"beta":2,"gamma":3}));
    let a = request(&rig, &alpha, false);
    let b = request(&rig, &beta, true);
    let c = request(&rig, &gamma, false);
    let root = create(&rig, "alpha", "Store records", Some(&a), None);
    assign(&rig, &root, &beta, true);
    let artifact = rig.test.rig.root().join("alpha/records.txt");
    std::fs::write(&artifact, "accepted original output\n").unwrap();
    accept(&rig, &root, &beta, &artifact);
    let child = create(&rig, "alpha", "Read records", None, Some(&root));
    assign(&rig, &child, &gamma, false);
    rig.call(
        "task.set_state",
        json!({"task_id":child,"state":"working"}),
        Some(&gamma.pane),
    )
    .unwrap();
    let future = create(&rig, "alpha", "Future records", None, Some(&root));
    let other = create(&rig, "beta", "Independent decision work", Some(&b), None);
    assign(&rig, &other, &alpha, true);
    let held = create(&rig, "beta", "Future independent work", Some(&b), None);
    assert!(rig
        .call(
            "task.assign",
            json!({"task_id":held,"agent_id":beta.id}),
            None
        )
        .is_err());
    let back = rig.atm_json(None, &["back", "--format", "json"]);
    let digest = &back["digest"];
    assert_eq!(digest["counts"]["during_absence"], 3);
    assert_eq!(digest["counts"]["waiting_tasks"], 1);
    let ordered: Vec<_> = digest["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "decision")
        .map(|row| row["decision"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ordered, vec![a.as_str(), b.as_str(), c.as_str()]);
    let mut after = Value::Null;
    let mut all = vec![];
    loop {
        let page = rig
            .call(
                "owner.digest",
                json!({"snapshot_seq":digest["snapshot_seq"],"after":after,"limit":2}),
                None,
            )
            .unwrap();
        assert_eq!(page["counts"], digest["counts"]);
        all.extend(page["rows"].as_array().unwrap().iter().cloned());
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    assert_eq!(all, *digest["rows"].as_array().unwrap());
    let mut watch = rig.watch();
    let alpha_cursor = rig.test.rig.event_cursor(&alpha.pane);
    let beta_cursor = rig.test.rig.event_cursor(&beta.pane);
    let gamma_cursor = rig.test.rig.event_cursor(&gamma.pane);
    let answer = rig.atm_json(None, &["decide", &a, "large", "--format", "json"]);
    assert_eq!(answer["redo"]["task_ids"], json!([root, child]));
    assert_eq!(answer["redo"]["completed_count"], 0);
    let fact = watch.wait("one atomic overrule", |e| {
        e["kind"] == "decision.overruled" && e["body"]["decision_id"] == a
    });
    assert_eq!(fact["body"]["redo_tasks"], json!([root, child]));
    for id in [&root, &child] {
        let value = task(&rig, id);
        assert_eq!(value["state"], "open");
        assert_eq!(value["agent_id"], Value::Null);
        assert_eq!(value["submission"], Value::Null);
        assert_eq!(value["cost"], Value::Null);
    }
    assert_eq!(task(&rig, &future)["state"], "open");
    assert_eq!(task(&rig, &other)["state"], "assigned");
    assert!(rig
        .call(
            "task.submit",
            json!({"task_id":root,"what":"result"}),
            Some(&beta.pane)
        )
        .is_err());
    assert!(rig
        .call(
            "task.verdict",
            json!({"task_id":root,"verdict":"accepted"}),
            None
        )
        .is_err());
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "accepted original output\n",
        "overrule is not a file/Git rollback"
    );
    let canonical = watch.wait("owner answer", |e| {
        e["kind"] == "msg.sent"
            && e["body"]["to"] == alpha.id
            && e["body"]["body"]
                .as_str()
                .unwrap_or("")
                .starts_with(&format!("Decision {a} overruled:"))
    });
    let canonical_id = format!("m{}", canonical["seq"]);
    consume(&rig, &mut watch, &alpha, &canonical_id, alpha_cursor, true);
    assert!(
        rig.inbox_entry(Some(&alpha.pane), None, &canonical_id)["content"]["body"]
            .as_str()
            .unwrap()
            .contains("large")
    );
    ack(&rig, Some(&alpha.pane), &canonical_id);
    let is_redo = |e: &Value| {
        e["kind"] == "msg.sent"
            && e["body"]["body"]
                .as_str()
                .unwrap_or("")
                .contains(&format!("after decision {a} was overruled:"))
    };
    let mut notes = watch.all(is_redo);
    assert!(notes.len() <= 4, "duplicate redo notes");
    while notes.len() < 4 {
        notes.push(watch.wait("redo notice", is_redo));
    }
    for seat in [&beta, &gamma] {
        let note = notes.iter().find(|e| e["body"]["to"] == seat.id).unwrap();
        let id = format!("m{}", note["seq"]);
        if seat.id == gamma.id {
            // The original turn continues until the test releases its clock.
            assert!(rig
                .test
                .rig
                .pane_rows(&gamma.pane)
                .iter()
                .any(|row| row.status == atm_core::SessionStatus::Working));
            rig.finish_turn(&gamma.pane);
        }
        consume(
            &rig,
            &mut watch,
            seat,
            &id,
            if seat.id == beta.id {
                beta_cursor
            } else {
                gamma_cursor
            },
            true,
        );
        assert!(
            rig.inbox_entry(Some(&seat.pane), None, &id)["content"]["body"]
                .as_str()
                .unwrap()
                .contains("No files were reverted.")
        );
        ack(&rig, Some(&seat.pane), &id);
    }
    for note in notes.iter().filter(|e| e["body"]["to"] == "owner") {
        let id = format!("m{}", note["seq"]);
        rig.inbox_entry(None, Some("owner"), &id);
        ack(&rig, None, &id);
    }
    assert_eq!(
        rig.call("decision.get", json!({"id":a}), None).unwrap()["redo"]["completed_count"],
        0
    );
    if let Some(old) = old {
        rollback_view(&mut rig, old, &a, &root, &child);
        // The synthetic harness sends no periodic hooks: reconnect it with an observed turn.
        for seat in [&alpha, &beta, &gamma] {
            rig.hold_turn(&seat.pane);
            rig.finish_turn(&seat.pane);
            rig.test.rig.wait_for_row(&seat.pane, |row| {
                row.id.as_str() == seat.session.as_deref().unwrap()
                    && row.status == atm_core::SessionStatus::Idle
            });
            rig.test.rig.wait_ready(&seat.pane, seat.session.as_deref());
        }
    }
    assign(&rig, &root, &beta, true);
    std::fs::write(&artifact, "accepted revised output\n").unwrap();
    accept(&rig, &root, &beta, &artifact);
    let refreshed = rig.atm_json(None, &["back", "--format", "json"]);
    assert_eq!(refreshed["changed"], false);
    assert_eq!(refreshed["digest"]["absence"], digest["absence"]);
    let number = refreshed["digest"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "overrule_number")
        .unwrap();
    assert_eq!(
        (number["completed"].clone(), number["required"].clone()),
        (json!(1), json!(2))
    );
    rig.test.rig.restart_daemon();
    assert_eq!(
        rig.call(
            "owner.digest",
            json!({"snapshot_seq":refreshed["digest"]["snapshot_seq"]}),
            None
        )
        .unwrap(),
        refreshed["digest"]
    );
    let final_back = rig.atm_json(None, &["back", "--format", "json"]);
    assert_eq!(
        final_back["digest"]["counts"],
        refreshed["digest"]["counts"]
    );
    let events = rig.test.tail(0);
    let messages: Vec<_> = events
        .iter()
        .filter(|e| {
            e["kind"] == "msg.sent"
                && e["body"]["body"]
                    .as_str()
                    .unwrap_or("")
                    .contains(&format!("after decision {a} was overruled:"))
        })
        .collect();
    assert_eq!(
        messages.len(),
        4,
        "restart must not repeat any creator/assignee note"
    );
    assert_eq!(task(&rig, &root)["state"], "accepted");
    assert_eq!(task(&rig, &child)["state"], "open");
    rig.test.rig.close();
}

fn copy_log(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_log(&entry.path(), &target);
        } else {
            assert!(kind.is_file());
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn rollback_view(rig: &mut MessageRig, old: &Path, decision: &str, root: &str, child: &str) {
    use std::io::{BufRead, BufReader, Read};
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    use std::process::Stdio;
    use std::time::Duration;
    assert_eq!(task(rig, root)["state"], "open");
    assert_eq!(task(rig, child)["state"], "open");
    rig.test.rig.stop_daemon(); // Process-exit fence: the source log cannot change during the copy.
    let copy = rig.test.rig.root().join("rollback-copy");
    copy_log(&rig.test.rig.root().join("state/log"), &copy.join("log"));
    // Copy logs only, never the original PID/socket/lock identity. No owner install is run.
    let (reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let child_process = rig
        .test
        .rig
        .command(old)
        .env("ATM_STATE_DIR", &copy)
        .env("RUST_LOG", "info")
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer.try_clone().unwrap())))
        .stderr(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .unwrap();
    let pid = child_process.id();
    let child_process = stand_in::process::OwnedChild::new(child_process);
    let mut reader = BufReader::new(reader);
    let mut observed = String::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "older daemon exited before readiness: {observed}"
        );
        let ready = line.contains("Daemon server listening")
            && line.contains(rig.test.rig.socket().to_str().unwrap());
        observed.push_str(&line);
        if ready {
            break;
        }
    }
    // The exec handshake can precede procfs environment visibility; readiness is the fence.
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    for marker in [
        format!("ATM_STATE_DIR={}", copy.display()),
        format!("ATM_SOCKET={}", rig.test.rig.socket().display()),
    ] {
        assert!(
            env.split(|b| *b == 0)
                .any(|entry| entry == marker.as_bytes()),
            "missing {marker}"
        );
    }
    assert_eq!(std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(), old);
    let get = |id: &str| {
        support::call_on(
            &rig.test.rig.socket(),
            "task.get",
            json!({"task_id":id}),
            None,
        )
        .unwrap()
    };
    let old_root = get(root);
    let old_child = get(child);
    let answer = support::call_on(
        &rig.test.rig.socket(),
        "decision.get",
        json!({"id":decision}),
        None,
    )
    .unwrap();
    assert_eq!(answer["decision"]["status"], "overruled");
    assert_eq!(answer["decision"]["resolution"]["option"], "large");
    assert!(answer.get("redo").is_none());
    assert_eq!(
        old_root["task"]["state"], "accepted",
        "older replay does not invalidate accepted work"
    );
    assert_eq!(
        old_child["task"]["state"], "working",
        "older replay does not unassign in-flight work"
    );
    eprintln!(
        "rollback copy: answer={}, accepted task={}, in-flight task={}",
        answer["decision"]["status"], old_root["task"]["state"], old_child["task"]["state"]
    );
    rig.test.rig.stop_daemon(); // Rechecks this private root's process identity before signaling it.
    assert!(child_process.wait_with_output().unwrap().status.success());
    reader.read_to_string(&mut observed).unwrap();
    eprintln!("older daemon log:\n{observed}");
    rig.test.rig.start_daemon(); // Untouched new state, not the rollback copy.
    assert_eq!(task(rig, root)["state"], "open");
    assert_eq!(task(rig, child)["state"], "open");
}

#[test]
fn return_digest_rework_across_three_projects_survives_restart() {
    scenario(None);
}

#[test]
#[ignore = "manual: copy an older build into this worktree and set ATM_TEST_OLD_DAEMON"]
fn older_daemon_replays_answer_but_not_redo_on_private_copy() {
    let old = PathBuf::from(
        std::env::var_os("ATM_TEST_OLD_DAEMON").expect("explicit private older build required"),
    )
    .canonicalize()
    .unwrap();
    assert!(
        old.starts_with(env!("CARGO_MANIFEST_DIR")),
        "must be a worker-owned copy, not an installed daemon"
    );
    assert!(old.ends_with("target/debug/atmd"));
    assert_ne!(
        old,
        Path::new(env!("CARGO_BIN_EXE_atmd"))
            .canonicalize()
            .unwrap()
    );
    scenario(Some(&old));
}
