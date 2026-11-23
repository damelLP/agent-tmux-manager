//! Run real atmd/tmux against an inert replay (never against a model CLI).
#![cfg(target_os = "linux")]
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
#[path = "support/stand_in.rs"]
mod stand_in;

use stand_in::StandInRig;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

const SINK: &str = include_str!("fixtures/legacy_survey_sink.py");
const FRAME: &str = include_str!(
    "../crates/atm-claude-adapter/tests/fixtures/claude_feedback_survey_2.1.327.styled.txt"
);

struct Replay {
    rig: StandInRig,
    pane: String,
    input: UnixStream,
}

impl Replay {
    fn start(draft: &str) -> Self {
        let rig = StandInRig::start();
        let frame = FRAME
            .lines()
            .skip(1)
            .map(|line| line.strip_suffix('|').unwrap().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        // Synthetic: replace only the empty editor's first grid cell.
        let frame = if draft.is_empty() {
            frame
        } else {
            let marked = frame.replacen("❯\u{a0} ", &format!("❯\u{a0}{draft}"), 1);
            assert_ne!(marked, frame);
            marked
        };
        assert!(rig
            .tmux(&["set-option", "-g", "default-size", "217x120"])
            .status
            .success());
        for (path, content) in [("frame.txt", frame.as_str()), ("sink.py", SINK)] {
            std::fs::write(rig.root().join(path), content).unwrap();
        }
        let listener = UnixListener::bind(rig.root().join("input.sock")).unwrap();
        let out = rig.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            rig.root().to_str().unwrap(),
            "exec python3 sink.py",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pane = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        // Bound accept on the actual readiness event, not on a poll cadence.
        use std::os::fd::AsRawFd;
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 30_000) },
            1,
            "sink did not render"
        );
        let (mut input, _) = listener.accept().unwrap();
        input
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut ready = [0];
        input.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'r']);
        let capture = rig.tmux(&["capture-pane", "-p", "-e", "-N", "-t", &pane]);
        assert!(capture.status.success());
        let text = String::from_utf8(capture.stdout).unwrap();
        assert!(text.contains("How is Claude doing this session? (optional)"));
        let facts = atm_core::styled_screen::StyledScreen::parse(&text).unwrap();
        assert!(
            facts.text().contains(&format!("❯\u{a0}{draft}")),
            "editor draft was not rendered"
        );
        Self { rig, pane, input }
    }

    fn atm(&self, action: &str, args: &[&str]) -> std::process::Output {
        let mut cmd = vec![action, self.pane.as_str()];
        cmd.extend_from_slice(args);
        self.rig.atm(&cmd)
    }

    fn fence_input(&mut self) -> Vec<u8> {
        // A non-digit sentinel through tmux fences the earlier RPC keys.
        assert!(self
            .rig
            .tmux(&["send-keys", "-t", &self.pane, "F12"])
            .status
            .success());
        let mut length = [0; 4];
        self.input.read_exact(&mut length).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
        self.input.read_exact(&mut bytes).unwrap();
        bytes
    }
}

#[test]
fn send_will_not_submit_a_synthetic_survey_choice() {
    let mut replay = Replay::start("1");
    let output = replay.atm("send", &[""]);
    let input = replay.fence_input();
    assert_eq!(output.status.code(), Some(1), "sent {input:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("feedback survey"));
    assert!(output.stdout.is_empty());
    assert!(input.is_empty(), "received {input:?}");
}

#[test]
fn replies_refuse_choice_submission_and_navigation_before_any_key() {
    for (draft, args) in [
        ("1", [&["-y"][..], &["-o", "1"], &[][..]].as_slice()),
        ("", [&["-o", "2"][..]].as_slice()),
    ] {
        let mut replay = Replay::start(draft);
        for args in args {
            let output = replay.atm("reply", args);
            let input = replay.fence_input();
            assert_eq!(output.status.code(), Some(1), "sent {input:?} for {args:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("feedback survey"));
            assert!(output.stdout.is_empty());
            assert!(input.is_empty(), "{args:?} received {input:?}");
        }
    }
}

#[test]
fn no_motion_reply_cancel_and_interrupt_keep_their_keys() {
    let mut replay = Replay::start("");
    for (action, args, keys) in [
        ("reply", &["-o", "1"][..], &b"\r"[..]),
        ("reply", &["-y"][..], &b"\r"[..]),
        ("reply", &["-n"][..], &b"\x1b"[..]),
        ("interrupt", &[][..], &b"\x03"[..]),
    ] {
        let output = replay.atm(action, args);
        assert!(
            output.status.success(),
            "{action} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(replay.fence_input(), keys);
    }
}

#[test]
fn benign_send_still_uses_bracketed_paste_and_enter() {
    let mut replay = Replay::start("");
    assert!(replay.atm("send", &["[sealed replay]"]).status.success());
    assert_eq!(replay.fence_input(), b"\x1b[200~[sealed replay]\x1b[201~\r");
}
