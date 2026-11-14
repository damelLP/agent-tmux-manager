//! Mandatory shared harness contract tests. No model CLIs are invoked.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/role.rs"]
#[allow(dead_code)]
mod bounded;

#[path = "support/stand_in.rs"]
mod stand_in;

use atm_core::SessionStatus;
use stand_in::{StandInConfig, StandInMode, StandInRig};

#[test]
fn stand_in_forwarder_identity() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex] {
        let pane = rig.launch(&StandInConfig::new(mode));
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", pane.pid)).unwrap();
        assert_eq!(comm.trim(), mode.name());
        rig.discover();
        if mode == StandInMode::Codex {
            let pending = rig.wait_for_row(&pane, |_| true);
            assert_eq!(pending.id.as_str(), format!("pending-{}", pane.pid));
            rig.paste(&pane, "identity prompt");
        }
        let row = rig.wait_for_row(&pane, |r| !r.id.is_pending());
        assert_eq!(row.tmux_pane.as_deref(), Some(pane.pane_id.as_str()));
        rig.discover();
        assert_eq!(
            rig.pane_rows(&pane).len(),
            1,
            "hook and discovery must share a PID"
        );
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_absent(&row.id);
        rig.wait_for_shell(&pane);
    }
    rig.close();
}

#[test]
fn stand_in_private_server_isolation() {
    let mut first = StandInRig::start();
    let mut second = StandInRig::start();
    let a = first.launch(&StandInConfig::new(StandInMode::Claude));
    let b = second.launch(&StandInConfig::new(StandInMode::Claude));
    assert_eq!(a.pane_id, b.pane_id);
    assert_ne!(first.socket(), second.socket());
    first.wait_for_row(&a, |r| r.status == SessionStatus::Idle);
    second.wait_for_row(&b, |r| r.status == SessionStatus::Idle);
    first.paste(&a, "only first");
    first.wait_for_row(&a, |r| r.status == SessionStatus::Working);
    assert_eq!(second.pane_rows(&b)[0].status, SessionStatus::Idle);
    first.close();
    assert!(second
        .tmux(&["display-message", "-p", "#{pid}"])
        .status
        .success());
    second.close();
}

#[test]
fn stand_in_missing_prerequisite_is_a_failure() {
    let failure = std::panic::catch_unwind(|| stand_in::require_tools(std::ffi::OsStr::new("")));
    let message = failure.expect_err("missing tools must never skip successfully");
    let message = message.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("required stand-in prerequisite: python3"),
        "{message}"
    );
}

#[test]
fn stand_in_rejects_unsealed_socket_before_starting() {
    let mut rig = StandInRig::start();
    for missing in [false, true] {
        let mut cmd = rig.command(&rig.launcher(StandInMode::Claude));
        cmd.env("ATM_STANDIN_ROOT", rig.root().join("tmp"));
        if missing {
            cmd.env_remove("ATM_SOCKET");
        }
        let output = rig.run(&mut cmd);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ATM_SOCKET must be"),
            "{output:?}"
        );
    }
    rig.close();
}

#[test]
fn stand_in_model_contract() {
    let output = bounded::run(
        std::process::Command::new("python3")
            .args(["-B", "-m", "unittest", "-v", "test_model"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .current_dir(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/stand_in"
            )),
    );
    assert!(output.status.success(), "{output:?}");
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
}
