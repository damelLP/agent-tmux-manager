//! Legacy pane capture and prompt extraction, without CLI output.

use atm_core::SessionView;
use atm_tmux::TmuxClient;

use super::{resolve_pane_id, ControlError};

/// Resolve and capture a target, returning the selected lines without printing.
pub async fn peek(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
    tail: Option<usize>,
    prompt: bool,
) -> Result<Vec<String>, ControlError> {
    let pane_id = resolve_pane_id(sessions, target)?;
    let lines = client
        .capture_pane(&pane_id)
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to capture pane {pane_id}"),
            source,
        })?;

    let output: &[String] = if prompt {
        extract_prompt(&lines)
    } else if let Some(n) = tail {
        let start = lines.len().saturating_sub(n);
        lines.get(start..).unwrap_or_default()
    } else {
        &lines
    };

    Ok(output.to_vec())
}

/// Extracts the active prompt from captured pane content.
///
/// Scans backwards from the bottom looking for Claude Code prompt patterns:
/// - Footer line: "Enter to select" / "Esc to cancel" / "(Y)es/(N)o"
/// - Numbered options: "1. ...", "2. ..."
/// - Separator lines (box drawing: ─── or ═══)
/// - Question/title lines
/// - Title blocks (☐/□/■)
///
/// Falls back to the last 15 lines if no prompt pattern is detected.
fn extract_prompt(lines: &[String]) -> &[String] {
    if lines.is_empty() {
        return lines;
    }

    // Find the footer line (bottom of prompt)
    let footer_idx = lines.iter().rposition(|l| {
        let trimmed = l.trim();
        trimmed.contains("Enter to select")
            || trimmed.contains("Esc to cancel")
            || trimmed.contains("(Y)es")
            || trimmed.contains("(y/n)")
            || trimmed.contains("to navigate")
    });

    let end = footer_idx.map_or(lines.len(), |i| i + 1);

    // Scan backwards from footer to find the top of the prompt block.
    // Strategy: keep going as long as lines look like prompt content.
    // Stop when we hit a line that's clearly not part of the prompt
    // (e.g., regular output, the ● response marker, the ❯ input line).
    let mut start = end.saturating_sub(1);

    for (i, line) in lines.iter().take(end.saturating_sub(1)).enumerate().rev() {
        let trimmed = line.trim();

        // Part of prompt: numbered options, indented descriptions, cursor
        if trimmed.starts_with("1.")
            || trimmed.starts_with("2.")
            || trimmed.starts_with("3.")
            || trimmed.starts_with("4.")
            || trimmed.starts_with("5.")
            || trimmed.starts_with("6.")
            || trimmed.starts_with("7.")
            || trimmed.starts_with("8.")
            || trimmed.starts_with("9.")
        {
            start = i;
            continue;
        }

        // Cursor indicator on option
        if trimmed.starts_with("❯ ") && trimmed.len() > 2 {
            // "❯ 1. Yes" is a prompt option; bare "❯ " is the input line
            let after_cursor = trimmed.get(4..).unwrap_or(""); // skip "❯ "
            if after_cursor.starts_with("1.")
                || after_cursor.starts_with("2.")
                || after_cursor.starts_with("3.")
                || after_cursor.starts_with("4.")
                || after_cursor.starts_with("5.")
            {
                start = i;
                continue;
            }
        }

        // Indented description line (part of an option)
        if line.starts_with("    ") && !trimmed.is_empty() {
            start = i;
            continue;
        }

        // Separator line (─── or ═══ etc.)
        if is_separator_line(trimmed) {
            start = i;
            continue;
        }

        // Empty line (spacing within prompt)
        if trimmed.is_empty() {
            start = i;
            continue;
        }

        // Question line (contains ?)
        if trimmed.contains('?') {
            start = i;
            continue;
        }

        // Title block (☐/□/■)
        if trimmed.starts_with('☐') || trimmed.starts_with('□') || trimmed.starts_with('■') {
            start = i;
            break; // Title is the top of the prompt
        }

        // If we've already found prompt content (start < end-1),
        // a non-matching line means we've gone past the prompt
        if start < end.saturating_sub(1) {
            break;
        }
    }

    // If we didn't find any prompt markers, fall back to last 15 lines
    if start == end.saturating_sub(1) && footer_idx.is_none() {
        let fallback_start = lines.len().saturating_sub(15);
        return lines.get(fallback_start..).unwrap_or_default();
    }

    // Trim leading blank/separator lines from the extracted prompt
    while start < end {
        let trimmed = lines.get(start).map(|l| l.trim()).unwrap_or("");
        if trimmed.is_empty() || is_separator_line(trimmed) {
            start += 1;
        } else {
            break;
        }
    }

    lines.get(start..end).unwrap_or_default()
}

/// Returns true if a line is a box-drawing separator (────, ════, etc.)
fn is_separator_line(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first_char = trimmed.chars().next().unwrap_or(' ');
    matches!(first_char, '─' | '━' | '═' | '┄' | '┈')
        && trimmed
            .chars()
            .all(|c| matches!(c, '─' | '━' | '═' | '┄' | '┈' | ' '))
}

#[cfg(test)]
mod prompt_tests {
    use super::extract_prompt;

    #[test]
    fn test_extract_numbered_prompt() {
        let lines: Vec<String> = vec![
            "Some previous output",
            "",
            "Do you like pineapple on pizza?",
            "",
            "❯ 1. Yes",
            "    Pineapple belongs on pizza",
            "  2. No",
            "    Pineapple does not belong on pizza",
            "  3. Type something.",
            "",
            "  4. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let prompt = extract_prompt(&lines);
        let text = prompt
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("pineapple on pizza"),
            "should contain the question"
        );
        assert!(text.contains("1. Yes"), "should contain option 1");
        assert!(text.contains("4. Chat"), "should contain option 4");
        assert!(text.contains("Enter to select"), "should contain footer");
        assert!(
            !text.contains("previous output"),
            "should not contain earlier output"
        );
    }

    #[test]
    fn test_extract_yn_prompt() {
        let lines: Vec<String> = vec![
            "lots of code output here",
            "more code",
            "",
            "Allow Bash: git status? (Y)es/(N)o",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let prompt = extract_prompt(&lines);
        assert!(prompt.iter().any(|l| l.contains("(Y)es")));
    }

    #[test]
    fn test_fallback_when_no_prompt() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();

        let prompt = extract_prompt(&lines);
        assert_eq!(prompt.len(), 15);
    }

    #[test]
    fn test_empty_input() {
        let lines: Vec<String> = vec![];
        let prompt = extract_prompt(&lines);
        assert!(prompt.is_empty());
    }
}
