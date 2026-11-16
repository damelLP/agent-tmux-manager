//! `atm setup` installing the atm skills from `crates/atm/assets/atm-skills/`
//! into each detected harness's own skills folder, and `atm uninstall`
//! removing only atm's four folders. Each installed skill folder holds its
//! `SKILL.md` and a copy of everything in `shared/`.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// The skills' source, relative to the repo root.
const SOURCE: &str = "crates/atm/assets/atm-skills";
/// The skills `atm setup` installs, by folder name.
const SKILLS: [&str; 4] = ["atm-coordinate", "atm-implement", "atm-lead", "atm-member"];
/// Each harness's directory, whose presence makes setup configure it, and
/// its skills folder, relative to HOME.
const HARNESSES: [(&str, &str); 3] = [
    (".claude", ".claude/skills"),
    (".pi/agent", ".pi/agent/skills"),
    (".codex", ".codex/skills"),
];

fn source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(SOURCE)
}

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

/// Runs `atm <cmd>`, asserting success, and returns its stdout.
fn run_atm_ok(home: &Path, cmd: &str) -> String {
    let output = run_atm(home, cmd);
    assert!(output.status.success(), "atm {cmd}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A temp home holding the given harness directories.
fn home_with(dirs: &[&str]) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    for dir in dirs {
        fs::create_dir_all(home.path().join(dir)).unwrap();
    }
    home
}

fn all_harnesses() -> tempfile::TempDir {
    home_with(&HARNESSES.map(|(dir, _)| dir))
}

/// Every file under `dir`, keyed by its path relative to `dir`. Symlinks
/// are listed as files, never walked into.
fn files_under(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), files);
            } else {
                let rel = entry.path().strip_prefix(root).unwrap().to_path_buf();
                files.insert(rel, fs::read(entry.path()).unwrap_or_default());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files);
    files
}

/// What an installed `skill` folder holds: its SKILL.md and `shared/`.
fn expected_skill(skill: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files_under(&source().join("shared"));
    let skill_md = fs::read(source().join(skill).join("SKILL.md")).unwrap();
    files.insert(PathBuf::from("SKILL.md"), skill_md);
    files
}

