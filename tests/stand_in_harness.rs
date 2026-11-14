//! Mandatory shared harness contract tests. No model CLIs are invoked.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/role.rs"]
#[allow(dead_code)]
mod bounded;

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
}
