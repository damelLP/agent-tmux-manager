//! Explicit private-server smoke test for the actual atmd actor/real tmux seam.
#![allow(clippy::unwrap_used)]
use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use atmd::control::{reservation::*, service::spawn_control, SpawnContext};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

async fn verify_private(client: &RealTmuxClient, root: &str) {
    let identity = client
        .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
        .await
        .unwrap();
    let (socket, pid) = identity.trim_end().split_once('\t').unwrap();
    assert!(std::path::Path::new(socket).starts_with(root));
    assert!(pid.parse::<u32>().unwrap() > 0);
}

#[tokio::test]
#[ignore = "requires explicit sealed environment and private tmux server"]
async fn reserved_real_tmux_effects_and_guard_rejection() {
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    assert!(root.starts_with("/tmp/xxxxxxxxxxx"));
    assert_ne!(std::env::var("ATM_SOCKET").unwrap(), "/tmp/atm.sock");
    let label = std::env::var("ATM_TMUX_SOCKET").unwrap();
    let client = RealTmuxClient::with_socket(label);
    client
        .raw_command(
            "new-session",
            &["-d", "-s", "private", "-x", "80", "-y", "24"],
        )
        .await
        .unwrap();
    verify_private(&client, &root).await;
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("real pane probe"),
        cancel.clone(),
    );
    let verifier = client.clone();
    let private_root = root.clone();
    let result = control
        .run_reserved(
            ReservationRequest::new(
                "project/lead".parse().unwrap(),
                None,
                ReservationKind::Respawn,
                WaitPolicy::Wait,
                Instant::now() + Duration::from_secs(60),
            ),
            move |r| async move {
                let placement = PaneId::parse("%0")?;
                assert!(r.inspect(&placement).await?.is_some());
                let pane = r
                    .split(
                        SpawnContext::new("%0".into(), None, false),
                        PaneDirection::Below,
                        "50%".into(),
                    )
                    .await?;
                r.tag(&pane, ExpectedTag::Absent).await?;
                assert_eq!(
                    r.inspect(&pane).await?.unwrap().local_agent_tag.as_deref(),
                    Some("project/lead")
                );
                for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
                    let capture = r.capture(&pane, format).await?;
                    assert_eq!(
                        capture.text.split_terminator('\n').count(),
                        usize::from(capture.inspection.pane.height)
                    );
                }
                assert!(matches!(
                    r.keys_guarded(
                        &pane,
                        r.agent().clone(),
                        PaneKey::Enter,
                        CaptureFormat::Plain,
                        |_| false
                    )
                    .await,
                    Err(ReservationError::ScreenChanged { .. })
                ));
                r.paste_guarded(
                    &pane,
                    r.agent().clone(),
                    "# harmless reserved probe;".into(),
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await?;
                r.keys_guarded(
                    &pane,
                    r.agent().clone(),
                    PaneKey::Enter,
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await?;
                r.untag(&pane, r.agent().clone()).await?;
                assert_eq!(r.inspect(&pane).await?.unwrap().local_agent_tag, None);
                r.tag(&pane, ExpectedTag::Absent).await?;
                verify_private(&verifier, &private_root).await;
                r.kill(
                    &pane,
                    r.agent().clone(),
                    serde_json::json!({"probe":true}),
                    chrono::Utc::now(),
                )
                .await?;
                assert!(r.inspect(&pane).await?.is_none());
                Ok::<_, ReservationError>(())
            },
            |_, _| async { Ok(()) },
        )
        .await;
    cancel.cancel();
    actor.await.unwrap();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    result.unwrap();
}
