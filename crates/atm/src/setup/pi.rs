//! pi: the embedded `pi-atm` extension and pi's `settings.json`.

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::json::{or_empty_if_not_object, read_json_file_or_empty, write_json_file_pretty};
use super::{io_err, Result, SetupError};

/// The pi-atm TypeScript extension content, embedded at compile time.
/// pi loads `.ts` files directly via `@mariozechner/jiti`.
/// `assets/pi-atm/` mirrors `extensions/pi-atm/`; `tests/setup_pi_package.rs` enforces it.
const PI_ATM_EXTENSION: &str = include_str!("../../assets/pi-atm/extensions/pi-atm.ts");

/// Package manifest written next to the embedded extension. Pi looks
/// at the `pi.extensions` array (not `main`) to discover extension
/// files within an installed package.
const PI_ATM_PACKAGE_JSON: &str = include_str!("../../assets/pi-atm/package.json");

/// True if pi appears to be installed for this user.
///
/// pi creates `~/.pi/agent/` on first run. Checking the directory
/// avoids depending on a particular install location for the binary
/// (npm global / nvm version / etc).
pub(super) fn detect_pi() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".pi/agent").exists())
        .unwrap_or(false)
}

/// Path under which we install the embedded `pi-atm` extension.
///
/// Pi resolves the `"packages/<name>"` settings entry relative to its
/// `agentDir` (`~/.pi/agent/`, not `~/.pi/`) — verified against
/// `package-manager.js`'s `globalBaseDir = this.agentDir` and
/// `agentDir = ~/.pi/agent`. So the install path is
/// `~/.pi/agent/packages/<name>/`.
fn pi_atm_package_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".pi/agent/packages/pi-atm"))
}

/// Path to pi's settings file.
fn pi_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".pi").join("agent").join("settings.json"))
}

/// Reads pi's settings.json. Returns an empty object if not present.
fn read_pi_settings() -> Result<Map<String, Value>> {
    let path = pi_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

fn write_pi_settings(settings: &Map<String, Value>) -> Result<()> {
    let path = pi_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    write_json_file_pretty(&path, settings)
}

/// Writes the embedded pi-atm extension files to
/// `~/.pi/agent/packages/pi-atm/` (overwriting if present), then
/// ensures `"packages/pi-atm"` is in pi's settings `packages` array.
///
/// Returns `(files_written, settings_changed)` for caller's success
/// message.
fn install_pi_extension() -> Result<(bool, bool)> {
    let pkg_dir = pi_atm_package_dir().ok_or(SetupError::HomeDirUnknown)?;
    let extensions_dir = pkg_dir.join("extensions");
    fs::create_dir_all(&extensions_dir).map_err(io_err("create", &extensions_dir))?;

    // Layout matches pi-amplike: package.json with pi.extensions
    // pointing at ./extensions/, and the .ts file inside.
    let ts_path = extensions_dir.join("pi-atm.ts");
    let pkg_path = pkg_dir.join("package.json");

    // Always write to refresh any in-place edits the user might have made.
    fs::write(&ts_path, PI_ATM_EXTENSION).map_err(io_err("write", &ts_path))?;
    fs::write(&pkg_path, PI_ATM_PACKAGE_JSON).map_err(io_err("write", &pkg_path))?;

    // Update pi's settings.json packages array.
    let mut settings = read_pi_settings()?;
    let packages = settings
        .entry("packages")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(SetupError::Shape(
            "packages is not an array in pi settings.json",
        ))?;

    // Pi's local-package format: "packages/<name>" (relative to
    // pi's `agentDir`, which is `~/.pi/agent/`).
    let entry = Value::String("packages/pi-atm".to_string());
    let already_present = packages.iter().any(|v| v == &entry);
    if !already_present {
        packages.push(entry);
        write_pi_settings(&settings)?;
        Ok((true, true))
    } else {
        Ok((true, false))
    }
}

/// Removes the pi-atm extension from `~/.pi/agent/packages/pi-atm/`
/// and from pi's settings.json.
pub(super) fn uninstall_pi_extension() -> Result<bool> {
    let mut changed = false;
    if let Some(pkg_dir) = pi_atm_package_dir() {
        if pkg_dir.exists() {
            fs::remove_dir_all(&pkg_dir).map_err(io_err("remove", &pkg_dir))?;
            changed = true;
        }
    }
    let mut settings = or_empty_if_not_object(read_pi_settings())?;
    if let Some(packages) = settings.get_mut("packages").and_then(|p| p.as_array_mut()) {
        let before = packages.len();
        packages.retain(|v| v.as_str() != Some("packages/pi-atm"));
        if packages.len() < before {
            write_pi_settings(&settings)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Installs the `pi-atm` extension into `~/.pi/agent/packages/pi-atm/`
/// and registers it in pi's settings.json `packages` array.
pub(super) fn setup_pi() -> Result<()> {
    println!("\nConfiguring pi...");
    let (files_written, settings_changed) = install_pi_extension()?;
    if files_written {
        let pkg_dir = pi_atm_package_dir().unwrap_or_default();
        println!("    pi-atm.ts written to {}", pkg_dir.display());
    }
    if settings_changed {
        println!("    settings.json - added 'packages/pi-atm'");
    } else {
        println!("    settings.json - already references 'packages/pi-atm'");
    }
    println!("  pi configuration written.");
    Ok(())
}
