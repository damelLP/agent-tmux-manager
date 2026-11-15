//! Synthetic project policy through real task methods; no harness or live files.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::OrgCommand;
use atmd::{
    org::OrgHandle,
    registry::spawn_registry,
    server::{
        methods::{
            router, CallContext, CallerId, CallerPane, MethodError, MethodRouter, MethodServices,
        },
        DaemonServer,
    },
};
use serde_json::{json, Value};
use std::{fs, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    router: MethodRouter,
    cancel: CancellationToken,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        let router = router(services).unwrap();
        let rig = Self {
            dir,
            org,
            router,
            cancel,
        };
        rig.project("first", "%7").await;
        rig
    }
    async fn project(&self, id: &str, pane: &str) {
        let root = self.dir.path().join(id);
        fs::create_dir_all(&root).unwrap();
        for command in [
            OrgCommand::RegisterProject {
                project_id: id.parse().unwrap(),
                root: root.to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Adopt {
                agent_id: format!("{id}/worker").parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            },
        ] {
            self.org.execute(command, chrono::Utc::now()).await.unwrap();
        }
    }
    fn intent(&self, id: &str, text: &str) {
        let path = self.dir.path().join(id).join(".atm/intent.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    async fn create(&self, pane: Option<&str>, params: Value) -> Result<Value, MethodError> {
        self.router
            .call(
                CallContext {
                    client_id: CallerId("intent-test".into()),
                    from_pane: pane.map(|p| CallerPane(p.into())),
                },
                "task.create",
                params,
            )
            .await
    }
    async fn refused(&self, pane: Option<&str>, params: Value, code: &str) {
        let before = self.org.snapshot().await.unwrap();
        let cursor = self.org.status().unwrap();
        let error = self.create(pane, params).await.unwrap_err();
        assert_eq!(error.0.code, code, "{error:?}");
        assert_eq!(self.org.snapshot().await.unwrap(), before);
        assert_eq!(self.org.status().unwrap(), cursor);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn params(measure: Value) -> Value {
    json!({"title":"A bounded task","measure":measure})
}

// Break caught: accepting a same-kind command or rubric that the intent did not approve.
#[tokio::test]
async fn exact_listed_measures_pass_and_unlisted_measures_leave_no_event() {
    let rig = Rig::new().await;
    rig.intent("first", "## How we will know\n- mechanical: `scripts/check`\n- reviewed: `.atm/measures/structure.md`\n- owner\n");
    for measure in [
        json!({"kind":"mechanical","command":"scripts/check"}),
        json!({"kind":"reviewed","rubric":".atm/measures/structure.md"}),
        json!({"kind":"owner"}),
    ] {
        assert_eq!(
            rig.create(Some("%7"), params(measure)).await.unwrap()["changed"],
            true
        );
    }
    for measure in [
        json!({"kind":"mechanical","command":"scripts/check --skip-tests"}),
        json!({"kind":"reviewed","rubric":".atm/measures/other.md"}),
    ] {
        rig.refused(Some("%7"), params(measure), "refused").await;
    }
    rig.intent(
        "first",
        "## How we will know\n- mechanical: scripts/check\n",
    );
    rig.refused(None, params(json!({"kind":"owner"})), "refused")
        .await;
}

// Break caught: caching the first intent, or treating malformed present policy as absent.
#[tokio::test]
async fn policy_is_read_fresh_and_missing_is_distinct_from_invalid() {
    let rig = Rig::new().await;
    let request = params(json!({"kind":"mechanical","command":"check"}));
    rig.create(None, request.clone()).await.unwrap();
    assert!(!rig.dir.path().join("first/.atm").exists());
    for text in [
        "",
        "# Goals\nNo measure section\n",
        "## How we will know\n",
        "## How we will know\n- unknown\n",
        "## How we will know\n- mechanical: other\n",
    ] {
        rig.intent("first", text);
        rig.refused(None, request.clone(), "refused").await;
    }
    rig.intent("first", "## How we will know\n- mechanical: check\n");
    rig.create(None, request.clone()).await.unwrap();
    fs::remove_file(rig.dir.path().join("first/.atm/intent.md")).unwrap();
    rig.create(None, request).await.unwrap();
}

// Break caught: selecting the daemon cwd or an arbitrary registered project for an owner.
#[tokio::test]
async fn project_selection_is_explicit_when_ambiguous_and_cannot_override_a_named_caller() {
    let rig = Rig::new().await;
    rig.project("second", "%8").await;
    rig.intent("first", "## How we will know\n- mechanical: first-check\n");
    rig.intent(
        "second",
        "## How we will know\n- mechanical: second-check\n",
    );
    let first = params(json!({"kind":"mechanical","command":"first-check"}));
    let mut second = params(json!({"kind":"mechanical","command":"second-check"}));
    rig.create(Some("%7"), first.clone()).await.unwrap();
    rig.refused(Some("%8"), first.clone(), "refused").await;
    rig.refused(None, first.clone(), "invalid_params").await;
    second["project_id"] = json!("second");
    rig.create(None, second.clone()).await.unwrap();
    rig.create(Some("%8"), second.clone()).await.unwrap();
    rig.refused(Some("%7"), second.clone(), "refused").await;
    second["project_id"] = json!("missing");
    rig.refused(None, second, "not_found").await;
    let mut first = first;
    first["project_id"] = json!("first");
    rig.create(None, first).await.unwrap();
}

// Break caught: ignoring a named caller's conflicting selector when its own policy allows the measure.
#[tokio::test]
async fn named_caller_cannot_select_another_project_with_an_own_listed_measure() {
    let rig = Rig::new().await;
    rig.project("second", "%8").await;
    rig.intent("first", "## How we will know\n- mechanical: first-check\n");
    rig.intent("second", "## How we will know\n- owner\n");
    let mut request = params(json!({"kind":"mechanical","command":"first-check"}));
    rig.create(Some("%7"), request.clone()).await.unwrap();
    request["project_id"] = json!("first");
    rig.create(Some("%7"), request.clone()).await.unwrap();
    request["project_id"] = json!("second");
    rig.refused(Some("%7"), request, "refused").await;
}

// Break caught: turning a missing project root/selector or a relative root into unchecked access.
#[tokio::test]
async fn no_registered_project_preserves_legacy_creation_but_bad_roots_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    let mut services = MethodServices::new(spawn_registry());
    services.org = org.clone();
    let router = router(services).unwrap();
    let caller = CallContext {
        client_id: CallerId("legacy".into()),
        from_pane: None,
    };
    router
        .call(
            caller.clone(),
            "task.create",
            params(json!({"kind":"owner"})),
        )
        .await
        .unwrap();
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "bad".parse().unwrap(),
            root: "relative".into(),
            move_root: false,
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(
        router
            .call(caller, "task.create", params(json!({"kind":"owner"})))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    cancel.cancel();
}

// Break caught: treating a missing/stale registered checkout as absent policy.
#[tokio::test]
async fn missing_project_roots_refuse_instead_of_skipping_policy() {
    for had_policy in [false, true] {
        let rig = Rig::new().await;
        let root = rig.dir.path().join("first");
        if had_policy {
            rig.intent("first", "## How we will know\n- owner\n");
            fs::rename(&root, rig.dir.path().join("moved")).unwrap();
        } else {
            fs::remove_dir(&root).unwrap();
        }
        let before = rig.org.snapshot().await.unwrap();
        let cursor = rig.org.status().unwrap();
        let error = rig
            .create(
                Some("%7"),
                params(json!({"kind":"mechanical","command":"unlisted-check"})),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "refused", "{error:?}");
        assert!(
            error.0.message.contains("project root missing"),
            "{error:?}"
        );
        assert_eq!(rig.org.snapshot().await.unwrap(), before);
        assert_eq!(rig.org.status().unwrap(), cursor);
        assert!(!root.exists());
    }
}

// Break caught: unbounded reads or policy bytes silently decoded/replaced on invalid UTF-8.
#[tokio::test]
async fn invalid_utf8_oversized_and_nonregular_policy_files_are_refused() {
    use std::os::unix::fs::symlink;
    let rig = Rig::new().await;
    rig.intent("first", "## How we will know\n- owner\n");
    let path = rig.dir.path().join("first/.atm/intent.md");
    let request = params(json!({"kind":"owner"}));
    for bytes in [vec![0xff], vec![b'x'; 256 * 1024 + 1]] {
        fs::write(&path, bytes).unwrap();
        rig.refused(None, request.clone(), "refused").await;
    }
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    rig.refused(None, request.clone(), "refused").await;
    fs::remove_dir(&path).unwrap();
    symlink("missing", &path).unwrap();
    rig.refused(None, request.clone(), "refused").await;
    fs::remove_file(&path).unwrap();
    let outside = rig.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("intent.md"), "## How we will know\n- owner\n").unwrap();
    fs::remove_dir(path.parent().unwrap()).unwrap();
    symlink(&outside, path.parent().unwrap()).unwrap();
    rig.refused(None, request, "refused").await;
}

// Break caught: a FIFO substituted for the intent hangs rather than refusing before open/read.
#[tokio::test]
async fn fifo_policy_is_refused_without_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let rig = Rig::new().await;
    rig.intent("first", "placeholder");
    let path = rig.dir.path().join("first/.atm/intent.md");
    fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: CString supplies a valid, NUL-terminated private fixture pathname.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    tokio::time::timeout(
        Duration::from_secs(30),
        rig.refused(None, params(json!({"kind":"owner"})), "refused"),
    )
    .await
    .unwrap();
}

// Break caught: policy validation living only in a test helper rather than wire method dispatch.
#[tokio::test]
async fn intent_success_and_refusal_wire_fixtures() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/intent/methods.json")).unwrap();
    let rig = Rig::new().await;
    rig.intent("first", "## How we will know\n- mechanical: check\n");
    let mut services = MethodServices::new(spawn_registry());
    services.org = rig.org.clone();
    let socket = rig.dir.path().join("intent.sock");
    let server = DaemonServer::with_services(&socket, services, rig.cancel.clone());
    let mut run = Box::pin(async move { server.run().await });
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(run.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let server = tokio::spawn(run);
    tokio::time::timeout(Duration::from_secs(30), async {
        for fixture in fixtures {
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            write
                .write_all(
                    b"{\"type\":\"connect\",\"protocol_version\":{\"major\":1,\"minor\":1}}\n",
                )
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            write
                .write_all(format!("{}\n", fixture["request"]).as_bytes())
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            if let Some(code) = fixture.get("error") {
                assert_eq!(response["error"]["code"], *code);
            } else {
                assert_eq!(response["result"]["changed"], true);
            }
        }
    })
    .await
    .unwrap();
    rig.cancel.cancel();
    server.await.unwrap().unwrap();
}
