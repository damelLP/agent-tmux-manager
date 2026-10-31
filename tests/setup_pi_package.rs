//! `atm setup` installing the pi-atm package exactly as its authoritative
//! source, `extensions/pi-atm/`. Setup embeds a mirror of it from
//! `crates/atm/assets/pi-atm/`, because cargo only packages files inside
//! the crate, so this catches the two copies drifting apart.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// The authoritative pi-atm package, relative to the repo root.
const SOURCE: &str = "extensions/pi-atm";
/// The copy `atm setup` embeds, relative to the repo root.
const MIRROR: &str = "crates/atm/assets/pi-atm";
/// Where `atm setup` installs the package, relative to HOME.
const INSTALLED: &str = ".pi/agent/packages/pi-atm";

/// Runs `atm <cmd>` with HOME as the isolated home directory.
fn run_atm(home: &Path, cmd: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atm"))
        .arg(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env_remove("TMUX")
        .output()
        .expect("run atm")
}

/// Adds the file at `path`, or every file under it, to `files`, keyed by
/// its path relative to `root`. Like pi loading an extension dir, it skips
/// names starting with `.` (vim swap files, Emacs lock links); it also
/// skips `~` backups. Symlinks are read, never walked into.
fn add_files(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let is_dir = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
    if !is_dir {
        let content = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        files.insert(path.strip_prefix(root).unwrap().to_path_buf(), content);
        return;
    }
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && !name.ends_with('~') {
            add_files(root, &entry.path(), files);
        }
    }
}

/// The source files pi loads: `package.json` and every path its
/// `pi.extensions` lists. README.md and LICENSE are for readers, not pi.
fn source_package() -> BTreeMap<PathBuf, Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(SOURCE);
    let manifest = root.join("package.json");
    let mut files = BTreeMap::new();
    add_files(&root, &manifest, &mut files);
    let parsed: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let entries = parsed["pi"]["extensions"]
        .as_array()
        .expect("package.json lists pi.extensions");
    for entry in entries {
        let entry = entry.as_str().expect("pi.extensions entries are paths");
        add_files(&root, &root.join(entry), &mut files);
    }
    files
}

#[test]
fn setup_installs_the_pi_package_exactly_as_its_source() {
    let home = tempfile::tempdir().expect("tempdir");
    // Setup configures pi only when pi's agent dir exists.
    fs::create_dir_all(home.path().join(".pi/agent")).unwrap();

    let output = run_atm(home.path(), "setup");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "atm setup: {output:?}");
    let installed_root = home.path().join(INSTALLED);
    assert!(
        installed_root.is_dir(),
        "atm setup did not install {INSTALLED}:\n{stdout}"
    );

    let source = source_package();
    let mut installed = BTreeMap::new();
    add_files(&installed_root, &installed_root, &mut installed);
    let mut drift = Vec::new();
    for (file, content) in &source {
        let name = file.display();
        match installed.get(file) {
            Some(copy) if copy == content => {}
            Some(_) => drift.push(format!(
                "{INSTALLED}/{name} differs from {SOURCE}/{name}, which wins. \
                 Compare with `diff {SOURCE}/{name} {MIRROR}/{name}`, then sync \
                 with `cp {SOURCE}/{name} {MIRROR}/{name}`; if they already \
                 match, crates/atm/src/setup/pi.rs installs the wrong bytes"
            )),
            None => drift.push(format!(
                "{SOURCE}/{name} is not installed; copy it with \
                 `cp {SOURCE}/{name} {MIRROR}/{name}` and embed it in \
                 crates/atm/src/setup/pi.rs"
            )),
        }
    }
    for file in installed.keys().filter(|file| !source.contains_key(*file)) {
        let name = file.display();
        drift.push(format!(
            "{INSTALLED}/{name} has no source; add it as {SOURCE}/{name} \
             or stop installing it in crates/atm/src/setup/pi.rs"
        ));
    }
    assert!(
        drift.is_empty(),
        "the installed pi-atm package has drifted from {SOURCE}:\n{}",
        drift.join("\n")
    );
}
