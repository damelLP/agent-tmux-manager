//! Synthetic RPC boundaries with fixed time, real replay, and event-held lanes.
use super::*;
use crate::{
    org::{department_directory::DepartmentFiles, reporting_line, Request},
    server::methods::{CallContext, CallerId, CallerPane, MethodServices},
};
use serde_json::{json, Value};
use std::{
    future::{poll_fn, Future},
    path::{Path, PathBuf},
    pin::Pin,
    task::Poll,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn now() -> chrono::DateTime<chrono::Utc> {
    "2026-01-01T00:01:00Z".parse().unwrap()
}
fn caller(pane: Option<&str>) -> CallContext {
    CallContext {
        client_id: CallerId("report-test".into()),
        from_pane: pane.map(|p| CallerPane(p.into())),
    }
}
fn source() -> Vec<Value> {
    include_str!("../../../atm-org/tests/fixtures/report/q5-cost-source.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
async fn fixture(mut records: Vec<Value>) -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    // A skipped future event is deliberately the committed replay high-water mark.
    records.push(json!({"seq":15,"ts":"2026-01-01T00:00:08Z","kind":"future.fact","body":{"PRIVATE_FUTURE":true}}));
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log/events");
    std::fs::create_dir_all(&log).unwrap();
    let text = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(log.join("00000001.jsonl"), text).unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    assert_eq!(org.status().unwrap(), 15);
    (dir, org, cancel)
}
fn fixed_router(org: OrgHandle, files: reporting_line::ReportingFiles) -> MethodRouter {
    let mut router = MethodRouter::default();
    register(&mut router, org, files, DepartmentFiles::new(None), now).unwrap();
    router
}

// Break caught: a stale cursor after skipped replay, a nonfixed boundary clock,
// changed RPC envelope/defaults, missing metrics, or a supposedly read-only append.
#[tokio::test]
async fn golden_request_response_includes_the_atomic_replay_cursor_without_writes() {
    let (_dir, org, cancel) = fixture(source()).await;
    let router = fixed_router(
        org.clone(),
        super::reporting_line::ReportingFiles::default(),
    );
    let request: atm_protocol::ClientMessage =
        serde_json::from_str(include_str!("../../tests/fixtures/org_report/request.json")).unwrap();
    let atm_protocol::MessageType::Request {
        id,
        method,
        params,
        from_pane,
    } = request.message
    else {
        panic!("golden request")
    };
    let value = router
        .call(caller(from_pane.as_deref()), &method, params)
        .await
        .unwrap();
    let response = atm_protocol::DaemonMessage::response_ok(id, value.clone());
    let expected: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/org_report/response.json"
    ))
    .unwrap();
    assert_eq!(serde_json::to_value(response).unwrap(), expected);
    assert_eq!(
        router
            .call(caller(None), "org.report", Value::Null)
            .await
            .unwrap(),
        value
    );
    assert_eq!(org.status().unwrap(), 15);
    assert_eq!(
        org.tail(crate::org::TailParams::default())
            .await
            .unwrap()
            .len(),
        15
    );
    assert!(!value.to_string().contains("PRIVATE_"));
    cancel.cancel();
}

