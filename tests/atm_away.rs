//! `atm away` and `atm back`: arguments, exact output and the one private RPC
//! each makes. A fake peer answers; no daemon, harness or fallback runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

fn command(dir: &Path, args: &[&str]) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "bin", "state", "tmux", "config"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    // Anything the CLI started instead of asking the peer would leave a mark.
    for name in ["atmd", "tmux"] {
        let path = dir.join("bin").join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf fallback >> \"$ATM_AWAY_FALLBACK\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.env_clear()
        .args(args)
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("SHELL", "/bin/sh")
        .env("ATM_SOCKET", dir.join("rpc.sock"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir)
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("ATM_TMUX_SOCKET", "away-cli")
        .env("TMUX_PANE", "%7")
        .env("ATM_AWAY_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Run the CLI against a peer that advertises `method` (or nothing), expects
/// exactly one request with empty params from pane `%7`, and answers `payload`.
async fn exchange(args: &[&str], method: &str, payload: Value, advertised: bool) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
    let method = method.to_owned();
    let peer = tokio::spawn(async move {
        let (mut read, mut write) = loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                continue;
            }
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["type"],
                "connect"
            );
            observed.store(true, Ordering::SeqCst);
            break (read, write);
        };
        let methods = if advertised {
            json!([method])
        } else {
            json!([])
        };
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"away-cli","methods":methods})).as_bytes()).await.unwrap();
        let mut line = String::new();
        if advertised {
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":method,"params":{},"from_pane":"%7"})
            );
            let mut response = json!({"type":"response","id":"r1"});
            response
                .as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(read.read_line(&mut line).await.unwrap(), 0, "one RPC only");
    });
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        command(dir.path(), args)
            .spawn()
            .unwrap()
            .wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    if !seen.load(Ordering::SeqCst) {
        peer.abort();
        panic!(
            "CLI never connected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.path().join("fallback").exists());
    output
}

async fn text(args: &[&str], method: &str, result: Value) -> String {
    let out = exchange(args, method, json!({ "result": result }), true).await;
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

const RULES: &str = "Details: the agent that asked decides. Changes to the intent that can be undone: taken provisionally on the recommended option. Anything that cannot be undone or reaches outside, and any action, hire or budget: waits for you.\n";

#[tokio::test]
async fn away_prints_the_time_the_rules_the_caps_and_what_is_pending() {
    let away = |caps: Value, pending: u64| json!({"changed":true,"away_since":"2026-11-16T08:02:11.500Z","stacking_caps":caps,"pending_count":pending});
    assert_eq!(
        text(&["away"], "owner.away", away(json!({"alpha":1}), 3)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}Stacking cap: alpha 1\n3 pending decisions are now handled by these rules: atm decisions\n")
    );
    assert_eq!(
        text(&["away"], "owner.away", away(json!({"alpha":1,"beta":2,"gamma":3}), 1)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}Stacking cap: alpha 1, beta 2, gamma 3\n1 pending decision is now handled by these rules: atm decisions\n")
    );
    // No project and nothing pending: neither line is printed.
    assert_eq!(
        text(&["away"], "owner.away", away(json!({}), 0)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}")
    );
    // A repeat says so, and nothing else.
    let mut repeat = away(json!({"alpha":1}), 2);
    repeat["changed"] = json!(false);
    assert_eq!(
        text(&["away"], "owner.away", repeat).await,
        "Already away since 2026-11-16T08:02:11Z.\n"
    );
}

#[tokio::test]
async fn back_from_an_old_daemon_keeps_counts_and_explains_missing_digest() {
    let back = |since: Value, provisional: u64, pending: u64| json!({"changed":true,"away_since":since,"provisional_count":provisional,"pending_count":pending});
    assert_eq!(
        text(&["back"], "owner.back", back(json!("2026-11-16T08:02:11Z"), 4, 2)).await,
        "Back. You were away since 2026-11-16T08:02:11Z.\n4 decisions taken provisionally wait for your answer; 2 are pending: atm decisions\nDigest unavailable from this daemon; run atm decisions.\n"
    );
    assert_eq!(
        text(&["back"], "owner.back", back(json!("2026-11-16T08:02:11Z"), 1, 1)).await,
        "Back. You were away since 2026-11-16T08:02:11Z.\n1 decision taken provisionally waits for your answer; 1 is pending: atm decisions\nDigest unavailable from this daemon; run atm decisions.\n"
    );
    // The switch was set by another call in between: the time is not known.
    assert_eq!(
        text(&["back"], "owner.back", back(Value::Null, 0, 0)).await,
        "Back.\n0 decisions taken provisionally wait for your answer; 0 are pending: atm decisions\nDigest unavailable from this daemon; run atm decisions.\n"
    );
    let not_away =
        json!({"changed":false,"away_since":null,"provisional_count":4,"pending_count":2});
    assert_eq!(
        text(&["back"], "owner.back", not_away).await,
        "Not away.\nDigest unavailable from this daemon; run atm decisions.\n"
    );
}

#[tokio::test]
async fn json_format_passes_the_daemons_result_through_with_later_fields() {
    for (args, method, result) in [
        (
            ["away", "--format", "json"],
            "owner.away",
            json!({"changed":true,"away_since":"2026-11-16T08:02:11Z","stacking_caps":{"alpha":1},"pending_count":3,"later":{"field":true}}),
        ),
        (
            ["back", "--format", "json"],
            "owner.back",
            json!({"changed":true,"away_since":"2026-11-16T08:02:11Z","provisional_count":4,"pending_count":2,"future_digest":["later"]}),
        ),
    ] {
        let out = text(&args, method, result.clone()).await;
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            result,
            "{method}"
        );
        assert!(out.ends_with('\n') && out.lines().count() == 1, "{out:?}");
    }
}

#[tokio::test]
async fn refusals_an_old_daemon_and_a_malformed_result_fail_without_output() {
    for (args, method) in [(["away"], "owner.away"), (["back"], "owner.back")] {
        for (payload, advertised, diagnostic) in [
            (
                json!({"error":{"code":"refused","message":"only the owner may set away or back"}}),
                true,
                "only the owner may set away or back",
            ),
            (
                json!({"error":{"code":"refused","message":"/project/.atm/away.toml: stacking_cap must be at least 1"}}),
                true,
                "/project/.atm/away.toml: stacking_cap must be at least 1",
            ),
            (
                json!({"result":{"changed":true}}),
                true,
                "invalid away result",
            ),
            (json!({}), false, "daemon is too old"),
        ] {
            let out = exchange(&args, method, payload, advertised).await;
            assert!(!out.status.success(), "{method}: {out:?}");
            assert!(out.stdout.is_empty(), "{method}: {out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(diagnostic),
                "{method}: {out:?}"
            );
        }
    }
}

// Synthetic reports from the approved examples; neutral project identities.
fn base_page() -> Value {
    serde_json::from_str(include_str!(
        "../crates/atm-org/tests/fixtures/return_digest_expected.json"
    ))
    .unwrap()
}
fn report(page: Value) -> Value {
    let rows = page["rows"].as_array().unwrap();
    let provisional = rows
        .iter()
        .filter(|r| r["kind"] == "decision" && r["decision"]["status"] == "taken_provisionally")
        .count();
    let pending = page["counts"]["waiting_decisions"].as_u64().unwrap();
    json!({"changed":true,"away_since":page["absence"]["since"],"provisional_count":provisional,"pending_count":pending,"digest":page})
}
fn small_page() -> Value {
    let mut page = base_page();
    page["counts"]["waiting_decisions"] = json!(1);
    page["counts"]["waiting_total"] = json!(1);
    page["rows"][0]["impact"]["not_started"] = json!(1);
    let mut pending = page["rows"][0]["decision"].clone();
    pending["id"] = json!("d20");
    pending["status"] = json!("pending");
    pending["provisional"] = Value::Null;
    pending["taken_at"] = Value::Null;
    pending["request"]["title"] = json!("Publish build");
    pending["request"]["may_be_undone"] = json!(false);
    page["rows"].as_array_mut().unwrap().extend([json!({"kind":"task","decision_id":"d11","task_id":"t18","title":"Integrate cache","state":"open","started":false,"direct":false}),json!({"kind":"waiting","decision":pending})]);
    page["total_rows"] = json!(4);
    page
}
fn decision_row(id: &str, title: &str, option: &str, by: &str, impact: Value) -> Value {
    let mut row = base_page()["rows"][0].clone();
    let decision = &mut row["decision"];
    decision["id"] = json!(id);
    decision["request"]["title"] = json!(title);
    decision["request"]["options"] = json!([option, "other"]);
    decision["request"]["recommended"] = json!(option);
    decision["request"]["changes_intent"] = json!(by == "system");
    decision["request"]["requester"] = json!(if by == "system" { "alpha/lead" } else { by });
    decision["provisional"] = json!({"decision_id":id,"option":option,"by":by});
    row["impact"] = impact;
    row
}
fn task_row(d: &str, id: &str, title: &str, state: &str, started: bool, direct: bool) -> Value {
    json!({"kind":"task","decision_id":d,"task_id":id,"title":title,"state":state,"started":started,"direct":direct})
}
fn large_page() -> Value {
    let mut page = base_page();
    page["snapshot_seq"] = json!(101);
    page["absence"]["away_seq"] = json!(10);
    page["absence"]["back_seq"] = json!(100);
    page["counts"] = json!({"during_absence":3,"earlier_provisional":1,"waiting_decisions":2,"waiting_tasks":3,"waiting_total":5,"taken_for_owner":5,"overruled":1});
    let a = decision_row(
        "d11",
        "Choose storage",
        "local",
        "system",
        json!({"started":3,"accepted":2,"not_started":1,"cancelled":1,"unknown":0}),
    );
    let b = decision_row(
        "d21",
        "Choose cache",
        "small",
        "beta/lead",
        json!({"started":1,"accepted":0,"not_started":0,"cancelled":0,"unknown":0}),
    );
    let mut c = decision_row(
        "d31",
        "Choose report",
        "text",
        "system",
        json!({"started":0,"accepted":0,"not_started":2,"cancelled":0,"unknown":0}),
    );
    c["decision"]["request"]["requester"] = json!("gamma/chief");
    let mut earlier = decision_row(
        "d3",
        "Choose format",
        "json",
        "system",
        json!({"started":1,"accepted":1,"not_started":0,"cancelled":0,"unknown":0}),
    );
    earlier["section"] = json!("earlier");
    let mut waiting = small_page()["rows"][3]["decision"].clone();
    waiting["id"] = json!("d40");
    let mut scope = waiting.clone();
    scope["id"] = json!("d50");
    scope["request"]["title"] = json!("Approve scope");
    scope["request"]["may_be_undone"] = json!(true);
    scope["request"]["changes_intent"] = json!(true);
    scope["request"]["recommended"] = Value::Null;
    page["rows"] = json!([a,task_row("d11","t14","Store records","accepted",true,true),task_row("d11","t16","Read records","accepted",true,false),task_row("d11","t18","Review storage","working",true,false),task_row("d11","t19","Add export","open",false,false),b,task_row("d21","t24","Cache records","changes_requested",true,true),c,task_row("d31","t34","Print report","open",false,true),task_row("d31","t36","Review report","open",false,false),earlier,task_row("d3","t4","Encode records","accepted",true,true),{"kind":"waiting","decision":waiting},{"kind":"waiting","decision":scope},{"kind":"waiting_task","task_id":"t19","title":"Add export","state":"open","reason":null,"held_on":"d11"},{"kind":"waiting_task","task_id":"t34","title":"Print report","state":"open","reason":null,"held_on":"d31"},{"kind":"waiting_task","task_id":"t36","title":"Review report","state":"open","reason":null,"held_on":"d31"},{"kind":"overrule_number","decision_id":"d5","completed":1,"required":2,"cancelled":0,"caller_pane":"%9"}]);
    page["total_rows"] = json!(18);
    page
}

/// The peer exits on the child's exit event, not on a timeout or readiness poll.
/// Every expected request includes its exact method/params and pane identity.
async fn pages(args: &[&str], script: Vec<(String, Value, Value)>, broken_stdout: bool) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let (done, mut exit) = tokio::sync::watch::channel(false);
    let expected = script.len();
    let peer = tokio::spawn(async move {
        let mut script = script.into_iter();
        let mut received = 0;
        loop {
            let stream = tokio::select! { biased; _=exit.changed()=>break, accepted=listener.accept()=>accepted.unwrap().0 };
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                continue;
            }
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["type"],
                "connect"
            );
            write.write_all(format!("{}\n",json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"paged-cli","methods":["owner.back","owner.digest","decision.resolve","decision.get"]})).as_bytes()).await.unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            let (method, params, payload) = script.next().expect("unexpected extra RPC");
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":method,"params":params,"from_pane":"%7"})
            );
            received += 1;
            let mut frame = json!({"type":"response","id":"r1"});
            frame
                .as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            assert_eq!(read.read_line(&mut line).await.unwrap(), 0);
        }
        received
    });
    let mut cmd = command(dir.path(), args);
    if broken_stdout {
        cmd.stdout(Stdio::from(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        ));
    }
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.spawn().unwrap().wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    done.send(true).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, expected, "CLI did not drain the report: {out:?}");
    assert!(!dir.path().join("fallback").exists());
    out
}
fn page_script(page: &Value, limit: usize) -> Vec<(String, Value, Value)> {
    let rows = page["rows"].as_array().unwrap();
    let mut result = vec![];
    for (part, chunk) in rows.chunks(limit).enumerate() {
        let start = part * limit;
        let end = start + chunk.len();
        let mut piece = page.clone();
        piece["rows"] = json!(chunk);
        piece["next_after"] = if end < rows.len() {
            json!(end - 1)
        } else {
            Value::Null
        };
        if part == 0 {
            let mut first = report(page.clone());
            first["digest"] = piece;
            result.push(("owner.back".into(), json!({}), json!({"result":first})));
        } else {
            result.push((
                "owner.digest".into(),
                json!({"snapshot_seq":page["snapshot_seq"],"after":start-1,"limit":100}),
                json!({"result":piece}),
            ));
        }
    }
    result
}

