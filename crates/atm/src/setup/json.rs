//! JSON file reading and writing shared by the harness settings files.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::{io_err, json_err, Result, SetupError};

/// Reads the JSON object in the file at `path`, returning an empty object
/// if the file does not exist. Errors carry the path for diagnostics.
///
/// Valid JSON that isn't an object (`[]`, `null`, `1`) is a
/// [`SetupError::NotAnObject`], so callers never panic indexing it or
/// replace the user's file with `{}`.
pub(super) fn read_json_file_or_empty(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = fs::read_to_string(path).map_err(io_err("read", path))?;

    match serde_json::from_str(&content).map_err(json_err("parse", path))? {
        Value::Object(settings) => Ok(settings),
        _ => Err(SetupError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Uninstall's read of a harness settings file. One that isn't a JSON
/// object holds no ATM entries, so it reads as empty (and, with nothing
/// removed, is never written back) rather than stopping the uninstall.
pub(super) fn or_empty_if_not_object(
    read: Result<Map<String, Value>>,
) -> Result<Map<String, Value>> {
    match read {
        Err(SetupError::NotAnObject { .. }) => Ok(Map::new()),
        read => read,
    }
}

/// Writes `value` to `path` as pretty-printed JSON, creating parent
/// directories as needed. Errors carry the path for diagnostics.
pub(super) fn write_json_file_pretty(path: &Path, value: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create", parent))?;
    }

    let content = serde_json::to_string_pretty(value).map_err(json_err("serialize", path))?;
    fs::write(path, content).map_err(io_err("write", path))
}

#[cfg(test)]
mod settings_read_tests {
    use super::{json_err, or_empty_if_not_object, read_json_file_or_empty, SetupError};
    use serde_json::{json, Map, Value};
    use std::fs;
    use std::path::Path;

    #[test]
    fn non_object_roots_are_rejected_naming_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for root in ["[]", "null", "1", "\"x\""] {
            fs::write(&path, root).unwrap();

            let err = read_json_file_or_empty(&path).unwrap_err();

            assert!(
                matches!(err, SetupError::NotAnObject { path: ref p } if p == &path),
                "{root}: {err:?}"
            );
            assert_eq!(
                err.to_string(),
                format!("{} is not a JSON object", path.display())
            );
        }
    }

    #[test]
    fn an_object_root_is_read_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"userKey": "kept"}"#).unwrap();

        let settings = read_json_file_or_empty(&path).unwrap();

        assert_eq!(Value::Object(settings), json!({"userKey": "kept"}));
    }

    #[test]
    fn a_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();

        let settings = read_json_file_or_empty(&dir.path().join("absent.json")).unwrap();

        assert!(settings.is_empty());
    }

    #[test]
    fn uninstall_reads_a_non_object_file_as_empty() {
        let not_object = SetupError::NotAnObject {
            path: "/h/.claude/settings.json".into(),
        };

        assert_eq!(or_empty_if_not_object(Err(not_object)).unwrap(), Map::new());
    }

    #[test]
    fn uninstall_still_fails_on_other_read_errors() {
        let p = Path::new("/h/.claude/settings.json");
        let parse = serde_json::from_str::<Value>("{").unwrap_err();

        let read = or_empty_if_not_object(Err(json_err("parse", p)(parse)));

        assert!(
            matches!(read, Err(SetupError::Json { op: "parse", .. })),
            "{read:?}"
        );
    }

    #[test]
    fn uninstall_reads_an_object_file_unchanged() {
        let settings = json!({"userKey": "kept"}).as_object().cloned().unwrap();

        assert_eq!(
            or_empty_if_not_object(Ok(settings.clone())).unwrap(),
            settings
        );
    }
}
