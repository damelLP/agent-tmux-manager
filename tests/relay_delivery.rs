//! Spoken-answer relays through a private daemon/tmux and model-free stand-ins.
//! All synchronization is a committed event, a subscribed row or a manual turn.
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
use stand_in::StandInMode;
use support::{call_on, Confirmation, MessageRig, Recipient};

// Unlike ready_agent/shell_agent, launch acknowledges terminal frames by event.
fn seat(rig: &mut MessageRig, name: &str) -> Recipient {
    let mut events = rig.watch();
    let pane = rig
        .test
        .rig
        .launch_job_controlled(&lifecycle::config(StandInMode::Claude));
    let row = rig.test.complete_fixture_turn(&pane);
    let session = row.id.to_string();
    events.wait("the fixture conversation's committed summary", |e| {
        e["kind"] == "session.summary"
            && e["body"]["session_id"] == session
            && e["body"]["first_prompt"] == serde_json::json!(row.first_prompt)
    });
    let adopted = rig.test.adopt(name, &pane);
    if adopted["state"] != "ready" {
        let root = lifecycle::checked(rig.test.rig.tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane.pane_id,
            "#{pane_pid} #{pane_current_command}",
        ]));
        let pid = root.split_whitespace().next().unwrap();
        eprintln!(
            "adopt process: {root}; stat={}; children={}",
            std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap(),
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap()
        );
    }
    assert_eq!(
        adopted["state"],
        "ready",
        "{adopted}\n{}\n{}",
        rig.daemon_log(),
        rig.test.rig.capture(&pane)
    );
    let id = adopted["agent_id"].as_str().unwrap().to_owned();
    assert_eq!(rig.test.agent(&id)["pane"], pane.pane_id);
    Recipient {
        pane,
        id,
        session: Some(session),
    }
}
fn unbound_pane(rig: &MessageRig) -> String {
    // A real pane, but no harness/session: launching another stand-in would
    // legitimately enqueue its initial session.summary during the refusal.
    lifecycle::checked(rig.test.rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]))
}
fn ask(rig: &MessageRig, who: &Recipient, intent: bool) -> String {
    rig.call("decision.request", json!({"kind":"plan","title":"Spoken choice","options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":intent}), Some(&who.pane)).unwrap()["decision_id"].as_str().unwrap().into()
}
fn resolve(id: &str, option: &str) -> Value {
    json!({"id":id,"option":option,"relay":true})
}
fn raw(
    rig: &MessageRig,
    method: &str,
    params: Value,
    pane: Option<&str>,
) -> Result<Value, (String, String)> {
    call_on(
        &rig.test.rig.socket(),
        method,
        params,
        pane.map(str::to_owned),
    )
}
// Hook accounting is independent of RPC completion: even a completed fixture
// turn can enqueue usage after its subscribed Idle row. It is not a mutation
// by this request; permit only that telemetry, never a decision/owner/message fact.
fn assert_no_mutation(rig: &MessageRig, before: u64) {
    let suffix = rig.test.tail(before);
    assert!(
        suffix.iter().all(|e| e["kind"] == "task.usage_observed"),
        "unexpected append: {suffix:?}"
    );
}
fn refused(rig: &MessageRig, method: &str, params: Value, pane: Option<&str>, code: &str) {
    let before = rig.test.cursor();
    let error = raw(rig, method, params.clone(), pane).unwrap_err();
    assert_eq!(error.0, code, "{method} {params} {pane:?}: {error:?}");
    assert_no_mutation(rig, before);
    assert!(!rig.test.project.join(".atm/rules.md").exists());
}
fn set(rig: &MessageRig, who: &Recipient) {
    assert_eq!(
        rig.call("owner.relay", json!({"agent":who.id}), None)
            .unwrap(),
        json!({"agent":who.id,"changed":true})
    );
}