// Break caught: placeholder/ordering/wording survives instead of the complete supplied report.
#[tokio::test]
async fn back_small_large_zero_repeat_and_final_exact_text() {
    assert_eq!(
        text(&["back"], "owner.back", report(small_page())).await,
        include_str!("fixtures/atm_back/small.txt")
    );
    for limit in [2, 100] {
        let out = pages(&["back"], page_script(&large_page(), limit), false).await;
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            include_str!("fixtures/atm_back/large.txt")
        );
    }
    let mut page = base_page();
    page["rows"] = json!([]);
    page["total_rows"] = json!(0);
    page["counts"] = json!({"during_absence":0,"earlier_provisional":0,"waiting_decisions":0,"waiting_tasks":0,"waiting_total":0,"taken_for_owner":0,"overruled":0});
    let expected="Back. You were away since 2026-11-16T08:00:00Z.\nRelay seat: none\nNo decisions were taken for you while away.\nRelayed spoken answers: none.\nEarlier provisional decisions: none.\nStill waiting: none.\nNumbers (all recorded time): 0 decisions taken for you; 0 overruled.\nTasks redone per overrule: none.\nRecorded links only: unlinked work and an unlinked task in progress are not included.\n";
    assert_eq!(
        text(&["back"], "owner.back", report(page.clone())).await,
        expected
    );
    let mut repeat = report(page.clone());
    repeat["changed"] = json!(false);
    repeat["away_since"] = Value::Null;
    let output = text(&["back"], "owner.back", repeat).await;
    assert!(output.starts_with(
        "Not away. Showing the last absence: 2026-11-16T08:00:00Z to 2026-11-16T18:00:00Z.\n"
    ));
    page["absence"] = Value::Null;
    let mut first = report(page);
    first["changed"] = json!(false);
    first["away_since"] = Value::Null;
    let output = text(&["back"], "owner.back", first).await;
    assert!(output
        .starts_with("Not away. No previous absence.\nRelay seat: none\nNo absence to report.\n"));
    let mut final_page = small_page();
    final_page["rows"][0]["decision"]["status"] = json!("resolved");
    final_page["rows"][0]["decision"]["resolution"] = json!({"decision_id":"d11","option":"small","by":"owner","note":null,"standing_rule":false});
    final_page["rows"][0]["answer_caller_pane"] = json!("%9");
    let output = text(&["back"], "owner.back", report(final_page)).await;
    assert!(output.contains("Work using this decision is listed below."));
    assert!(output.contains("  used: t14"));
    assert!(output.contains("  Answer caller: %9"));
    assert!(!output.contains("  Answer: atm decide d11"));
}