/// Asserts the skill folders in `skills` are exactly atm's four, as in the
/// source, with the scripts executable and nothing else.
fn assert_installed(skills: &Path) {
    for skill in SKILLS {
        let installed = skills.join(skill);
        assert_eq!(
            files_under(&installed),
            expected_skill(skill),
            "{} differs from {SOURCE}/{skill}/SKILL.md + {SOURCE}/shared/",
            installed.display()
        );
        // Scripts 0755 as in git; other files keep the umask's mode, so
        // only their execute bits are checked.
        for file in expected_skill(skill).keys() {
            let path = installed.join(file);
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            if file.starts_with("scripts") {
                assert_eq!(mode, 0o755, "{}", path.display());
            } else {
                assert_eq!(mode & 0o111, 0, "{} is executable", path.display());
            }
        }
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A skill of the user's own, and the built-in skills Codex writes into
/// `~/.codex/skills/.system/`: setup and uninstall leave both alone.
fn add_other_skills(skills: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    for (dir, body) in [("mine", "my skill"), (".system/imagegen", "codex's")] {
        fs::create_dir_all(skills.join(dir)).unwrap();
        fs::write(skills.join(dir).join("SKILL.md"), body).unwrap();
    }
    others(skills)
}

/// Every file in `skills` outside atm's four folders.
fn others(skills: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files_under(skills);
    files.retain(|path, _| !SKILLS.iter().any(|skill| path.starts_with(skill)));
    files
}

#[test]
fn setup_installs_all_skills_for_each_detected_harness() {
    let home = all_harnesses();
    let home = home.path();

    let stdout = run_atm_ok(home, "setup");

    for (_, skills) in HARNESSES {
        let skills = home.join(skills);
        let line = format!("  Installing atm skills to {}... done\n", skills.display());
        assert!(stdout.contains(&line), "no {line:?} in:\n{stdout}");
        assert_eq!(names_in(&skills), SKILLS, "{}", skills.display());
        assert_installed(&skills);
    }
}

/// The source holds only what setup installs (each skill's SKILL.md and
/// `shared/`), the script tests and the README. A file anywhere else
/// would never reach a harness.
#[test]
fn every_source_file_is_installed() {
    let stray: Vec<_> = files_under(&source())
        .into_keys()
        .filter(|file| {
            !(file == Path::new("README.md")
                || file.starts_with("tests")
                || file.starts_with("shared")
                || SKILLS
                    .iter()
                    .any(|skill| file == &Path::new(skill).join("SKILL.md")))
        })
        .collect();
    assert!(
        stray.is_empty(),
        "atm setup doesn't install {stray:?} from {SOURCE}: move it into \
         shared/, or add the skill to crates/atm/src/setup/skills.rs and SKILLS here"
    );
}

#[test]
fn setup_installs_skills_only_for_detected_harnesses() {
    let home = home_with(&[".codex"]);
    let home = home.path();

    run_atm_ok(home, "setup");

    assert_installed(&home.join(".codex/skills"));
    for absent in [".claude", ".pi", ".agents"] {
        assert!(!home.join(absent).exists(), "setup created ~/{absent}");
    }
}

/// A second setup replaces atm's folders whole: an edited SKILL.md comes
/// back and a file an older atm installed goes. Other skills stay.
#[test]
fn setup_replaces_atms_copy_and_keeps_other_skills() {
    let home = all_harnesses();
    let home = home.path();
    run_atm_ok(home, "setup");
    let mut kept = Vec::new();
    for (_, skills) in HARNESSES {
        let skills = home.join(skills);
        for skill in SKILLS {
            fs::write(skills.join(skill).join("SKILL.md"), "edited").unwrap();
            fs::write(skills.join(skill).join("stale.md"), "old").unwrap();
        }
        kept.push(add_other_skills(&skills));
    }

    run_atm_ok(home, "setup");

    for ((_, skills), kept) in HARNESSES.iter().zip(kept) {
        let skills = home.join(skills);
        assert_installed(&skills);
        assert_eq!(others(&skills), kept, "{}", skills.display());
    }
}

/// A developer may symlink atm's folder to a checkout: setup replaces the
/// link and never writes through it into the checkout.
#[test]
fn setup_replaces_a_symlinked_skill_folder_and_leaves_its_target() {
    for skill in SKILLS {
        let home = home_with(&[".claude"]);
        let home = home.path();
        let checkout = home.join("checkout").join(skill);
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("SKILL.md"), "work in progress").unwrap();
        let skills = home.join(".claude/skills");
        fs::create_dir_all(&skills).unwrap();
        symlink(&checkout, skills.join(skill)).unwrap();

        run_atm_ok(home, "setup");

        assert!(!fs::symlink_metadata(skills.join(skill))
            .unwrap()
            .is_symlink());
        assert_installed(&skills);
        assert_eq!(
            files_under(&checkout),
            BTreeMap::from([(PathBuf::from("SKILL.md"), b"work in progress".to_vec())])
        );
    }
}

#[test]
fn uninstall_removes_only_atms_skill_folders() {
    let home = all_harnesses();
    let home = home.path();
    run_atm_ok(home, "setup");
    let kept: Vec<_> = HARNESSES
        .iter()
        .map(|(_, skills)| add_other_skills(&home.join(skills)))
        .collect();

    let stdout = run_atm_ok(home, "uninstall");

    for ((_, skills), kept) in HARNESSES.iter().zip(kept) {
        let skills = home.join(skills);
        let line = format!("Removing atm skills from {}... done\n", skills.display());
        assert!(stdout.contains(&line), "no {line:?} in:\n{stdout}");
        assert_eq!(
            names_in(&skills),
            [".system", "mine"],
            "{}",
            skills.display()
        );
        assert_eq!(files_under(&skills), kept, "{}", skills.display());
    }
}

/// The skills need no settings: setup writes no skill or plugin entry, and
/// never creates Codex's config.toml.
#[test]
fn setup_writes_no_settings_for_the_skills() {
    let home = all_harnesses();
    let home = home.path();

    run_atm_ok(home, "setup");

    let read = |file: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(home.join(file)).unwrap()).unwrap()
    };
    let claude = read(".claude/settings.json");
    for key in ["skills", "enabledPlugins", "extraKnownMarketplaces"] {
        assert!(
            claude.get(key).is_none(),
            "settings.json has {key}: {claude}"
        );
    }
    let pi = read(".pi/agent/settings.json");
    assert!(pi.get("skills").is_none(), "pi settings.json: {pi}");
    assert!(!home.join(".codex/config.toml").exists());
}

