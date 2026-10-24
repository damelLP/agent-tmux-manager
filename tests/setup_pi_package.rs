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

/// Adds every file under `dir` to `files`, keyed by its path relative to
/// `root`.
fn read_tree(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_tree(root, &path, files);
        } else {
            let file = path.strip_prefix(root).unwrap().to_path_buf();
            files.insert(file, fs::read(&path).unwrap());
        }
    }
}

/// The source files pi loads: `package.json` and everything under
/// `extensions/`. README.md and LICENSE are for readers, not pi.
fn source_package() -> BTreeMap<PathBuf, Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(SOURCE);
    let mut files = BTreeMap::new();
    let manifest = PathBuf::from("package.json");
    files.insert(manifest.clone(), fs::read(root.join(&manifest)).unwrap());
    read_tree(&root, &root.join("extensions"), &mut files);
    files
}

/// Every file `atm setup` installed into pi's packages dir under `home`.
fn installed_package(home: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let root = home.join(INSTALLED);
    let mut files = BTreeMap::new();
    read_tree(&root, &root, &mut files);
    files
}

#[test]
fn setup_installs_the_pi_package_exactly_as_its_source() {
    let home = tempfile::tempdir().expect("tempdir");
    // Setup configures pi only when pi's agent dir exists.
    fs::create_dir_all(home.path().join(".pi/agent")).unwrap();

    let output = run_atm(home.path(), "setup");
    assert!(output.status.success(), "atm setup: {output:?}");

    let source = source_package();
    let installed = installed_package(home.path());
    let mut drift = Vec::new();
    for (file, content) in &source {
        let name = file.display();
        match installed.get(file) {
            Some(copy) if copy == content => {}
            Some(_) => drift.push(format!(
                "{INSTALLED}/{name} differs from {SOURCE}/{name}; sync it with \
                 `cp {SOURCE}/{name} {MIRROR}/{name}`"
            )),
            None => drift.push(format!(
                "{SOURCE}/{name} is not installed; copy it with \
                 `cp {SOURCE}/{name} {MIRROR}/{name}` and embed it in \
                 crates/atm/src/setup.rs"
            )),
        }
    }
    for file in installed.keys().filter(|file| !source.contains_key(*file)) {
        let name = file.display();
        drift.push(format!(
            "{INSTALLED}/{name} has no source; add it as {SOURCE}/{name} \
             or stop installing it in crates/atm/src/setup.rs"
        ));
    }
    assert!(
        drift.is_empty(),
        "the installed pi-atm package has drifted from {SOURCE}:\n{}",
        drift.join("\n")
    );
}
