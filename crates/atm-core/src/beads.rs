//! Beads task lookup for preview pane integration.
//!
//! Scans `.beads/issues.jsonl` in a working directory for in-progress tasks.

use std::path::Path;

/// A beads task summary for display in the preview pane.
#[derive(Debug, Clone)]
pub struct BeadsTask {
    /// Issue ID (e.g., "agent-tmux-manager-6n0")
    pub id: String,
    /// Issue title
    pub title: String,
    /// Issue description
    pub description: Option<String>,
}

/// Finds in-progress beads tasks in the given working directory.
///
/// Scans `{working_dir}/.beads/issues.jsonl` for entries with `"status":"in_progress"`.
/// Returns tasks sorted by most recently updated first.
pub fn find_in_progress_tasks(working_dir: &str) -> Vec<BeadsTask> {
    let jsonl_path = Path::new(working_dir).join(".beads/issues.jsonl");
    match std::fs::read_to_string(&jsonl_path) {
        Ok(content) => parse_in_progress_tasks(&content),
        Err(_) => Vec::new(),
    }
}

/// Parses `issues.jsonl` content into its in-progress tasks, most recently
/// updated first. Malformed lines and entries without an ID or title are skipped.
fn parse_in_progress_tasks(content: &str) -> Vec<BeadsTask> {
    let mut tasks: Vec<(String, BeadsTask)> = Vec::new(); // (updated_at, task) for sorting

    for line in content.lines() {
        // Quick pre-filter before parsing JSON
        if !line.contains("\"in_progress\"") {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let status = val
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if status != "in_progress" {
                continue;
            }
            let id = val
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let title = val
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let updated_at = val
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let description = val
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            if !id.is_empty() && !title.is_empty() {
                tasks.push((
                    updated_at,
                    BeadsTask {
                        id,
                        title,
                        description,
                    },
                ));
            }
        }
    }

    // Most recently updated first
    tasks.sort_by(|a, b| b.0.cmp(&a.0));
    tasks.into_iter().map(|(_, task)| task).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_in_progress_tasks_empty() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_in_progress_tasks(dir.path().to_str().unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_in_progress_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("issues.jsonl"),
            r#"{"id":"test-1","title":"Open task","status":"open","updated_at":"2026-01-01T00:00:00Z"}
{"id":"test-2","title":"Active task","status":"in_progress","updated_at":"2026-01-02T00:00:00Z"}
{"id":"test-3","title":"Done task","status":"closed","updated_at":"2026-01-03T00:00:00Z"}
{"id":"test-4","title":"Another active","status":"in_progress","updated_at":"2026-01-04T00:00:00Z"}
"#,
        )
        .unwrap();

        let result = find_in_progress_tasks(dir.path().to_str().unwrap());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Another active"); // most recent first
        assert_eq!(result[1].title, "Active task");
    }

    fn ids(tasks: &[BeadsTask]) -> Vec<&str> {
        tasks.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn test_parse_keeps_only_in_progress_newest_first() {
        let content = r#"{"id":"a","title":"Old active","status":"in_progress","updated_at":"2026-01-01T00:00:00Z","description":"First line\nSecond line"}
{"id":"b","title":"Open","status":"open","updated_at":"2026-01-05T00:00:00Z"}
{"id":"c","title":"Closed but says in_progress","status":"closed","notes":"was in_progress","updated_at":"2026-01-06T00:00:00Z"}
{"id":"d","title":"New active","status":"in_progress","updated_at":"2026-01-03T00:00:00Z","description":""}
"#;

        let tasks = parse_in_progress_tasks(content);

        assert_eq!(ids(&tasks), ["d", "a"]);
        assert_eq!(tasks[0].title, "New active");
        assert_eq!(tasks[0].description, None); // empty description dropped
        assert_eq!(
            tasks[1].description.as_deref(),
            Some("First line\nSecond line")
        );
    }

    #[test]
    fn test_parse_skips_malformed_and_incomplete_lines() {
        let content = r#"{"id":"a","title":"Truncated","status":"in_progress"
not json "in_progress"
{"id":"","title":"No id","status":"in_progress"}
{"id":"b","title":"","status":"in_progress"}
{"title":"Missing id","status":"in_progress"}
{"id":"c","title":"Status not a string","status":["in_progress"]}
{"id":"d","title":"Good","status":"in_progress","updated_at":"2026-01-01T00:00:00Z"}
"#;

        assert_eq!(ids(&parse_in_progress_tasks(content)), ["d"]);
    }

    #[test]
    fn test_parse_empty_content() {
        assert!(parse_in_progress_tasks("").is_empty());
        assert!(parse_in_progress_tasks("\n\n").is_empty());
    }

    #[test]
    fn test_parse_crlf_line_endings() {
        let content = "{\"id\":\"a\",\"title\":\"Windows\",\"status\":\"in_progress\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\r\n\
                       {\"id\":\"b\",\"title\":\"Newer\",\"status\":\"in_progress\",\"updated_at\":\"2026-01-02T00:00:00Z\"}\r\n";

        let tasks = parse_in_progress_tasks(content);

        assert_eq!(ids(&tasks), ["b", "a"]);
        assert_eq!(tasks[1].title, "Windows");
    }
}
