//! Public consumer contract, compiled without the library's cfg(test).
//! Probe fixtures: tmux 3.8, 2026-11-14, private shell/option capture.
#![allow(clippy::unwrap_used)] // Test helpers, as in the existing integration suites.
use atm_tmux::{mock::MockCall, PaneInfo, TmuxClient};
use atmd::control::{
    reservation::{
        testing::ScriptedTmuxClient, CaptureFormat, PaneCapture, PaneId, PaneInspection, PaneKey,
        ReservationError, ReservationKind, ReservationRequest, WaitPolicy,
    },
    service::spawn_control,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const FORMAT: &str = "ATM-PANE-v1\t#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{pane_dead}\t#{pane_in_mode}\t#{bracket_paste_flag}\t#{cursor_x}\t#{cursor_y}";

fn inspection(pane: &str, tag: Option<&str>) -> PaneInspection {
    PaneInspection::new(
        PaneInfo {
            pane_id: pane.into(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: 2353666,
            width: 40,
            height: 8,
            is_active: true,
        },
        tag.map(str::to_owned),
        false,
        false,
        Some(false),
        5,
        1,
    )
}

fn fixtures() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../src/control/fixtures/tmux_3.8_reserved_observations.json"
    ))
    .unwrap()
}

async fn metadata(client: &dyn TmuxClient, pane: &str) -> String {
    let filter = format!("#{{==:#{{pane_id}},{pane}}}");
    client
        .raw_command("list-panes", &["-a", "-f", &filter, "-F", FORMAT])
        .await
        .unwrap()
}

async fn tag(client: &dyn TmuxClient, pane: &str) -> String {
    client
        .raw_command(
            "show-options",
            &["-p", "-q", "-v", "-t", pane, "@atm-agent"],
        )
        .await
        .unwrap()
}

async fn frame(client: &dyn TmuxClient, pane: &str, format: CaptureFormat) -> String {
    let mut args = vec!["-p", "-t", pane, FORMAT, ";", "capture-pane", "-p", "-N"];
    if format == CaptureFormat::Ansi {
        args.push("-e");
    }
    args.extend([
        "-t",
        pane,
        ";",
        "display-message",
        "-p",
        "-t",
        pane,
        "ATM-END\t#{pane_id}",
    ]);
    client.raw_command("display-message", &args).await.unwrap()
}