// Break caught: assuming attribution is authenticated, or treating malformed
// panes like unbound panes. This preflight runs before production edits.
#[test]
fn relay_caller_attribution_preflight() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let builder = seat(&mut rig, "builder");
    let unbound = unbound_pane(&rig);
    let id = ask(&rig, &builder, true);
    eprintln!("tmux version: {:?}", rig.test.rig.tmux(&["-V"]));
    eprintln!(
        "binding: {} -> {}; unbound {}",
        builder.pane.pane_id, builder.id, unbound
    );
    refused(
        &rig,
        "decision.resolve",
        json!({"id":id,"option":"yes"}),
        Some(&builder.pane.pane_id),
        "refused",
    );
    refused(
        &rig,
        "decision.resolve",
        json!({"id":id,"option":"yes"}),
        Some("%00"),
        "invalid_params",
    );
    let result = raw(
        &rig,
        "decision.resolve",
        json!({"id":id,"option":"no"}),
        Some(&unbound),
    )
    .unwrap();
    assert_eq!(result["decision"]["status"], "resolved");
    assert_eq!(result["decision"]["resolution"]["by"], "owner");
    eprintln!("actual unbound caller result: {result}");
    rig.test.rig.close();
}

// Break caught: job-control opt-in not creating a foreground job, or a failed
// test leaving that job alive after its private tmux server was torn down.
#[test]
fn relay_job_controlled_cleanup_reaps_children_on_unwind() {
    for fail in [false, true] {
        let mut child = None;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut rig = stand_in::StandInRig::start();
            let pane = rig.launch_job_controlled(&lifecycle::config(StandInMode::Claude));
            let group = unsafe { libc::getpgid(pane.pid as i32) };
            child = Some((pane.pid, group));
            assert_eq!(
                group, pane.pid as i32,
                "stand-in must own its foreground job"
            );
            if fail {
                panic!("deliberate relay fixture failure");
            }
            rig.close();
        }));
        assert_eq!(outcome.is_err(), fail, "unexpected fixture failure");
        let (pid, group) = child.unwrap();
        stand_in::process::wait_exit(pid, std::time::Duration::from_secs(60)).unwrap();
        let live: Vec<_> = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                // Match atmd's proc_stat_has_exited: procfs can retain an exited
                // job until reaped, but a zombie leader may still have live threads.
                let exited = match *fields.first()? {
                    "X" | "x" => true,
                    "Z" => fields.get(17) == Some(&"1"),
                    _ => false,
                };
                (fields.get(2)?.parse::<i32>().ok()? == group && !exited)
                    .then_some(entry.file_name())
            })
            .collect();
        assert!(
            live.is_empty(),
            "foreground job survived teardown: {live:?}"
        );
        eprintln!("cleanup failure={fail}: child {pid}, own pgid {group}, exit observed, no live group members");
    }
}

// Break caught: a caller-selected slot, absent registration, ambiguous clear,
// or a read/retry that writes another event.
#[test]
fn owner_relay_set_show_clear_is_owner_only_and_durable() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let builder = seat(&mut rig, "builder");
    assert_eq!(
        rig.call("owner.relay", json!({}), None).unwrap(),
        json!({"agent":null,"changed":false})
    );
    for params in [json!({}), json!({"agent":chief.id}), json!({"clear":true})] {
        refused(
            &rig,
            "owner.relay",
            params,
            Some(&chief.pane.pane_id),
            "refused",
        );
    }
    for params in [
        json!({"agent":"chief"}),
        json!({"agent":"project/missing"}),
        json!({"agent":chief.id,"clear":true}),
        json!({"by":"owner"}),
    ] {
        let code = if params["agent"] == "project/missing" {
            "not_found"
        } else {
            "invalid_params"
        };
        refused(&rig, "owner.relay", params, None, code);
    }
    set(&rig, &chief);
    let before = rig.test.cursor();
    for params in [json!({}), Value::Null, json!({"agent":chief.id})] {
        assert_eq!(
            rig.call("owner.relay", params, None).unwrap(),
            json!({"agent":chief.id,"changed":false})
        );
    }
    assert_no_mutation(&rig, before);
    set(&rig, &builder);
    assert_eq!(
        rig.call("owner.relay", json!({"clear":true}), None)
            .unwrap(),
        json!({"agent":null,"changed":true})
    );
    let before = rig.test.cursor();
    assert_eq!(
        rig.call("owner.relay", json!({"clear":true}), None)
            .unwrap(),
        json!({"agent":null,"changed":false})
    );
    assert_no_mutation(&rig, before);
    set(&rig, &chief);
    rig.test.rig.restart_daemon();
    assert_eq!(
        rig.call("owner.relay", json!({}), None).unwrap(),
        json!({"agent":chief.id,"changed":false})
    );
    rig.test.rig.close();
}

