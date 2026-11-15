//! Pure parsing of the owner's listed task acceptance measures.
//!
//! The single `How we will know` section lists `mechanical: <command>`,
//! `reviewed: .atm/measures/<rubric>`, or `owner` as Markdown list items.
//! Values may have matching inline backticks; their internal bytes stay exact.
//! ATX/Setext headings and unordered/numbered lists are supported; nested
//! headings stay in the section until a peer/ancestor heading ends it. Fenced
//! and indented code and HTML-commented lines are ignored, not approvals.
//! This is a small Markdown subset, not a general renderer. Present invalid
//! or empty sections fail closed; the I/O boundary decides missing-file policy.

use crate::TaskMeasure;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IntentError {
    #[error("intent file has no 'How we will know' section")]
    MissingSection,
    #[error("intent file repeats 'How we will know' at line {line}")]
    DuplicateSection { line: usize },
    #[error("intent file's 'How we will know' section lists no measures")]
    EmptySection,
    #[error("intent line {line}: expected owner, mechanical: <command>, or reviewed: .atm/measures/<rubric>")]
    InvalidMeasure { line: usize },
}

/// Parse the single measures section without reading files or executing commands.
/// Identical entries are deduplicated in their original order. Errors never
/// return a partially accepted list; the replay-only `Unknown` is never emitted.
pub fn parse_intent_measures(text: &str) -> Result<Vec<TaskMeasure>, IntentError> {
    let mut lines = text.lines().enumerate().peekable();
    let (mut found, mut section, mut fence) = (false, None, None);
    let (mut comment, mut after_item) = (false, false);
    let mut measures = Vec::new();
    while let Some((row, raw)) = lines.next() {
        if comment {
            commented_line(raw, &mut comment);
            continue;
        }
        let text = raw.trim();
        if std::mem::take(&mut after_item)
            && !text.is_empty()
            && list_item(text).is_none()
            && heading(text, lines.peek().map(|(_, s)| *s)).is_none()
            && fence_run(text).is_none()
            && !thematic_break(text)
            && !text.contains("<!--")
        {
            return Err(IntentError::InvalidMeasure {
                line: row.saturating_add(1),
            });
        }
        let Some(line) = markdown_line(raw) else {
            continue;
        };
        if let Some((marker, length)) = fence {
            if fence_run(line).is_some_and(|(ch, count, tail)| {
                ch == marker && count >= length && tail.trim().is_empty()
            }) {
                fence = None;
            }
            continue;
        }
        if commented_line(line, &mut comment) {
            // An annotated heading still ends the section it follows.
            if let Some((level, _, _)) = heading(line, lines.peek().map(|(_, s)| *s)) {
                if section.is_some_and(|current| level <= current) {
                    section = None;
                }
            }
            continue;
        }
        if let Some((marker, length, _)) = fence_run(line) {
            fence = Some((marker, length));
            continue;
        }
        if let Some((level, title, setext)) = heading(line, lines.peek().map(|(_, s)| *s)) {
            if setext {
                lines.next();
            }
            if title.eq_ignore_ascii_case("How we will know") {
                if found {
                    return Err(IntentError::DuplicateSection {
                        line: row.saturating_add(1),
                    });
                }
                found = true;
                section = Some(level);
            } else if section.is_some_and(|current| level <= current) {
                section = None;
            }
            continue;
        }
        if section.is_none() || thematic_break(line) {
            continue;
        }
        if let Some(item) = list_item(line) {
            let measure = measure(item).ok_or_else(|| IntentError::InvalidMeasure {
                line: row.saturating_add(1),
            })?;
            if !measures.contains(&measure) {
                measures.push(measure);
            }
            after_item = true;
        }
    }
    if !found {
        Err(IntentError::MissingSection)
    } else if measures.is_empty() {
        Err(IntentError::EmptySection)
    } else {
        Ok(measures)
    }
}

// Ignore the whole line if any part is commented: unsupported inline-comment
// formatting must not turn a partially hidden declaration into an approval.
fn commented_line(mut line: &str, open: &mut bool) -> bool {
    let mut commented = *open;
    while let Some((_, tail)) = line.split_once(if *open { "-->" } else { "<!--" }) {
        commented = true;
        *open = !*open;
        line = tail;
    }
    commented
}

fn markdown_line(raw: &str) -> Option<&str> {
    let line = raw.trim_start_matches(' ');
    (raw.len().saturating_sub(line.len()) <= 3 && !line.starts_with('\t')).then(|| line.trim_end())
}

fn fence_run(line: &str) -> Option<(char, usize, &str)> {
    let marker = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let count = line.chars().take_while(|c| *c == marker).count();
    (count >= 3).then_some((marker, count, line.get(count..)?))
}

fn heading<'a>(line: &'a str, next: Option<&str>) -> Option<(usize, &'a str, bool)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = line.get(level..)?;
    if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
        let mut title = rest.trim();
        let unclosed = title.trim_end_matches('#');
        if unclosed.chars().next_back().is_none_or(char::is_whitespace) {
            title = unclosed.trim_end();
        }
        return Some((level, title, false));
    }
    if line.is_empty() || list_item(line).is_some() || thematic_break(line) {
        return None;
    }
    let underline = markdown_line(next?)?;
    let level = match underline.chars().next()? {
        '=' if underline.chars().all(|c| c == '=') => 1,
        '-' if underline.chars().all(|c| c == '-') => 2,
        _ => return None,
    };
    Some((level, line.trim(), true))
}

fn thematic_break(line: &str) -> bool {
    let Some(marker) = line.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    line.chars().filter(|c| !c.is_whitespace()).count() >= 3
        && line.chars().all(|c| c == marker || c.is_whitespace())
}

fn list_item(line: &str) -> Option<&str> {
    let (marker, item) = line.split_once([' ', '\t']).unwrap_or((line, ""));
    let numbered = marker.strip_suffix(['.', ')']).is_some_and(|digits| {
        !digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit())
    });
    (matches!(marker, "-" | "*" | "+") || numbered).then(|| item.trim())
}

fn measure(item: &str) -> Option<TaskMeasure> {
    if item == "owner" {
        return Some(TaskMeasure::Owner);
    }
    let (kind, value) = item.split_once(':')?;
    let value = unquote(value.trim())?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    match kind {
        "mechanical" => Some(TaskMeasure::Mechanical {
            command: value.into(),
        }),
        "reviewed" if safe_rubric(value) => Some(TaskMeasure::Reviewed {
            rubric: value.into(),
        }),
        _ => None,
    }
}

fn unquote(value: &str) -> Option<&str> {
    let ticks = value.chars().take_while(|c| *c == '`').count();
    if ticks == 0 {
        return Some(value);
    }
    if value.chars().rev().take_while(|c| *c == '`').count() != ticks {
        return None;
    }
    let delimiter = value.get(..ticks)?;
    Some(
        value
            .strip_prefix(delimiter)?
            .strip_suffix(delimiter)?
            .trim(),
    )
}

fn safe_rubric(value: &str) -> bool {
    value.strip_prefix(".atm/measures/").is_some_and(|path| {
        !path.contains('\\') && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
    })
}
