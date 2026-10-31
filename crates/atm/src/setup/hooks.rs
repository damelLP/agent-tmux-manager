//! Hook plumbing Claude Code and Codex share: marker matching, the two loops and script files.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::{io_err, Result, SetupError};

/// Splits `cmd` into words roughly as a POSIX shell would, expanding
/// nothing: quotes group and are dropped, a backslash escapes the next
/// character (inside double quotes only `$`, `` ` ``, `"` and `\`; none
/// inside single quotes), backslash-newline joins lines, and an unquoted
/// space, tab, newline (POSIX's default IFS) or one of `;&|()<>` ends a
/// word. Other Unicode whitespace, such as NBSP, stays inside a word.
fn shell_words(cmd: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\\') => match chars.next() {
                Some('\n') => {}
                next => word.get_or_insert_default().extend(next),
            },
            (Some('"'), '\\') => {
                let word = word.get_or_insert_default();
                match chars.next() {
                    Some('\n') => {}
                    Some(c @ ('$' | '`' | '"' | '\\')) => word.push(c),
                    next => word.extend(std::iter::once('\\').chain(next)),
                }
            }
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, c) if " \t\n;&|()<>".contains(c) => words.extend(word.take()),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// True if `hook`'s command runs the script named `marker`: one of its
/// [`shell_words`] has `marker` as its file name. `/x/atm-hook`,
/// `"/x y/atm-hook"`, `ATM_DEBUG=1 /x/atm-hook` and `/x/atm-hook; :`
/// match; `/x/atm-hook-wrapper.sh` and `'/x/atm-hook wrapper.sh'` don't.
pub(super) fn is_marker_command(hook: &Value, marker: &str) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|cmd| {
            shell_words(cmd)
                .iter()
                .any(|word| Path::new(word).file_name().and_then(|n| n.to_str()) == Some(marker))
        })
}

/// Checks if a hook entry with a command that runs `marker` is present
/// in a hooks array. `marker` is the vendor script's basename
/// (`atm-hook`, `atm-codex-hook`); see [`is_marker_command`].
pub(super) fn has_hook_command_marker(hooks_array: &[Value], marker: &str) -> bool {
    hooks_array.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| hooks.iter().any(|hook| is_marker_command(hook, marker)))
            .unwrap_or(false)
    })
}

/// Removes the commands that run `marker` from each entry's inner
/// `hooks` array, keeping the entry's other commands and fields (e.g.
/// `matcher`). Drops an entry only when that leaves its `hooks` empty.
/// Returns how many commands were removed.
#[must_use]
pub(super) fn remove_hook_command_marker(hooks_array: &mut Vec<Value>, marker: &str) -> usize {
    let mut removed = 0;
    hooks_array.retain_mut(|entry| {
        let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| !is_marker_command(hook, marker));
        removed += before - hooks.len();
        hooks.len() == before || !hooks.is_empty()
    });
    removed
}

/// Setup's hook loop: for each of `hook_names`, adds `new_entry`'s entry to
/// that event's array in `settings`' `hooks` object unless a command there
/// already runs `marker`, printing the outcome. `not_object` is the error
/// for a `hooks` that isn't an object. Returns how many entries were added.
pub(super) fn add_hook_entries(
    settings: &mut Map<String, Value>,
    hook_names: impl Iterator<Item = &'static str>,
    marker: &str,
    not_object: &'static str,
    new_entry: impl Fn(&str) -> Value,
) -> Result<usize> {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(SetupError::Shape(not_object))?;

    let mut added = 0;

    for hook_type in hook_names {
        let hooks_array = hooks
            .entry(hook_type)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(SetupError::Shape("hook type is not an array"))?;

        if has_hook_command_marker(hooks_array, marker) {
            println!("    {hook_type} - already configured");
        } else {
            hooks_array.push(new_entry(hook_type));
            added += 1;
            println!("    {hook_type} - added");
        }
    }

    Ok(added)
}

/// Uninstall's hook loop: for each of `hook_names`, removes the commands
/// that run `marker` from that event's array in `settings`' `hooks` object,
/// printing the events that lost one, and drops the array if it's empty,
/// even if it already was. So `settings` can change when this returns 0.
/// Returns how many commands were removed; writing is up to the caller.
#[must_use]
pub(super) fn remove_hook_entries(
    settings: &mut Map<String, Value>,
    hook_names: impl Iterator<Item = &'static str>,
    marker: &str,
) -> usize {
    let mut removed = 0;
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for hook_type in hook_names {
            if let Some(hooks_array) = hooks.get_mut(hook_type).and_then(|h| h.as_array_mut()) {
                let count = remove_hook_command_marker(hooks_array, marker);

                if count > 0 {
                    removed += count;
                    println!("  {hook_type} - removed");
                }

                // Remove empty arrays
                if hooks_array.is_empty() {
                    hooks.remove(hook_type);
                }
            }
        }
    }
    removed
}

/// Writes `content` to `path` (creating parent directories as needed)
/// and marks it executable.
pub(super) fn install_executable_script(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create", parent))?;
    }

    fs::write(path, content).map_err(io_err("write", path))?;

    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(io_err("set permissions on", path))?;
    }

    Ok(())
}

