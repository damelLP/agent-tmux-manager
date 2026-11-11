//! Role CLI routing with a stand-in tmux. The relocated real atm has a
//! sibling atmd trap; a separate listener also detects readiness probes.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/daemon.rs"]
mod daemon;
#[path = "support/role.rs"]
mod support;

#[test]
fn role_session_targets_use_the_existing_prefix_resolution() {
    use atm_core::{SessionId, SessionView};
    let sessions = vec![
        SessionView {
            id: SessionId::new("lead-one"),
            id_short: "first".into(),
            tmux_pane: Some("%7".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("child-one"),
            parent_session_id: Some(SessionId::new("lead-one")),
            tmux_pane: Some("%7".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("lead-two"),
            tmux_pane: Some("%8".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("no-pane"),
            ..SessionView::default()
        },
    ];
    for (target, error) in [
        ("lead-one", None),
        ("first", None),
        ("child", None),
        ("missing", Some("No session matching")),
        ("lead", Some("Ambiguous target")),
        ("no-pane", Some("has no tmux pane")),
    ] {
        let dir = support::rig();
        fake_tmux(dir.path());
        daemon::fake_daemon(dir.path(), Some(sessions.clone()));
        let out = support::run(&mut support::command(
            dir.path(),
            &["role", "coordinator", "-t", target],
        ));
        assert!(!dir.path().join("daemon-started").exists());
        assert!(out.stdout.is_empty());
        if let Some(error) = error {
            assert!(!out.status.success(), "{out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(error),
                "{out:?}"
            );
            assert!(!dir.path().join("tmux.log").exists());
        } else {
            assert!(out.status.success(), "{out:?}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("tmux.log")).unwrap(),
                "-u set-option -p -t %7 @atm-role coordinator\n"
            );
        }
    }
}

#[test]
fn role_without_target_never_resolves_the_environment_through_atmd() {
    let dir = support::rig();
    fake_tmux(dir.path());
    let listener = support::connection_trap(dir.path());
    let out = support::run(
        support::command(dir.path(), &["role", "coordinator"]).env("TMUX_PANE", "stale-pane"),
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tmux.log")).unwrap(),
        "-u set-option -p -t stale-pane @atm-role coordinator\n"
    );
    support::assert_no_daemon(dir.path(), Some(&listener));
}

#[test]
fn cli_rig_collects_output_larger_than_a_pipe_buffer() {
    let out = support::run(std::process::Command::new("/bin/sh").args([
        "-c",
        "i=0; while [ $i -lt 8192 ]; do printf 0123456789abcdef; printf 0123456789abcdef >&2; i=$((i+1)); done",
    ]));
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(out.stdout.len(), 131_072);
    assert_eq!(out.stderr.len(), 131_072);
}

fn fake_tmux(dir: &std::path::Path) {
    support::executable(
        &dir.join("bin/tmux"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/tmux.log\"\ncase \" $* \" in *' %999 '*) echo 'no such pane: %999' >&2; exit 1;; esac\n",
    );
}

#[test]
fn role_clear_and_default_pane_are_daemon_free() {
    for listening in [false, true] {
        for (args, pane, expected) in [
            (
                vec!["role", "--clear", "-t", "%7"],
                None,
                "-u set-option -pu -t %7 @atm-role\n",
            ),
            (
                vec!["role", "coordinator"],
                Some("%8"),
                "-u set-option -p -t %8 @atm-role coordinator\n",
            ),
            (
                vec!["role", "--clear"],
                Some("%8"),
                "-u set-option -pu -t %8 @atm-role\n",
            ),
            (
                vec!["role", "coordinator", "-t", "%7"],
                Some("%8"),
                "-u set-option -p -t %7 @atm-role coordinator\n",
            ),
        ] {
            let dir = support::rig();
            fake_tmux(dir.path());
            let listener = listening.then(|| support::connection_trap(dir.path()));
            let mut cmd = support::command(dir.path(), &args);
            if let Some(pane) = pane {
                cmd.env("TMUX_PANE", pane);
            }
            let out = support::run(&mut cmd);
            assert!(out.status.success(), "{args:?}: {out:?}");
            assert!(out.stdout.is_empty(), "{out:?}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("tmux.log")).unwrap(),
                expected
            );
            support::assert_no_daemon(dir.path(), listener.as_ref());
        }
    }
}

#[test]
fn role_requires_its_own_pane_or_an_explicit_target() {
    for pane in [None, Some("")] {
        let dir = support::rig();
        fake_tmux(dir.path());
        let listener = support::connection_trap(dir.path());
        let mut cmd = support::command(dir.path(), &["role", "coordinator"]);
        if let Some(pane) = pane {
            cmd.env("TMUX_PANE", pane);
        }
        let out = support::run(&mut cmd);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("pass -t"),
            "{out:?}"
        );
        assert!(!dir.path().join("tmux.log").exists());
        support::assert_no_daemon(dir.path(), Some(&listener));
    }
}

#[test]
fn role_rejects_invalid_cli_combinations_before_io() {
    for args in [
        vec!["role"],
        vec!["role", "implementer"],
        vec!["role", "coordinator", "--clear"],
    ] {
        let dir = support::rig();
        fake_tmux(dir.path());
        let listener = support::connection_trap(dir.path());
        let out = support::run(&mut support::command(dir.path(), &args));
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(!dir.path().join("tmux.log").exists());
        support::assert_no_daemon(dir.path(), Some(&listener));
    }
}

#[test]
fn role_returns_tmuxs_missing_pane_error_without_daemon_io() {
    let dir = support::rig();
    fake_tmux(dir.path());
    let listener = support::connection_trap(dir.path());
    let out = support::run(&mut support::command(
        dir.path(),
        &["role", "coordinator", "-t", "%999"],
    ));
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no such pane: %999"),
        "{out:?}"
    );
    assert!(out.stdout.is_empty());
    support::assert_no_daemon(dir.path(), Some(&listener));
}

#[test]
fn role_sets_an_explicit_pane_without_a_daemon() {
    for listening in [false, true] {
        let dir = support::rig();
        support::executable(
            &dir.path().join("bin/tmux"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/tmux.log\"\n",
        );
        let listener = listening.then(|| support::connection_trap(dir.path()));
        let out = support::run(&mut support::command(
            dir.path(),
            &["role", "coordinator", "-t", "%7"],
        ));
        assert!(out.status.success(), "atm role: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tmux.log")).unwrap(),
            "-u set-option -p -t %7 @atm-role coordinator\n"
        );
        support::assert_no_daemon(dir.path(), listener.as_ref());
    }
}