// Break caught: an uninterpretable task is presented as promised redo work.
#[tokio::test]
async fn unknown_dependencies_and_legacy_or_cancelled_numbers_are_explicit() {
    let mut page = small_page();
    page["rows"][0]["impact"]["accepted"] = json!(0);
    page["rows"][0]["impact"]["unknown"] = json!(1);
    page["rows"][1]["state"] = json!("unknown");
    let output = text(&["back"], "owner.back", report(page)).await;
    assert!(output.contains("Overruling is refused: a recorded task has an unknown state."));
    assert!(output.contains("  unknown: t14 [unknown; direct] Write cache\n"));
    assert!(!output.contains("  redo: t14"));
    for (completed, required, cancelled, caller, expected) in [
        (
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            "  d5: not recorded; caller=not recorded\n",
        ),
        (
            json!(1),
            json!(2),
            json!(1),
            json!("%9"),
            "  d5: 1/2 accepted again (1 cancelled); caller=%9\n",
        ),
    ] {
        let mut page = large_page();
        let number = page["rows"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row["kind"] == "overrule_number")
            .unwrap();
        number["completed"] = completed;
        number["required"] = required;
        number["cancelled"] = cancelled;
        number["caller_pane"] = caller;
        assert!(text(&["back"], "owner.back", report(page))
            .await
            .contains(expected));
    }
}

