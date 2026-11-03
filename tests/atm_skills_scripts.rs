//! Runs the atm skills' script tests, `crates/atm/assets/atm-skills/tests/scripts.sh`:
//! `atm-wait` against a stand-in `atm` and `atm-profile`'s lookup. No daemon
//! and no tmux are involved. The test skips cleanly (printed message, returns)
//! when `bash` or `jq` is not on PATH, as `e2e_with_tmux.rs` does for tmux.

use std::process::Command;

#[test]
fn atm_skills_scripts() {
    for tool in ["bash", "jq"] {
        if Command::new(tool).arg("--version").output().is_err() {
            eprintln!("SKIP: {tool} not on PATH");
            return;
        }
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/atm/assets/atm-skills/tests/scripts.sh"
    );
    let out = Command::new("bash")
        .arg(script)
        .output()
        .expect("run crates/atm/assets/atm-skills/tests/scripts.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    assert!(
        out.status.success(),
        "script tests failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