// Break caught: unknown/ill-typed/unbounded inputs, noncanonical cursors or
// missing referenced objects become successful defaults or a different error.
#[tokio::test]
async fn params_are_object_only_strict_bounded_and_checked_against_the_snapshot() {
    let (_dir, org, cancel) = fixture(source()).await;
    let router = fixed_router(
        org.clone(),
        super::reporting_line::ReportingFiles::default(),
    );
    for params in [
        json!([]),
        json!("bad"),
        json!({"unknown":true}),
        json!({"bucket":0}),
        json!({"bucket":1001}),
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"bucket":1.5}),
        json!({"since":123}),
        json!({"until":"2026-01-01"}),
        json!({"since":"2026-01-02T00:00:00Z"}),
        json!({"after":"t05"}),
        json!({"after":"t+5"}),
        json!({"after":"t18446744073709551616"}),
    ] {
        assert_eq!(
            router
                .call(caller(None), "org.report", params.clone())
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params",
            "{params}"
        );
    }
    for params in [json!({"project_id":"missing"}), json!({"after":"t999"})] {
        assert_eq!(
            router
                .call(caller(None), "org.report", params)
                .await
                .unwrap_err()
                .0
                .code,
            "not_found"
        );
    }
    assert_eq!(
        router
            .call(caller(Some("invalid")), "org.report", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "invalid_params"
    );
    for params in [
        json!({"bucket":1,"limit":1}),
        json!({"bucket":1000,"limit":1000}),
        json!({"since":now(),"until":now()}),
        json!({"since":"2026-01-01T01:00:00+01:00"}),
    ] {
        router
            .call(caller(None), "org.report", params)
            .await
            .unwrap();
    }
    assert_eq!(org.status().unwrap(), 15);
    cancel.cancel();
    let router = fixed_router(
        OrgHandle::disabled("offline"),
        super::reporting_line::ReportingFiles::default(),
    );
    assert_eq!(
        router
            .call(caller(None), "org.report", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "unavailable"
    );
}

// Break caught: a reply is trimmed to fit, a single oversized row is returned,
// or only task rows (not the full result envelope) enter the byte budget.
#[tokio::test]
async fn complete_oversized_reports_are_refused_instead_of_shrunk() {
    let mut records = source();
    for row in records.iter_mut().filter(|v| v["kind"] == "task.created") {
        row["body"]["title"] = json!("x".repeat(270_000));
    }
    let (_dir, org, cancel) = fixture(records).await;
    let router = fixed_router(
        org.clone(),
        super::reporting_line::ReportingFiles::default(),
    );
    assert_eq!(
        router
            .call(caller(None), "org.report", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    let page = router
        .call(caller(None), "org.report", json!({"limit":1}))
        .await
        .unwrap();
    assert_eq!(page["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_after"], "t5");
    assert_eq!(page["projects"][0]["tasks"]["open"], 1);
    assert_eq!(page["projects"][0]["tasks"]["accepted"], 1);
    assert_eq!(org.status().unwrap(), 15);
    cancel.cancel();

    let mut records = source();
    records[4]["body"]["title"] = json!("x".repeat(530_000));
    let (_dir, org, cancel) = fixture(records).await;
    let router = fixed_router(org, super::reporting_line::ReportingFiles::default());
    assert_eq!(
        router
            .call(caller(None), "org.report", json!({"limit":1}))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    cancel.cancel();
}

const TOOLS: &str = "title='Agent tools'\nprojects=['alpha','missing']\n";
const APPS: &str = "title='App delivery'\nprojects=['alpha','beta']\n";
const PARTIAL: &str = "department report may be incomplete: bad.toml: not a regular file";

fn department_dir(root: &Path, include_apps: bool, broken: bool) -> PathBuf {
    let dir = root.join("departments");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("tools.toml"), TOOLS).unwrap();
    if include_apps {
        std::fs::write(dir.join("apps.toml"), APPS).unwrap();
    }
    if broken {
        std::fs::create_dir(dir.join("bad.toml")).unwrap();
    }
    dir
}

fn tools_row() -> Value {
    json!({"id":"tools","title":"Agent tools","cost":{"microdollars":300000},"cost_unknown_tasks":1})
}

fn legacy_response() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/org_report/response.json"
    ))
    .unwrap()
}

fn router_on_department_lane(org: OrgHandle, departments: DepartmentFiles) -> MethodRouter {
    let mut router = MethodRouter::default();
    register(
        &mut router,
        org,
        reporting_line::ReportingFiles::default(),
        departments,
        now,
    )
    .unwrap();
    router
}