/// The skills are an addition to each harness's hooks: a skills folder setup
/// can't write (here a file in its place, as a read-only or foreign
/// `~/.claude/skills` would be) is reported, and setup goes on to the other
/// harnesses, the tmux bindings and atm's config.
#[test]
fn a_failed_skills_install_is_reported_and_setup_goes_on() {
    let home = all_harnesses();
    let home = home.path();
    fs::write(home.join(".claude/skills"), "not a folder").unwrap();

    let stdout = run_atm_ok(home, "setup");

    let line = format!(
        "  Installing atm skills to {}... failed: ",
        home.join(".claude/skills").display()
    );
    assert_failed_with_cause(&stdout, &line);
    assert!(home.join(".claude/settings.json").exists(), "{stdout}");
    assert_installed(&home.join(".pi/agent/skills"));
    assert_installed(&home.join(".codex/skills"));
    assert!(home.join(".config/atm/config.toml").exists(), "{stdout}");
}

/// Uninstall reports a skills folder it can't clear and goes on to the
/// other harnesses.
#[test]
fn a_failed_skills_removal_is_reported_and_uninstall_goes_on() {
    let home = all_harnesses();
    let home = home.path();
    run_atm_ok(home, "setup");
    let claude_skills = home.join(".claude/skills");
    fs::remove_dir_all(&claude_skills).unwrap();
    fs::write(&claude_skills, "not a folder").unwrap();

    let stdout = run_atm_ok(home, "uninstall");

    let line = format!(
        "Removing atm skills from {}... failed: ",
        claude_skills.display()
    );
    assert_failed_with_cause(&stdout, &line);
    for skills in [".pi/agent/skills", ".codex/skills"] {
        assert!(
            names_in(&home.join(skills)).is_empty(),
            "{skills}: {stdout}"
        );
    }
    assert!(
        stdout.ends_with("ATM uninstalled successfully!\n"),
        "{stdout}"
    );
}

/// Asserts `stdout` has a line starting with `start` (a skills step's
/// "... failed: ") that also names the OS error under it, such as "Not a
/// directory (os error 20)", not just the path.
fn assert_failed_with_cause(stdout: &str, start: &str) {
    let line = stdout
        .lines()
        .find(|line| line.starts_with(start))
        .unwrap_or_else(|| panic!("no {start:?} in:\n{stdout}"));
    assert!(line.contains("(os error "), "no cause in {line:?}");
}

/// Gives every folder under `dir` its write permission back, so a test that
/// locked one can clean up.
fn unlock(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            unlock(&path);
        }
    }
}

/// A failed install leaves the old or new copy whole and still attempts the
/// other skills. Repeat for each position in the manifest. As root the
/// permission restriction does not fail, so the new copy is also allowed.
#[test]
fn a_failed_install_leaves_each_skill_whole() {
    for failing in SKILLS {
        let home = home_with(&[".claude"]);
        let home = home.path();
        run_atm_ok(home, "setup");
        let skills = home.join(".claude/skills");
        for skill in SKILLS {
            fs::write(skills.join(skill).join("SKILL.md"), format!("old {skill}")).unwrap();
        }
        let blocked = skills.join(failing);
        let old = files_under(&blocked);
        fs::set_permissions(
            blocked.join("references"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let stdout = run_atm_ok(home, "setup");
        unlock(home);

        let installed = files_under(&blocked);
        assert!(
            installed == expected_skill(failing) || installed == old,
            "{failing} is neither the old copy nor the new one: {installed:?}\n{stdout}"
        );
        let staging = skills.join(".atm-staging");
        for skill in SKILLS {
            if skill != failing {
                assert_eq!(
                    files_under(&skills.join(skill)),
                    expected_skill(skill),
                    "{stdout}"
                );
            }
            assert!(
                !staging.join(skill).exists(),
                "a new {skill} was left: {stdout}"
            );
        }

        run_atm_ok(home, "uninstall");
        assert!(names_in(&skills).is_empty(), "{:?}", names_in(&skills));
    }
}

// Break caught: adding named skills retires an old skill or overwrites an
// unrelated skill when upgrading an existing two-skill installation.
#[test]
fn setup_upgrades_a_legacy_two_skill_home_without_removing_the_old_pair() {
    let home = all_harnesses();
    let home = home.path();
    let mut kept = Vec::new();
    for (_, dir) in HARNESSES {
        let skills = home.join(dir);
        for old in ["atm-coordinate", "atm-implement"] {
            fs::create_dir_all(skills.join(old)).unwrap();
            fs::write(skills.join(old).join("SKILL.md"), "legacy copy").unwrap();
        }
        kept.push(add_other_skills(&skills));
    }

    run_atm_ok(home, "setup");

    for ((_, dir), kept) in HARNESSES.iter().zip(kept) {
        let skills = home.join(dir);
        for skill in SKILLS {
            assert!(
                skills.join(skill).join("SKILL.md").is_file(),
                "missing {skill}"
            );
        }
        assert_installed(&skills);
        assert_eq!(others(&skills), kept);
    }
}
