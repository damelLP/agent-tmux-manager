//! Candidate kit uses prebuilt binaries: never invokes cargo recursively.
#![cfg(target_os = "linux")]

#[test]
fn candidate_kit_contract() {
    run_contract("scripts/candidate_kit_test.py");
}

#[test]
fn promotion_kit_contract() {
    run_contract("scripts/promotion_kit_test.py");
}

fn run_contract(script: &str) {
    let output = std::process::Command::new("timeout")
        .args(["240", "python3", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("CANDIDATE_TEST_ATM", env!("CARGO_BIN_EXE_atm"))
        .env("CANDIDATE_TEST_ATMD", env!("CARGO_BIN_EXE_atmd"))
        .output()
        .expect("run candidate kit contract tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