// Break caught: first-page-only JSON, lost future fields or a repeated owner.back mutation.
#[tokio::test]
async fn back_json_drains_pages_and_preserves_future_fields() {
    let mut page = large_page();
    page["future_digest"] = json!({"kept":true});
    page["rows"][1]["future_task"] = json!(7);
    let out = pages(&["back", "--format", "json"], page_script(&page, 2), false).await;
    assert!(out.status.success(), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result, report(page));
    assert_eq!(String::from_utf8(out.stdout).unwrap().lines().count(), 1);
}

// Break caught: reports beyond both page and frame totals are cut short.
#[tokio::test]
async fn back_json_drains_more_than_a_hundred_rows_and_a_frame_of_utf8() {
    let mut page = base_page();
    page["snapshot_seq"] = json!(1000);
    page["absence"]["back_seq"] = json!(999);
    page["rows"][0]["impact"] =
        json!({"started":0,"accepted":0,"not_started":120,"cancelled":0,"unknown":0});
    for index in 100..220 {
        page["rows"].as_array_mut().unwrap().push(task_row(
            "d11",
            &format!("t{index}"),
            &"☃".repeat(2048),
            "open",
            false,
            true,
        ));
    }
    page["rows"].as_array_mut().unwrap().remove(1);
    page["total_rows"] = json!(121);
    assert!(serde_json::to_vec(&page).unwrap().len() > 524288);
    let out = pages(&["back", "--format", "json"], page_script(&page, 50), false).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        report(page)
    );
}