// Break caught: relay falling through to final owner Resolve, trusting an
// address in params, or recording a second take for an identical retry.
#[test]
fn relay_rpc_records_derived_agent_and_repeats_once() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let builder = seat(&mut rig, "builder");
    set(&rig, &chief);
    let id = ask(&rig, &builder, true);
    let mut watch = rig.watch();
    let result = rig
        .call("decision.resolve", resolve(&id, "no"), Some(&chief.pane))
        .unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(result["decision"]["status"], "taken_provisionally");
    assert_eq!(
        result["decision"]["provisional"],
        json!({"decision_id":id,"option":"no","by":"owner","relayed_by":chief.id})
    );
    let take = watch.wait("the relayed take", |e| {
        e["kind"] == "decision.taken_provisionally"
    });
    assert_eq!(take["body"], result["decision"]["provisional"]);
    let notice = watch.wait("the take's notice", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == builder.id
    });
    let message = format!("m{}", notice["seq"]);
    watch.wait(
        "the delivered reference",
        support::about("msg.delivered", &message),
    );
    rig.finish_turn(&builder.pane);
    let before = rig.test.cursor();
    let again = rig
        .call("decision.resolve", resolve(&id, "no"), Some(&chief.pane))
        .unwrap();
    assert_eq!(again["changed"], false);
    assert_eq!(again["decision"], result["decision"]);
    assert_no_mutation(&rig, before);
    refused(
        &rig,
        "decision.resolve",
        resolve(&id, "yes"),
        Some(&chief.pane.pane_id),
        "refused",
    );
    watch.catch_up(&rig);
    assert_eq!(
        watch
            .all(|e| e["kind"] == "decision.taken_provisionally")
            .len(),
        1
    );
    assert_eq!(
        watch
            .all(|e| e["kind"] == "msg.sent" && e["body"]["to"] == builder.id)
            .len(),
        1
    );
    set(&rig, &builder);
    refused(
        &rig,
        "decision.resolve",
        resolve(&id, "no"),
        Some(&chief.pane.pane_id),
        "refused",
    );
    rig.call("owner.relay", json!({"clear":true}), None)
        .unwrap();
    refused(
        &rig,
        "decision.resolve",
        resolve(&id, "no"),
        Some(&builder.pane.pane_id),
        "refused",
    );
    rig.test.rig.close();
}

// Break caught: any section-1 refusal recording an owner answer or a rule.
#[test]
fn relay_callers_fail_closed_without_appends() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let builder = seat(&mut rig, "builder");
    let unbound = unbound_pane(&rig);
    let id = ask(&rig, &builder, true);
    refused(
        &rig,
        "decision.resolve",
        resolve(&id, "no"),
        Some(&chief.pane.pane_id),
        "refused",
    );
    set(&rig, &chief);
    for caller in [
        None,
        Some(unbound.as_str()),
        Some(builder.pane.pane_id.as_str()),
    ] {
        refused(
            &rig,
            "decision.resolve",
            resolve(&id, "no"),
            caller,
            "refused",
        );
    }
    for caller in ["%00", "%", "session:1.2", " %7", "%７"] {
        refused(
            &rig,
            "decision.resolve",
            resolve(&id, "no"),
            Some(caller),
            "invalid_params",
        );
    }
    for (key, value) in [
        ("note", Value::Null),
        ("note", json!("")),
        ("note", json!("spoken")),
        ("standing_rule", json!(false)),
        ("standing_rule", json!(true)),
        ("by", json!("owner")),
        ("relayed_by", json!(chief.id)),
    ] {
        let mut params = resolve(&id, "no");
        params[key] = value;
        refused(
            &rig,
            "decision.resolve",
            params,
            Some(&chief.pane.pane_id),
            "invalid_params",
        );
    }
    rig.call("agent.down", json!({"agent":chief.id}), None)
        .unwrap();
    assert_eq!(rig.test.agent(&chief.id)["state"], "down");
    refused(
        &rig,
        "decision.resolve",
        resolve(&id, "no"),
        Some(&chief.pane.pane_id),
        "refused",
    );
    let decision = rig.call("decision.get", json!({"id":id}), None).unwrap();
    assert_eq!(decision["decision"]["status"], "pending");
    assert_eq!(decision["decision"]["provisional"], Value::Null);
    rig.test.rig.close();
}

