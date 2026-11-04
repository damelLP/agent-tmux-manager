//! The atm skills (`atm-coordinate`, `atm-implement`): embedded at compile
//! time from `assets/atm-skills/` and written as plain skill folders into a
//! harness's own skills folder. Setup edits no settings for them: Claude
//! Code 2.1.316, pi 0.73.1 and codex-cli 0.163.0 each load a skill folder
//! from their user skills folder as it is (checked sealed, 2026-11-03).
//!
//! Each skill folder gets its `SKILL.md` and a copy of every file in
//! `assets/atm-skills/shared/`, at the same path, so a skill names its
//! scripts and reference relative to its own folder.
//! `tests/setup_atm_skills.rs` checks the installed files against the
//! source.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::hooks::{install_executable_script, write_file};
use super::{io_err, Result, SetupError};

/// Each skill's folder name and `SKILL.md`. The folder name must equal the
/// `name` in its frontmatter: Claude Code keys a skill on its folder, pi
/// and Codex on `name`.
const SKILLS: [(&str, &str); 2] = [
    (
        "atm-coordinate",
        include_str!("../../assets/atm-skills/atm-coordinate/SKILL.md"),
    ),
    (
        "atm-implement",
        include_str!("../../assets/atm-skills/atm-implement/SKILL.md"),
    ),
];

/// `assets/atm-skills/shared/`: each file's path inside a skill folder and
/// its content. Files under `scripts/` are installed executable.
const SHARED: [(&str, &str); 4] = [
    (
        "references/atm-commands.md",
        include_str!("../../assets/atm-skills/shared/references/atm-commands.md"),
    ),
    (
        "scripts/atm-wait",
        include_str!("../../assets/atm-skills/shared/scripts/atm-wait"),
    ),
    (
        "scripts/atm-profile",
        include_str!("../../assets/atm-skills/shared/scripts/atm-profile"),
    ),
    (
        // Codex ignores `disable-model-invocation`; this keeps the skill
        // out of its model's list. Claude Code and pi ignore the file.
        "agents/openai.yaml",
        include_str!("../../assets/atm-skills/shared/agents/openai.yaml"),
    ),
];

/// Removes whatever is at `path`: a folder with everything in it, or a
/// file or symlink in its place (a symlink's target is left alone).
/// Returns whether anything was there.
fn remove_entry(path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err("read", path)(e)),
    };
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(io_err("remove", path))?;
    Ok(true)
}

/// Writes the atm skills into `skills_dir`. Each skill's folder is
/// replaced whole, so files an older atm installed go too.
fn install_atm_skills(skills_dir: &Path) -> Result<()> {
    for (name, skill_md) in SKILLS {
        let dir = skills_dir.join(name);
        remove_entry(&dir)?;
        write_file(&dir.join("SKILL.md"), skill_md)?;
        for (path, content) in SHARED {
            let file = dir.join(path);
            if path.starts_with("scripts/") {
                install_executable_script(&file, content)?;
            } else {
                write_file(&file, content)?;
            }
        }
    }
    Ok(())
}

/// Setup's step for one harness: installs the atm skills into its skills
/// folder, `skills_dir`. The harness's hooks are in place by then, so a
/// failure here (a read-only skills folder, say) is reported and setup
/// goes on.
pub(super) fn setup_atm_skills(skills_dir: Option<PathBuf>) -> Result<()> {
    let dir = skills_dir.ok_or(SetupError::HomeDirUnknown)?;
    print!("  Installing atm skills to {}... ", dir.display());
    match install_atm_skills(&dir) {
        Ok(()) => println!("done"),
        Err(e) => println!("failed: {e}"),
    }
    Ok(())
}

/// Uninstall's step for one harness: removes atm's skill folders from
/// `skills_dir`, leaving the folder and every other skill in it. A folder
/// it can't remove is reported, and uninstall goes on.
pub(super) fn uninstall_atm_skills(skills_dir: Option<PathBuf>) -> Result<()> {
    let dir = skills_dir.ok_or(SetupError::HomeDirUnknown)?;
    print!("\nRemoving atm skills from {}... ", dir.display());
    let mut removed = false;
    let mut failed = None;
    for (name, _) in SKILLS {
        match remove_entry(&dir.join(name)) {
            Ok(was_there) => removed |= was_there,
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    match failed {
        Some(e) => println!("failed: {e}"),
        None => println!("{}", if removed { "done" } else { "not found" }),
    }
    Ok(())
}