// Shared paged peer exercises decide too, without a second socket fixture.
#[tokio::test]
async fn decide_drains_immutable_redo_ids_and_keeps_last_progress_and_future_fields() {
    let list: Value = serde_json::from_str(include_str!("fixtures/atm_decide/list.json")).unwrap();
    let mut decision = list["decisions"][0].clone();
    decision["status"] = json!("overruled");
    decision["resolution"] =
        json!({"decision_id":"d41","option":"no","by":"owner","note":null,"standing_rule":false});
    let first = json!({"decision":decision,"changed":true,"future_result":true,"redo":{"required_count":3,"completed_count":0,"cancelled_count":0,"task_ids":["t14"],"next_after":"t14"}});
    let last = json!({"required_count":3,"completed_count":1,"cancelled_count":0,"task_ids":["t16","t18"],"next_after":null,"future_page":true});
    let script = vec![
        (
            "decision.resolve".into(),
            json!({"id":"d41","option":"no","note":null,"standing_rule":false}),
            json!({"result":first.clone()}),
        ),
        (
            "decision.get".into(),
            json!({"id":"d41","redo_after":"t14"}),
            json!({"result":{"decision":decision,"redo":last.clone()}}),
        ),
    ];
    let out = pages(&["decide", "d41", "no", "--format", "json"], script, false).await;
    assert!(out.status.success(), "{out:?}");
    let mut expected = first;
    let mut redo = last;
    redo["task_ids"] = json!(["t14", "t16", "t18"]);
    expected["redo"] = redo;
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        expected
    );
}