// Break caught: CLI forgetting the designation action/format or permitting a
// client identity override. All commands use the built CLI and private socket.
#[test]
fn relay_cli_sets_shows_clears_and_preserves_json() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let output = rig.atm(None, &["relay"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"Relay seat: none\n");
    assert_eq!(
        rig.atm_json(None, &["relay", &chief.id, "--format", "json"]),
        json!({"agent":chief.id,"changed":true})
    );
    assert_eq!(
        rig.atm(None, &["relay"]).stdout,
        format!("Relay seat: {}\n", chief.id).as_bytes()
    );
    assert_eq!(
        rig.atm_json(None, &["relay", &chief.id, "--format", "json"]),
        json!({"agent":chief.id,"changed":false})
    );
    for args in [
        vec!["relay", "chief"],
        vec!["relay", &chief.id, "--clear"],
        vec!["relay", "--format", "xml"],
        vec!["relay", "--by", "owner"],
    ] {
        assert_eq!(rig.atm(None, &args).status.code(), Some(2), "{args:?}");
    }
    let mut command = rig
        .test
        .rig
        .command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
    command
        .args(["relay", "--clear"])
        .env("TMUX_PANE", &chief.pane.pane_id)
        .env("ATM_AGENT", "owner");
    let refused = rig.test.rig.run(&mut command);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(refused.stdout.is_empty());
    assert_eq!(
        rig.atm_json(None, &["relay", "--clear", "--format", "json"]),
        json!({"agent":null,"changed":true})
    );
    assert_eq!(
        rig.atm_json(None, &["relay", "--clear", "--format", "json"]),
        json!({"agent":null,"changed":false})
    );
    rig.test.rig.close();
}

// Break caught: --relay being dropped, returning a final success, or allowing
// free text/rules under the owner's name. A nonrecommended listed option works.
#[test]
fn decide_relay_is_provisional_and_rejects_note_or_rule() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let builder = seat(&mut rig, "builder");
    set(&rig, &chief);
    let id = ask(&rig, &builder, true);
    for extra in [vec!["note"], vec!["--rule"], vec!["note", "--rule"]] {
        let mut args = vec!["decide", &id, "no", "--relay"];
        args.extend(extra);
        assert_eq!(rig.atm(Some(&chief.pane), &args).status.code(), Some(2));
    }
    let out = rig.atm(Some(&chief.pane), &["decide", &id, "no", "--relay"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out.stdout,
        format!("{id} taken_provisionally: no\nAwaiting the owner's confirmation.\n").as_bytes()
    );
    let again = rig.atm_json(
        Some(&chief.pane),
        &["decide", &id, "no", "--relay", "--format", "json"],
    );
    assert_eq!(again["changed"], false);
    assert_eq!(again["decision"]["status"], "taken_provisionally");
    assert_eq!(again["decision"]["provisional"]["relayed_by"], chief.id);
    assert!(!rig
        .atm(Some(&chief.pane), &["decide", &id, "no"])
        .status
        .success());
    assert!(!rig
        .atm(Some(&builder.pane), &["decide", &id, "no", "--relay"])
        .status
        .success());
    assert!(!rig
        .atm(Some(&chief.pane), &["decide", &id, "yes", "--relay"])
        .status
        .success());
    assert_eq!(
        rig.atm_json(None, &["decide", &id, "no", "--format", "json"])["decision"]["status"],
        "resolved"
    );
    assert!(!rig.test.project.join(".atm/rules.md").exists());
    rig.test.rig.close();
}

// Break caught: a self-relay becoming agent-decided, retry becoming confirmation,
// or retry of an away-table take falsely acquiring relay provenance.
#[test]
fn relay_retry_never_becomes_owner_confirmation() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    set(&rig, &chief);
    let id = ask(&rig, &chief, false);
    let args = ["decide", &id, "no", "--relay", "--format", "json"];
    let first = rig.atm_json(Some(&chief.pane), &args);
    let again = rig.atm_json(Some(&chief.pane), &args);
    assert_eq!(
        first["decision"]["provisional"],
        json!({"decision_id":id,"option":"no","by":"owner","relayed_by":chief.id})
    );
    assert_eq!(again["decision"], first["decision"]);
    assert_eq!(again["changed"], false);
    assert_eq!(
        rig.atm_json(None, &["decide", &id, "yes", "--format", "json"])["decision"]["status"],
        "overruled"
    );
    rig.atm_json(None, &["away", "--format", "json"]);
    let mut watch = rig.watch();
    let old = ask(&rig, &chief, false);
    watch.wait("the away-table take", |e| {
        e["kind"] == "decision.taken_provisionally" && e["body"]["decision_id"] == old
    });
    let unchanged = rig.atm_json(
        Some(&chief.pane),
        &["decide", &old, "yes", "--relay", "--format", "json"],
    );
    assert_eq!(unchanged["changed"], false);
    assert_eq!(
        unchanged["decision"]["provisional"],
        json!({"decision_id":old,"option":"yes","by":chief.id})
    );
    rig.test.rig.close();
}