#[tokio::test]
async fn guarded_input_contract_runs_predicates_against_fresh_scripted_captures() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    let mut safe = inspection("%0", Some("project/lead"));
    safe.bracketed_paste = Some(true);
    for format in [CaptureFormat::Ansi, CaptureFormat::Plain] {
        client
            .script_capture(&PaneCapture::new(
                "probe     \nready     \n\n\n\n\n\n\n".into(),
                safe.clone(),
                format,
            ))
            .unwrap();
    }
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(client),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("consumer test"),
        cancel.clone(),
    );
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    control
        .run_reserved(
            ReservationRequest::new(
                "project/lead".parse().unwrap(),
                Some(PaneId::parse("%0").unwrap()),
                ReservationKind::Delivery,
                WaitPolicy::Wait,
                Instant::now() + Duration::from_secs(30),
            ),
            move |r| async move {
                let pane = PaneId::parse("%0").unwrap();
                let key_called = called.clone();
                let (key_seen, key_received) = tokio::sync::oneshot::channel();
                let (paste_seen, paste_received) = tokio::sync::oneshot::channel();
                assert!(matches!(
                    r.keys_guarded(
                        &pane,
                        r.agent().clone(),
                        PaneKey::Enter,
                        CaptureFormat::Ansi,
                        move |capture: &PaneCapture| {
                            key_called.store(true, Ordering::SeqCst);
                            let _ = key_seen.send(capture.inspection.in_mode);
                            false
                        }
                    )
                    .await,
                    Err(ReservationError::ScreenChanged { .. })
                ));
                assert!(matches!(
                    r.paste_guarded(
                        &pane,
                        r.agent().clone(),
                        "message;\nEnter".into(),
                        CaptureFormat::Plain,
                        move |capture: &PaneCapture| {
                            called.store(true, Ordering::SeqCst);
                            let _ = paste_seen.send(capture.inspection.dead);
                            false
                        }
                    )
                    .await,
                    Err(ReservationError::ScreenChanged { .. })
                ));
                assert!(!key_received.await.unwrap());
                assert!(!paste_received.await.unwrap());
                assert_eq!(
                    ReservationError::ScreenChanged { pane }.to_string(),
                    "pane %0 no longer passes the approved screen check; nothing was sent"
                );
                Ok::<_, ReservationError>(())
            },
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap();
    assert!(observed.load(Ordering::SeqCst));
    assert!(mock
        .calls()
        .iter()
        .all(|call| matches!(call, MockCall::RawCommand { .. })));
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn consumer_scripts_are_per_pane_and_preserve_empty_and_malformed_local_tags() {
    let client = ScriptedTmuxClient::new();
    let a = PaneId::parse("%0").unwrap();
    let b = PaneId::parse("%7").unwrap();
    client
        .script_inspect(&a, Some(&inspection("%0", Some(""))))
        .unwrap();
    client
        .script_inspect(&b, Some(&inspection("%7", Some("weird\nquote\t\""))))
        .unwrap();
    // Interleaving reads across two panes must not consume a global raw FIFO.
    assert_eq!(
        metadata(&client, "%0").await,
        fixtures()["filtered_list"].as_str().unwrap()
    );
    assert!(metadata(&client, "%7").await.contains("\t%7\t"));
    assert_eq!(
        tag(&client, "%7").await,
        fixtures()["malformed_tag"].as_str().unwrap()
    );
    assert_eq!(
        tag(&client, "%0").await,
        fixtures()["empty_tag"].as_str().unwrap()
    );
    assert_eq!(client.pending_reads(&a).unwrap(), 0);
    assert_eq!(client.pending_reads(&b).unwrap(), 0);
    client.script_tag_check(&inspection("%0", None)).unwrap();
    metadata(&client, "%0").await;
    assert_eq!(
        tag(&client, "%0").await,
        fixtures()["absent_tag"].as_str().unwrap()
    );
    client.paste_text("%0", "literal;\nEnter").await.unwrap();
    assert!(
        matches!(client.mock().calls().last(), Some(MockCall::PasteText { text, .. }) if text == "literal;\nEnter")
    );
}

#[tokio::test]
async fn consumer_capture_scripts_match_real_plain_and_ansi_probe_framing() {
    let client = ScriptedTmuxClient::new();
    let pane = PaneId::parse("%0").unwrap();
    for (format, key, text) in [
        (
            CaptureFormat::Plain,
            "plain",
            "probe     \nready     \n\n\n\n\n\n\n",
        ),
        (
            CaptureFormat::Ansi,
            "ansi",
            "\x1b[31mprobe  \x1b[39m   \nready     \n\n\n\n\n\n\n",
        ),
    ] {
        client
            .script_capture(&PaneCapture {
                text: text.into(),
                inspection: inspection("%0", Some("project/lead")),
                format,
            })
            .unwrap();
        assert_eq!(client.pending_reads(&pane).unwrap(), 3);
        metadata(&client, "%0").await;
        assert_eq!(
            frame(&client, "%0", format).await,
            fixtures()[key].as_str().unwrap()
        );
        assert_eq!(
            tag(&client, "%0").await,
            fixtures()["agent_tag"].as_str().unwrap()
        );
        assert_eq!(client.pending_reads(&pane).unwrap(), 0);
    }
}

#[tokio::test]
async fn consumer_missing_scripts_fail_closed_and_missing_pane_is_explicit() {
    let client = ScriptedTmuxClient::new();
    let pane = PaneId::parse("%0").unwrap();
    client.script_inspect(&pane, None).unwrap();
    assert_eq!(
        metadata(&client, "%0").await,
        fixtures()["filtered_missing"].as_str().unwrap()
    );
    assert_eq!(client.pending_reads(&pane).unwrap(), 0);
    assert!(client
        .raw_command(
            "show-options",
            &["-p", "-q", "-v", "-t", "%0", "@atm-agent"]
        )
        .await
        .is_err());
    client
        .script_inspect(&pane, Some(&inspection("%0", None)))
        .unwrap();
    // Wrong read order is an error, not a plausible but shifted fixture.
    assert!(client
        .raw_command(
            "show-options",
            &["-p", "-q", "-v", "-t", "%0", "@atm-agent"]
        )
        .await
        .is_err());
    assert_eq!(client.pending_reads(&pane).unwrap(), 2);
    metadata(&client, "%0").await;
    tag(&client, "%0").await;
    assert!(client
        .script_inspect(&pane, Some(&inspection("%7", None)))
        .is_err());
    assert!(client
        .script_capture(&PaneCapture {
            text: "truncated".into(),
            inspection: inspection("%0", None),
            format: CaptureFormat::Plain
        })
        .is_err());
}

#[tokio::test]
async fn consumer_scripted_client_works_with_real_control_handle() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    mock.set_next_pane_id("%42");
    client.script_tag_check(&inspection("%42", None)).unwrap();
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(client),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("consumer test"),
        cancel.clone(),
    );
    let pane = control
        .split_spawn(
            atmd::control::SpawnContext::new("%0".into(), None, false),
            atm_tmux::PaneDirection::Below,
            "50%".into(),
        )
        .await
        .unwrap();
    assert_eq!(pane, "%42");
    control
        .type_spawn_command(pane, "probe".into())
        .await
        .unwrap();
    assert!(
        matches!(mock.calls().last(), Some(MockCall::SendKeys { pane, keys }) if pane == "%42" && keys == "Enter")
    );
    cancel.cancel();
    task.await.unwrap();
}