// Break caught: dropping the department shape/title, aggregating only the task
// page, or rewriting any legacy field or ledger row to produce the view.
#[tokio::test]
async fn golden_department_report_adds_only_scoped_cost_lines() {
    let (dir, org, cancel) = fixture(source()).await;
    let files = DepartmentFiles::new(Some(department_dir(dir.path(), true, false)));
    let router = router_on_department_lane(org.clone(), files);
    let mut expected = legacy_response();
    expected["result"]["departments"] = json!([
        {"id":"apps","title":"App delivery","cost":{"microdollars":300000},"cost_unknown_tasks":1},
        tools_row()
    ]);
    let result = router
        .call(caller(None), "org.report", json!({}))
        .await
        .unwrap();
    let response = atm_protocol::DaemonMessage::response_ok("golden".into(), result.clone());
    assert_eq!(serde_json::to_value(response).unwrap(), expected);
    for (params, task) in [
        (json!({"limit":1}), "t5"),
        (json!({"after":"t5","limit":1}), "t14"),
    ] {
        let page = router
            .call(caller(None), "org.report", params)
            .await
            .unwrap();
        assert_eq!(page["departments"], expected["result"]["departments"]);
        assert_eq!(page["projects"], result["projects"]);
        assert_eq!(page["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(page["tasks"][0]["task_id"], task);
    }
    assert_eq!(org.status().unwrap(), 15);
    assert_eq!(
        org.tail(crate::org::TailParams::default())
            .await
            .unwrap()
            .len(),
        15
    );
    cancel.cancel();
}

// Break caught: production registration drops its configured reader, scan
// errors abort the whole report, or files/removal are not read fresh.
#[tokio::test]
async fn report_keeps_loaded_departments_and_warns_once() {
    let (dir, org, cancel) = fixture(source()).await;
    let departments = department_dir(dir.path(), false, true);
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.org = org.clone();
    services.departments_dir = Some(departments.clone());
    let router = crate::server::methods::router(services).unwrap();
    let result = router
        .call(caller(None), "org.report", json!({}))
        .await
        .unwrap();
    assert_eq!(result["departments"], json!([tools_row()]));
    assert_eq!(result["warning"], PARTIAL);
    std::fs::remove_file(departments.join("tools.toml")).unwrap();
    let result = router
        .call(caller(None), "org.report", json!({}))
        .await
        .unwrap();
    assert!(result.get("departments").is_none());
    assert_eq!(result["warning"], PARTIAL);
    assert_eq!(
        result["projects"][0]["cost"],
        json!({"microdollars":300000})
    );
    cancel.cancel();
}

// Break caught: a configured but nonexistent directory adds keys or a warning.
#[tokio::test]
async fn report_omits_department_section_for_a_missing_folder() {
    let (dir, org, cancel) = fixture(source()).await;
    let router =
        router_on_department_lane(org, DepartmentFiles::new(Some(dir.path().join("missing"))));
    let result = router
        .call(caller(None), "org.report", json!({}))
        .await
        .unwrap();
    assert_eq!(result, legacy_response()["result"]);
    cancel.cancel();
}

// Break caught: a shared department imports invisible or out-of-window cost,
// or a project selector without a matching row makes up a known amount.
#[tokio::test]
async fn department_costs_follow_report_visibility_and_filters() {
    let root = tempfile::tempdir().unwrap();
    let roles = root.path().join(".atm/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(
        roles.join("build.md"),
        "+++\nharness='pi'\nmanager='owner'\n+++\n",
    )
    .unwrap();
    let mut records = source();
    records[0]["body"]["root"] = json!(root.path());
    records[13]["body"]["creator"] = json!("beta/review");
    records[13]["body"]["project_id"] = json!("beta");
    let (dir, org, cancel) = fixture(records).await;
    let router = router_on_department_lane(
        org,
        DepartmentFiles::new(Some(department_dir(dir.path(), true, false))),
    );
    let beta = json!([
        {"id":"apps","title":"App delivery","cost":null,"cost_unknown_tasks":1},
        {"id":"tools","title":"Agent tools","cost":null,"cost_unknown_tasks":0}
    ]);
    let alpha = json!([
        {"id":"apps","title":"App delivery","cost":{"microdollars":300000},"cost_unknown_tasks":0},
        {"id":"tools","title":"Agent tools","cost":{"microdollars":300000},"cost_unknown_tasks":0}
    ]);
    for (pane, params, task, rows) in [
        (Some("%2"), json!({}), "t14", &beta),
        (None, json!({"project_id":"beta"}), "t14", &beta),
        (None, json!({"since":"2026-01-01T00:00:07Z"}), "t14", &beta),
        (None, json!({"project_id":"alpha"}), "t5", &alpha),
        (None, json!({"until":"2026-01-01T00:00:07Z"}), "t5", &alpha),
    ] {
        let result = router
            .call(caller(pane), "org.report", params.clone())
            .await
            .unwrap();
        assert_eq!(result["departments"], *rows, "{pane:?} {params}");
        assert_eq!(result["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(result["tasks"][0]["task_id"], task);
    }
    cancel.cancel();
}

fn gate_report_snapshot(org: &OrgHandle) -> (OrgHandle, mpsc::Receiver<Request>) {
    let (requests, inbox) = mpsc::channel(1);
    (
        OrgHandle {
            requests,
            ..org.clone()
        },
        inbox,
    )
}

fn take_report_snapshot(
    inbox: &mut mpsc::Receiver<Request>,
) -> oneshot::Sender<(u64, atm_org::OrgState)> {
    let Request::ReportSnapshot(reply) = inbox.try_recv().unwrap() else {
        panic!("expected the report snapshot request");
    };
    reply
}

async fn assert_pending(mut call: Pin<&mut impl Future>) {
    poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

// Break caught: either lane starts only after the other returns, adding a
// serial wait or making the base wait for the optional scan to start.
#[tokio::test]
async fn report_starts_department_read_while_snapshot_is_pending() {
    let (dir, org, cancel) = fixture(source()).await;
    let snapshot = org.report_snapshot().await.unwrap();
    let (gated, mut inbox) = gate_report_snapshot(&org);
    let files = DepartmentFiles::new(Some(department_dir(dir.path(), false, false)));
    let held = files.slot.acquire().await.unwrap();
    let router = router_on_department_lane(gated, files.clone());
    let mut call = std::pin::pin!(router.call(caller(None), "org.report", json!({})));
    assert_pending(call.as_mut()).await;
    let reply = take_report_snapshot(&mut inbox);
    drop(held);
    assert!(
        matches!(
            files.slot.try_acquire(),
            Err(tokio::sync::TryAcquireError::NoPermits)
        ),
        "department scan was not queued while the snapshot was held"
    );
    assert!(reply.send(snapshot).is_ok());
    assert_eq!(call.await.unwrap()["departments"], json!([tools_row()]));
    cancel.cancel();
}

// Break caught: a timed out department lane aborts or suppresses the base view.
#[tokio::test]
async fn report_keeps_base_result_when_department_slot_times_out() {
    let (dir, org, cancel) = fixture(source()).await;
    let snapshot = org.report_snapshot().await.unwrap();
    let (gated, mut inbox) = gate_report_snapshot(&org);
    let files = DepartmentFiles::new(Some(department_dir(dir.path(), false, false)));
    let _held = files.slot.acquire().await.unwrap();
    let router = router_on_department_lane(gated, files.clone());
    let mut call = std::pin::pin!(router.call(caller(None), "org.report", json!({})));
    // The only waits below are gated and in memory. No detached reader can start.
    tokio::time::pause();
    assert_pending(call.as_mut()).await;
    assert!(take_report_snapshot(&mut inbox).send(snapshot).is_ok());
    assert_pending(call.as_mut()).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let mut result = call.await.unwrap();
    assert_eq!(
        result.as_object_mut().unwrap().remove("warning"),
        Some(json!(
            "department report may be incomplete: departments: reader timed out or is busy"
        ))
    );
    assert_eq!(result, legacy_response()["result"]);
    cancel.cancel();
}

// Break caught: adding the department section after charging the byte budget.
#[tokio::test]
async fn byte_budget_includes_department_rows_and_warning() {
    let mut expected = legacy_response()["result"].clone();
    expected["departments"] = json!([tools_row()]);
    expected["warning"] = json!(PARTIAL);
    let fixed_bytes = serde_json::to_vec(&expected).unwrap().len() - "Work".len();
    for extra in [0, 1] {
        let mut records = source();
        records[4]["body"]["title"] = json!("x".repeat(512 * 1024 - fixed_bytes + extra));
        let (dir, org, cancel) = fixture(records).await;
        let router = router_on_department_lane(
            org,
            DepartmentFiles::new(Some(department_dir(dir.path(), false, true))),
        );
        let reply = router.call(caller(None), "org.report", json!({})).await;
        if extra == 0 {
            assert_eq!(
                serde_json::to_vec(&reply.unwrap()).unwrap().len(),
                512 * 1024
            );
        } else {
            assert_eq!(reply.unwrap_err().0.code, "refused");
        }
        cancel.cancel();
    }
}

// Break caught: excluding seq/scope/project totals from the byte count, or
// rejecting an exactly full envelope instead of only one exceeding the limit.
#[tokio::test]
async fn byte_budget_includes_the_whole_envelope_and_allows_exactly_512_kib() {
    let expected: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/org_report/response.json"
    ))
    .unwrap();
    let fixed_bytes = serde_json::to_vec(&expected["result"]).unwrap().len() - "Work".len();
    for extra in [0, 1] {
        let mut records = source();
        records[4]["body"]["title"] = json!("x".repeat(512 * 1024 - fixed_bytes + extra));
        let (_dir, org, cancel) = fixture(records).await;
        let router = fixed_router(org, reporting_line::ReportingFiles::default());
        let reply = router.call(caller(None), "org.report", json!({})).await;
        if extra == 0 {
            assert_eq!(
                serde_json::to_vec(&reply.unwrap()).unwrap().len(),
                512 * 1024
            );
        } else {
            assert_eq!(reply.unwrap_err().0.code, "refused");
        }
        cancel.cancel();
    }
}

// Break caught: org.report creates a separate reader lane, swallows a lane
// infrastructure failure into partial totals, or makes owner reads walk roles.
#[tokio::test]
async fn report_uses_the_router_owned_lane_and_preserves_infrastructure_failure() {
    let (_dir, org, cancel) = fixture(source()).await;
    let files = super::reporting_line::ReportingFiles::default();
    let permit = files.slot.clone().acquire_owned().await.unwrap();
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.org = org.clone();
    let router = crate::server::methods::router_with_reporting(services, files.clone()).unwrap();
    let owner = router
        .call(caller(None), "org.report", json!({}))
        .await
        .unwrap();
    assert_eq!(owner["tasks"].as_array().unwrap().len(), 2);
    let mut report = Box::pin(router.call(caller(Some("%1")), "org.report", json!({})));
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(report.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    org.snapshot().await.unwrap(); // FIFO actor barrier, never a delay
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(report.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    files.slot.close(); // Event-synchronized infrastructure shutdown.
    let error = report.await.unwrap_err();
    assert_eq!(error.0.code, "unavailable");
    assert!(error.0.message.contains("context reader stopped"));
    assert_eq!(
        router
            .call(caller(Some("%99")), "org.report", json!({}))
            .await
            .unwrap()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(org.status().unwrap(), 15);
    drop(permit);
    cancel.cancel();
}