// Break caught: a stale/malformed/looping page writes partial output or triggers fallback.
#[tokio::test]
async fn bad_digest_pages_and_broken_output_fail_without_partial_stdout() {
    for mode in ["conflict", "cursor", "metadata", "empty"] {
        let mut script = page_script(&small_page(), 2);
        match mode {
            "conflict" => {
                script[1].2 = json!({"error":{"code":"conflict","message":"return digest is no longer current"}})
            }
            "cursor" => script[1].2["result"]["next_after"] = json!(1),
            "metadata" => script[1].2["result"]["snapshot_seq"] = json!(999),
            _ => script[1].2["result"]["rows"] = json!([]),
        };
        let out = pages(&["back"], script, false).await;
        assert!(!out.status.success(), "{mode}: {out:?}");
        assert!(out.stdout.is_empty(), "{mode}: {out:?}");
    }
    let out = pages(&["back"], page_script(&small_page(), 100), true).await;
    assert!(!out.status.success(), "{out:?}");
}

// Break caught: terminal controls survive escaping, or an actual option is put in a shell command.
#[tokio::test]
async fn digest_controls_are_data_and_shell_options_are_not_executed() {
    let mut page = small_page();
    page["rows"][0]["decision"]["request"]["title"] =
        json!("Bad\u{001b}]0;title\u{0007}\u{009b}31m\nnext");
    page["rows"][0]["decision"]["provisional"]["option"] = json!("`touch never` 'quoted'");
    let output = text(&["back"], "owner.back", report(page)).await;
    assert!(!output.contains(['\u{001b}', '\u{0007}', '\u{009b}']));
    assert!(output.contains("Answer: atm decide d11 <option>"));
    assert!(!output.contains("atm decide d11 `touch"));
}

#[tokio::test]
async fn argument_errors_do_not_contact_a_daemon() {
    for args in [
        vec!["away", "now"],
        vec!["away", "--cap", "2"],
        vec!["away", "--format", "yaml"],
        vec!["back", "d41"],
        vec!["back", "--format"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = command(dir.path(), &args)
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(!dir.path().join("fallback").exists());
    }
}
