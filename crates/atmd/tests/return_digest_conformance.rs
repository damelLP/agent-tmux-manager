//! Synthetic return-report method fixtures on a real private server, no harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::OrgCommand;
use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use atmd::{
    org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
    registry::spawn_registry,
    server::{methods::MethodServices, DaemonServer},
};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

async fn call(
    socket: &Path,
    method: &str,
    params: Value,
    pane: Option<&str>,
) -> Result<Value, atm_protocol::ResponseError> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        for message in [
            ClientMessage::connect(Some("digest-fixture".into())),
            ClientMessage::request(
                "digest".into(),
                method.into(),
                params,
                pane.map(str::to_owned),
            ),
        ] {
            write
                .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
        }
        match serde_json::from_str::<DaemonMessage>(&line).unwrap() {
            DaemonMessage::Response {
                payload: ResponsePayload::Result(value),
                ..
            } => Ok(value),
            DaemonMessage::Response {
                payload: ResponsePayload::Error(error),
                ..
            } => Err(error),
            other => panic!("unexpected response {other:?}"),
        }
    })
    .await
    .unwrap()
}

// Break caught: router schemas/envelopes differ from the owner-only bounded contract.
#[tokio::test]
async fn return_digest_method_fixtures_and_frozen_pages() {
    let dir = tempfile::tempdir().unwrap();
    let org_cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
    for name in ["alpha", "beta", "gamma"] {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        org.execute(
            OrgCommand::RegisterProject {
                project_id: name.parse().unwrap(),
                root: root.to_str().unwrap().into(),
                move_root: false,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    org.execute(
        OrgCommand::Adopt {
            agent_id: "alpha/lead".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let mut services = MethodServices::new(spawn_registry());
    services.org = org.clone();
    services.agents = Some(AgentHandle::disabled("fixture"));
    services.messages = Some(MessageHandle::disabled("fixture"));
    let (control, _) = atmd::control::service::spawn_control(
        Box::new(atm_tmux::MockTmuxClient::new()),
        services.registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    services.control = Some(control);
    let socket = dir.path().join("atm.sock");
    let server = DaemonServer::with_services(socket.clone(), services, cancel.clone());
    let mut run = Box::pin(async move { server.run().await });
    // run binds synchronously before its first receive; no readiness polling.
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(run.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let task = tokio::spawn(run);
    let back = call(&socket, "owner.back", json!({}), None).await.unwrap();
    assert_eq!(back["changed"], false);
    assert_eq!(back["digest"]["rows"], json!([]));
    let seq = back["digest"]["snapshot_seq"].clone();
    assert_eq!(
        call(&socket, "owner.digest", json!({"snapshot_seq":seq}), None)
            .await
            .unwrap(),
        back["digest"]
    );
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/away/digest_methods.json")).unwrap();
    for fixture in fixtures {
        let error = call(
            &socket,
            fixture["method"].as_str().unwrap(),
            fixture["params"].clone(),
            fixture["pane"].as_str(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, fixture["error"].as_str().unwrap(), "{fixture}");
    }
    call(&socket, "owner.back", json!({}), None).await.unwrap();
    assert_eq!(
        call(&socket, "owner.digest", json!({"snapshot_seq":seq}), None)
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    org_cancel.cancel();
}
