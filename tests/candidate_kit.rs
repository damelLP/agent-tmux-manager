//! Candidate kit uses prebuilt binaries: never invokes cargo recursively.
#![cfg(target_os = "linux")]

#[test]
fn candidate_kit_contract() -> std::io::Result<()> {
    // Outer hang guard: allow three 120 s inner guards (including cleanup)
    // plus normal suite work, so a failure can report and stop owned resources.
    run_contract("420", "scripts/candidate_kit_test.py")
}

#[test]
fn promotion_kit_contract() -> std::io::Result<()> {
    run_contract("240", "scripts/promotion_kit_test.py")
}

fn run_contract(limit: &str, script: &str) -> std::io::Result<()> {
    let output = std::process::Command::new("timeout")
        .args([limit, "python3", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("CANDIDATE_TEST_ATM", env!("CARGO_BIN_EXE_atm"))
        .env("CANDIDATE_TEST_ATMD", env!("CARGO_BIN_EXE_atmd"))
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
