//! Lifetime delivery notices on a real private daemon and model-free stand-in.
//! Recorder acknowledgments and later FIFO notes fence negative assertions;
//! there is no quiet-time wait. All processes belong to the shared sealed rig.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)] // This test uses a subset of the shared fixture.
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)] // Focus and delivery-input controls are not used here.
#[path = "support/message_delivery.rs"]
mod support;

use serde_json::{json, Value};
use std::{io::Read, os::fd::AsRawFd, os::unix::net::UnixListener, time::Duration};
use support::{notice, Confirmation, MessageRig};

struct Recorder {
    listener: UnixListener,
}
impl Recorder {
    fn configure(rig: &mut MessageRig) -> Self {
        let socket = rig.test.rig.root().join("record.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let command = vec!["python3", "-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()); s.close()", socket.to_str().unwrap()];
        let path = rig.test.rig.root().join("config/atm/config.toml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(&format!(
            "\n[notify]\ncommand = {}\ndelay_secs = 0\n",
            serde_json::to_string(&command).unwrap()
        ));
        std::fs::write(path, config).unwrap();
        rig.test.rig.restart_daemon();
        Self { listener }
    }

    fn next(&self) -> String {
        let mut fd = libc::pollfd {
            fd: self.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Kernel readiness is the synchronization; 60 s is only a hang guard.
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 60_000) },
            1,
            "recorder connection hang guard"
        );
        let (mut stream, _) = self.listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        eprintln!("recorded argv argument: {text}");
        text
    }

    fn expect(&self, id: &str, from: &str) {
        assert_eq!(
            self.next(),
            format!("{id} from {from}: [{id}] Read: atm inbox --id {id} --for owner")
        );
    }
}

#[test]
fn private_daemon_notifier_and_stand_in_boundaries() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let recorder = Recorder::configure(&mut rig);
    let agent = rig.ready_agent("worker", stand_in::StandInMode::Pi);
    let barrier = rig
        .test
        .rig
        .control(&agent.pane, json!({"command":"barrier"}));
    eprintln!("stand-in barrier: {barrier}");
    let id = rig.send(None, "owner", "notifier preflight");
    recorder.expect(&id, "owner");
    let before = rig.test.rig.daemon_pids();
    rig.test.rig.restart_daemon();
    assert_ne!(rig.test.rig.daemon_pids(), before);
    let fence = rig.send(None, "owner", "after real restart");
    recorder.expect(&fence, "owner");
    rig.test.rig.close();
}

#[test]
fn committed_notice_keeps_id_body_and_read_state_across_real_restarts() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let recorder = Recorder::configure(&mut rig);
    let agent = rig.ready_agent("worker", stand_in::StandInMode::Pi);
    rig.call(
        "agent.down",
        json!({"agent": agent.id, "kill": false}),
        None,
    )
    .unwrap();
    let mut watch = rig.watch();
    let source = rig.send(None, &agent.id, "first source");
    let first = watch.wait("first blocked note", notice(&format!("{source} blocked:")));
    let note_id = format!("m{}", first["seq"]);
    recorder.expect(&note_id, "system");
    rig.call("msg.ack", json!({"ids": [note_id]}), None)
        .unwrap();
    let original = rig.inbox_entry(None, None, &note_id);
    assert!(!original["read_at"].is_null());
    drop(watch);

    for round in 0..2 {
        let old_pids = rig.test.rig.daemon_pids();
        rig.test.rig.restart_daemon();
        assert_ne!(rig.test.rig.daemon_pids(), old_pids);
        let mut watch = rig.watch();
        let follower = rig.send(None, &agent.id, &format!("new source {round}"));
        let later = watch.wait(
            "later source notice fences the earlier FIFO evaluation",
            notice(&format!("{follower} blocked:")),
        );
        recorder.expect(&format!("m{}", later["seq"]), "system");
        // A later item in the same serial notifier queue fences any duplicate.
        let fence = rig.send(None, "owner", "notification fence");
        recorder.expect(&fence, "owner");
        let owner = rig
            .call(
                "msg.inbox",
                json!({"unread_only": false, "limit": 100}),
                None,
            )
            .unwrap();
        let matches: Vec<&Value> = owner["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["content"]["body"]
                    .as_str()
                    .is_some_and(|body| body.starts_with(&format!("{source} blocked:")))
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "same source/reason repeated after restart: {owner}"
        );
        assert_eq!(*matches[0], original);
        assert_eq!(
            matches[0]["content"]["delivery_notice"],
            json!({"source_message_id": source, "reason": "blocked.agent_unavailable"})
        );
        let entry = rig.inbox_entry(None, Some(&agent.id), &follower);
        assert_eq!(entry["delivery"]["blocked_by"], source);
        assert_eq!(entry["delivery"]["attempt"], 0);
        watch.catch_up(&rig);
        eprintln!(
            "restart {round}: original note {note_id} retained; follower {follower}: {}",
            entry["delivery"]
        );
    }
    rig.test.rig.close();
}