/// Removes the script at `path`, reporting whether it existed.
pub(super) fn remove_script_file(path: &Path) -> Result<bool> {
    if path.exists() {
        fs::remove_file(path).map_err(io_err("remove", path))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod shared_entry_tests {
    use super::{
        has_hook_command_marker, is_marker_command, remove_hook_command_marker, shell_words,
    };
    use crate::setup::claude::{create_hook_entry, create_status_line_entry, has_atm_status_line};
    use crate::setup::codex::create_codex_hook_entry;
    use serde_json::{json, Value};

    const USER_HOOK: &str = "/usr/bin/my-own-hook";

    /// ATM's `entry` with the user's own command added to its inner
    /// `hooks` array, as a user editing settings.json might do.
    fn shared_with_user_hook(mut entry: Value) -> Value {
        entry["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": USER_HOOK}));
        entry
    }

    /// Removing `marker` from a shared entry must leave the entry, its
    /// other fields and the user's command, minus only ATM's command.
    fn assert_keeps_user_command(entry: Value, marker: &str) {
        let mut hooks_array = vec![shared_with_user_hook(entry.clone())];
        let mut expected = entry;
        expected["hooks"] = json!([{"type": "command", "command": USER_HOOK}]);

        let removed = remove_hook_command_marker(&mut hooks_array, marker);

        assert_eq!(removed, 1);
        assert_eq!(hooks_array, vec![expected]);
    }

    #[test]
    fn claude_uninstall_keeps_a_user_command_sharing_atms_entry() {
        assert_keeps_user_command(create_hook_entry("PreToolUse"), "atm-hook");
        assert_keeps_user_command(create_hook_entry("SessionStart"), "atm-hook");
    }

    #[test]
    fn codex_uninstall_keeps_a_user_command_sharing_atms_entry() {
        assert_keeps_user_command(create_codex_hook_entry(), "atm-codex-hook");
    }

    #[test]
    fn an_entry_left_empty_is_dropped_but_an_already_empty_one_is_kept() {
        let already_empty = json!({"matcher": "Bash", "hooks": []});
        let mut hooks_array = vec![create_hook_entry("PreToolUse"), already_empty.clone()];

        let removed = remove_hook_command_marker(&mut hooks_array, "atm-hook");

        assert_eq!(removed, 1);
        assert_eq!(hooks_array, vec![already_empty]);
    }

    #[test]
    fn a_script_whose_name_only_contains_the_marker_is_not_atms() {
        let wrapper = json!({
            "hooks": [{"type": "command", "command": "/home/u/bin/atm-hook-wrapper.sh"}]
        });
        let mut hooks_array = vec![wrapper.clone()];

        assert!(!has_hook_command_marker(&hooks_array, "atm-hook"));
        assert_eq!(remove_hook_command_marker(&mut hooks_array, "atm-hook"), 0);
        assert_eq!(hooks_array, vec![wrapper]);
    }

    #[test]
    fn a_command_running_the_script_with_env_args_or_quotes_is_atms() {
        for command in [
            "/home/u/.local/bin/atm-hook",
            "ATM_DEBUG=1 /home/u/.local/bin/atm-hook",
            "/home/u/.local/bin/atm-hook --verbose",
            "\"$HOME/.local/bin/atm-hook\"",
            "'/Users/John Doe/.local/bin/atm-hook'",
            "/Users/John\\ Doe/.local/bin/atm-hook",
            "/home/u/.local/bin/atm-hook; :",
            "/home/u/.local/bin/atm-hook&&true",
            "(/home/u/.local/bin/atm-hook)",
        ] {
            let command = json!({"type": "command", "command": command});
            assert!(is_marker_command(&command, "atm-hook"), "{command}");
        }
    }

    #[test]
    fn a_quoted_script_name_with_a_space_after_the_marker_is_not_atms() {
        for command in [
            "'/home/u/bin/atm-hook wrapper.sh'",
            "\"/home/u/bin/atm-hook wrapper.sh\" --flag",
            "/home/u/bin/atm-hook\\ wrapper.sh",
            "\"/home/u/bin/atm\\-hook\"",
            "/home/u/bin/atm-hook\u{a0}wrapper.sh",
        ] {
            let command = json!({"type": "command", "command": command});
            assert!(!is_marker_command(&command, "atm-hook"), "{command}");
        }
    }

    #[test]
    fn shell_words_groups_quotes_and_splits_on_operators() {
        assert_eq!(
            shell_words(r#"a 'b c' "d\"e" f\ g;h&&i|j (k) <l >m"#),
            ["a", "b c", "d\"e", "f g", "h", "i", "j", "k", "l", "m"]
        );
        assert_eq!(shell_words("x '' y"), ["x", "", "y"]);
        assert_eq!(shell_words("a\tb\nc\u{a0}d"), ["a", "b", "c\u{a0}d"]);
        assert_eq!(shell_words(r"'a\b'"), [r"a\b"]);
    }

    /// POSIX: inside double quotes a backslash escapes only `$`, `` ` ``,
    /// `"`, `\` and newline, and is kept before anything else. A
    /// backslash-newline is a line continuation, quoted or not.
    #[test]
    fn shell_words_keeps_double_quoted_backslashes_posix_keeps() {
        assert_eq!(
            shell_words(r#""a\-b" "c\$d" "e\\f" "g\`h""#),
            [r"a\-b", "c$d", r"e\f", "g`h"]
        );
        assert_eq!(shell_words("a\\\nb \"c\\\nd\""), ["ab", "cd"]);
    }

    #[test]
    fn a_status_line_script_whose_name_only_contains_the_marker_is_not_atms() {
        let wrapper = json!({"type": "command", "command": "/home/u/bin/atm-hook-wrapper.sh"});

        assert!(!has_atm_status_line(&wrapper));
        assert!(has_atm_status_line(&create_status_line_entry()));
    }
}
