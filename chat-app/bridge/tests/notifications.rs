#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
mod support;

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio};

use serde_json::{json, Value};
use support::{guard, Bridge, Rig, ORIGIN, OWNER};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::UnixListener,
    process::Child,
};

const PATH: &str = "/notify/subscription";
const PHONE: &str = "https://fcm.googleapis.com/wp/fixture:phone";
const REPLACEMENT: &str = "https://fcm.googleapis.com/fcm/send/fixture:replacement";

fn child_with_log(rig: &Rig, socket: &Path, dir: &Path) -> Child {
    rig.child(env!("CARGO_BIN_EXE_atm-chat-bridge"))
        .args([
            "--port",
            "0",
            "--owner-login",
            OWNER,
            "--public-origin",
            ORIGIN,
            "--socket",
        ])
        .arg(socket)
        .arg("--notify-dir")
        .arg(dir)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

async fn stop_and_log(mut child: Child) -> String {
    child.kill().await.unwrap();
    let mut log = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .await
        .unwrap();
    log
}

async fn settings(bridge: &Bridge) -> Value {
    let (head, body) = bridge.http("GET", PATH, &[OWNER]).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("cache-control: no-store"));
    serde_json::from_str(&body).unwrap()
}

async fn change(bridge: &Bridge, origin: &str, endpoint: &str, on: bool) -> String {
    let (head, _) = bridge
        .file_http(
            "POST",
            PATH,
            &[
                ("Tailscale-User-Login", OWNER),
                ("Origin", origin),
                ("Content-Type", "application/json"),
            ],
            &serde_json::to_vec(&json!({"endpoint":endpoint,"on":on})).unwrap(),
            false,
        )
        .await;
    head
}

#[tokio::test]
async fn subscription_is_owner_only_origin_bound_and_stable_over_restart() {
    guard(async {
        let rig = Rig::new();
        let dir = rig.path("notify");
        let args = [Path::new("--notify-dir"), dir.as_path()];
        let bridge = rig.bridge_with(&rig.path("offline.sock"), &args).await;
        let (head, _) = bridge.http("GET", PATH, &[OWNER]).await;
        assert!(head.contains("x-content-type-options: nosniff"), "{head}");
        let first = settings(&bridge).await;
        assert_eq!(first["key"].as_str().unwrap().len(), 87);
        assert!(first["endpoint"].is_null());
        for logins in [&[][..], &["other@example.invalid"][..], &[OWNER, OWNER][..]] {
            for path in [PATH, "/notify/latest"] {
                let (head, _) = bridge.http("GET", path, logins).await;
                assert!(head.starts_with("HTTP/1.1 403"), "{head}");
                assert!(head.contains("x-content-type-options: nosniff"), "{head}");
            }
        }
        assert!(bridge
            .http("GET", "/notify/latest", &[OWNER])
            .await
            .0
            .starts_with("HTTP/1.1 204"));
        assert!(change(&bridge, "https://elsewhere.invalid", PHONE, true)
            .await
            .starts_with("HTTP/1.1 403"));
        assert!(change(
            &bridge,
            ORIGIN,
            "https://other.example.invalid/wp/token",
            true
        )
        .await
        .starts_with("HTTP/1.1 400"));
        assert!(change(&bridge, ORIGIN, PHONE, true)
            .await
            .starts_with("HTTP/1.1 204"));
        assert_eq!(settings(&bridge).await["endpoint"], PHONE);
        bridge.stop().await;
        let bridge = rig.bridge_with(&rig.path("offline.sock"), &args).await;
        assert_eq!(
            settings(&bridge).await,
            json!({"key":first["key"],"endpoint":PHONE})
        );
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            assert_eq!(mode(&entry.path()), 0o600);
        }
        assert!(change(&bridge, ORIGIN, REPLACEMENT, true)
            .await
            .starts_with("HTTP/1.1 204"));
        assert!(change(&bridge, ORIGIN, PHONE, false)
            .await
            .starts_with("HTTP/1.1 204"));
        assert_eq!(settings(&bridge).await["endpoint"], REPLACEMENT);
        assert!(change(&bridge, ORIGIN, REPLACEMENT, false)
            .await
            .starts_with("HTTP/1.1 204"));
        assert!(settings(&bridge).await["endpoint"].is_null());
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_missing_vapid_key_cannot_keep_a_subscription_to_its_old_key() {
    guard(async {
        let rig = Rig::new();
        let dir = rig.path("notify");
        let args = [Path::new("--notify-dir"), dir.as_path()];
        let bridge = rig.bridge_with(&rig.path("offline.sock"), &args).await;
        let first = settings(&bridge).await;
        assert!(change(&bridge, ORIGIN, PHONE, true)
            .await
            .starts_with("HTTP/1.1 204"));
        bridge.stop().await;
        fs::remove_file(dir.join(".vapid.pem")).unwrap();
        let bridge = rig.bridge_with(&rig.path("offline.sock"), &args).await;
        let regenerated = settings(&bridge).await;
        assert!(
            regenerated["endpoint"].is_null(),
            "old endpoint is bound to the lost key"
        );
        assert_ne!(regenerated["key"], first["key"]);
        assert!(!dir.join(".subscription").exists());
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_notification_state_reports_why_without_exposing_subscription_bytes() {
    guard(async {
        let rig = Rig::new();
        let dir = rig.path("notify");
        let args = [Path::new("--notify-dir"), dir.as_path()];
        let bridge = rig.bridge_with(&rig.path("offline.sock"), &args).await;
        assert!(change(&bridge, ORIGIN, PHONE, true)
            .await
            .starts_with("HTTP/1.1 204"));
        bridge.stop().await;
        fs::write(dir.join(".subscription"), "private fixture endpoint").unwrap();
        let mut child = child_with_log(&rig, &rig.path("offline.sock"), &dir);
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        while !stdout
            .next_line()
            .await
            .unwrap()
            .expect("chat must still start")
            .starts_with("Local: http://")
        {}
        let log = stop_and_log(child).await;
        assert!(
            log.contains("notification") && log.contains("unsupported push origin"),
            "{log}"
        );
        assert!(!log.contains("private fixture endpoint"), "{log}");
    })
    .await;
}

#[tokio::test]
async fn watch_reports_failure_without_logging_every_reconnect() {
    guard(async {
        let rig = Rig::new();
        let sock = rig.path("bridge.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let child = child_with_log(&rig, &sock, &rig.path("notify"));
        for _ in 0..4 {
            // The next connection fences handling of the preceding EOF.
            let (conn, _) = listener.accept().await.unwrap();
            drop(conn);
        }
        let log = stop_and_log(child).await;
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("notification watch"))
                .count(),
            1,
            "{log}"
        );
    })
    .await;
}