fn told_relay(
    rig: &MessageRig,
    watch: &mut support::Watch,
    who: &Recipient,
    cursor: u64,
    body: &str,
) {
    let sent = watch.wait("the relay notice", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == who.id
    });
    assert_eq!(
        sent["body"],
        json!({"from":"system","to":who.id,"kind":"answer","body":body,"attachments":[],"task":null})
    );
    let id = format!("m{}", sent["seq"]);
    watch.wait(
        "its confirmed delivery",
        support::about("msg.delivered", &id),
    );
    let reference = support::reference(&id);
    rig.test.rig.wait_event(&who.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference
    });
    rig.finish_turn(&who.pane);
    eprintln!("delivered {id}: {body}");
}

// Break caught: incomplete restart reconciliation, duplicate notice after retry,
// or the relay mark disappearing before the owner answers. Real built binaries.
#[test]
fn relay_notice_is_enqueued_once_across_retry_and_restart() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let chief = seat(&mut rig, "chief");
    let builder = seat(&mut rig, "builder");
    rig.atm_json(None, &["relay", &chief.id, "--format", "json"]);
    let id = ask(&rig, &builder, true);
    let cursor = rig.test.rig.event_cursor(&builder.pane);
    let mut watch = rig.watch();
    let args = ["decide", &id, "no", "--relay", "--format", "json"];
    let result = rig.atm_json(Some(&chief.pane), &args);
    eprintln!("intent relay: {result}");
    let body = format!("Decision {id} taken provisionally: no\nRelayed by {} as the owner's spoken answer; he has not confirmed it himself. Finish your task in progress on this option. Start nothing further on it: a task built on it names {id} in rests_on and waits until the owner has confirmed {id}.", chief.id);
    told_relay(&rig, &mut watch, &builder, cursor, &body);
    assert_eq!(rig.atm_json(Some(&chief.pane), &args)["changed"], false);
    rig.hold_turn(&builder.pane);
    rig.hold_turn(&chief.pane);
    rig.test.rig.restart_daemon();
    rig.finish_turn(&builder.pane);
    rig.finish_turn(&chief.pane);
    assert_eq!(rig.test.agent(&chief.id)["pane"], chief.pane.pane_id);
    // resolve's initialized() is the completed reconcile fence, not readiness
    // of the socket/listener or an elapsed delay.
    assert_eq!(rig.atm_json(Some(&chief.pane), &args)["changed"], false);
    assert_eq!(
        rig.atm_json(None, &["relay", "--format", "json"])["agent"],
        chief.id
    );
    let events = rig.test.tail(0);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "msg.sent" && e["body"]["body"] == body)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |e| e["kind"] == "decision.taken_provisionally" && e["body"]["decision_id"] == id
            )
            .count(),
        1
    );
    let confirmed = rig.atm_json(None, &["decide", &id, "no", "--format", "json"]);
    assert_eq!(confirmed["decision"]["status"], "resolved");
    assert_eq!(
        confirmed["decision"]["provisional"],
        result["decision"]["provisional"]
    );
    eprintln!("after restart: one take, one exact notice; confirmed with provenance retained");

    let detail = ask(&rig, &chief, false);
    let cursor = rig.test.rig.event_cursor(&chief.pane);
    let mut watch = rig.watch();
    rig.atm_json(
        Some(&chief.pane),
        &["decide", &detail, "no", "--relay", "--format", "json"],
    );
    told_relay(&rig, &mut watch, &chief, cursor, &format!("Decision {detail} taken provisionally: no\nRelayed by {} as the owner's spoken answer; he has not confirmed it himself. Carry on; he may still overrule it.", chief.id));
    rig.test.rig.close();
}
