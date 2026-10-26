//! Best-effort token-usage extraction from Codex rollout transcripts.
//!
//! Codex hook payloads include a `transcript_path`, but the transcript
//! format is not a stable hook interface. Keep all knowledge of its
//! current JSONL shape here, scan only a bounded tail, and treat absent
//! or changed data as unavailable rather than failing hook processing.
//!
//! Captured: codex-cli 0.163.0, 2026-10-24,
//! `tests/fixtures/codex_rollout_tokens_0.163.0.jsonl` (see the crate
//! docs). The reader was added in 49f2350 on 2026-08-16 against a codex
//! version that was not recorded.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use atm_core::LifecycleEvent;
use serde::Deserialize;

/// Maximum transcript data inspected for one hook event.
///
/// Token-count records are emitted near the end of a rollout. Bounding
/// the scan prevents a large or malformed transcript from delaying the
/// daemon while still allowing intervening event records.
const MAX_TRAILING_BYTES: u64 = 1_048_576;

/// Token usage extracted from the latest Codex `token_count` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexTokenUsage {
    /// Cumulative tokens consumed by the session.
    pub total_tokens: Option<u64>,
    /// Tokens occupied by the most recent active context.
    pub current_tokens: Option<u64>,
    /// Context-window capacity reported by Codex.
    pub context_window_size: Option<u32>,
}

impl CodexTokenUsage {
    /// Converts the extracted usage to the vendor-neutral lifecycle shape.
    #[must_use]
    pub fn to_lifecycle_event(self) -> LifecycleEvent {
        LifecycleEvent::ContextUpdate {
            tokens: self.total_tokens,
            current_tokens: self.current_tokens,
            context_window_size: self.context_window_size,
            cost_usd: None,
        }
    }

    fn from_info(info: TokenCountInfo) -> Option<Self> {
        // A current-context value is required. Emitting a cumulative-
        // only update would make the core's pi compatibility fallback
        // display lifetime usage as Codex context occupancy.
        let current_tokens = info.last_token_usage.and_then(|usage| usage.total_tokens)?;

        Some(Self {
            total_tokens: info.total_token_usage.and_then(|usage| usage.total_tokens),
            current_tokens: Some(current_tokens),
            context_window_size: info
                .model_context_window
                .and_then(|size| u32::try_from(size).ok())
                .filter(|size| *size > 0),
        })
    }
}

/// Reads the latest usable token-count record from a Codex JSONL rollout.
///
/// Only the final `MAX_TRAILING_BYTES` bytes are inspected. A valid
/// transcript with no recent usage record returns `Ok(None)`; I/O
/// failures are returned so the daemon can log and ignore them.
pub fn read_token_usage(path: impl AsRef<Path>) -> io::Result<Option<CodexTokenUsage>> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Ok(None);
    }

    let mut file = File::open(path)?;
    let file_len = metadata.len();
    let read_len = file_len.min(MAX_TRAILING_BYTES);
    let start = file_len.saturating_sub(read_len);

    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(read_len).unwrap_or(0));
    file.take(read_len).read_to_end(&mut bytes)?;

    // A bounded tail normally starts in the middle of a JSONL record.
    // Drop that partial line before UTF-8 and JSON decoding.
    let complete_lines = if start == 0 {
        bytes.as_slice()
    } else if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
        bytes.get(newline.saturating_add(1)..).unwrap_or_default()
    } else {
        return Ok(None);
    };

    let Ok(text) = std::str::from_utf8(complete_lines) else {
        return Ok(None);
    };

    for line in text.lines().rev() {
        let Ok(record) = serde_json::from_str::<RolloutRecord>(line) else {
            continue;
        };
        let Some(payload) = record.payload else {
            continue;
        };
        if record.record_type.as_deref() != Some("event_msg")
            || payload.payload_type.as_deref() != Some("token_count")
        {
            continue;
        }
        // Codex emits `info: null` placeholders near the beginning of
        // ordinary turns. Skip them and keep searching for the latest
        // actual measurement instead of falsely resetting usage.
        // Captured: codex-cli, version and method not recorded; added in
        // 49f2350 on 2026-08-16. codex-cli 0.163.0 wrote none in 40
        // records against a fake provider that sends no rate-limit
        // headers; the real backend is unverified.
        let Some(info) = payload.info else {
            continue;
        };
        if let Some(usage) = CodexTokenUsage::from_info(info) {
            return Ok(Some(usage));
        }
    }

    Ok(None)
}

#[derive(Debug, Deserialize)]
struct RolloutRecord {
    #[serde(rename = "type", default)]
    record_type: Option<String>,
    #[serde(default)]
    payload: Option<RolloutPayload>,
}

#[derive(Debug, Deserialize)]
struct RolloutPayload {
    #[serde(rename = "type", default)]
    payload_type: Option<String>,
    #[serde(default)]
    info: Option<TokenCountInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    #[serde(default)]
    total_token_usage: Option<TokenBreakdown>,
    #[serde(default)]
    last_token_usage: Option<TokenBreakdown>,
    #[serde(default)]
    model_context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenBreakdown {
    #[serde(default)]
    total_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn transcript(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create transcript");
        for line in lines {
            writeln!(file, "{line}").expect("write transcript line");
        }
        file
    }

    #[test]
    fn reads_latest_token_count_while_ignoring_transcript_content() {
        // Synthetic: hand-written records in the token_count shape.
        let file = transcript(&[
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100},"last_token_usage":{"total_tokens":80},"model_context_window":200000}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","content":"sensitive prompt"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"total_tokens":1000},"last_token_usage":{"cached_input_tokens":700,"total_tokens":250},"model_context_window":258400}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);

        let usage = read_token_usage(file.path())
            .expect("read transcript")
            .expect("token usage");
        assert_eq!(
            usage,
            CodexTokenUsage {
                total_tokens: Some(1000),
                current_tokens: Some(250),
                context_window_size: Some(258_400),
            }
        );
    }

    #[test]
    fn null_info_placeholder_keeps_the_last_valid_usage() {
        // Synthetic: hand-written records; no capture with a null `info`
        // was kept.
        let file = transcript(&[
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100},"last_token_usage":{"total_tokens":80},"model_context_window":200000}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#,
        ]);

        assert_eq!(
            read_token_usage(file.path()).expect("read transcript"),
            Some(CodexTokenUsage {
                total_tokens: Some(100),
                current_tokens: Some(80),
                context_window_size: Some(200_000),
            })
        );
    }

    #[test]
    fn malformed_and_unrelated_records_are_ignored() {
        // Synthetic: hand-written malformed and unrelated records.
        let file = transcript(&[
            "not-json",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);

        assert_eq!(
            read_token_usage(file.path()).expect("read transcript"),
            None
        );
    }

    #[test]
    fn non_regular_transcript_path_is_ignored() {
        // Synthetic: a directory in place of a rollout file.
        let directory = tempfile::tempdir().expect("create transcript directory");
        assert_eq!(
            read_token_usage(directory.path()).expect("inspect transcript path"),
            None
        );
    }

    #[test]
    fn cumulative_only_schema_drift_does_not_replace_context_usage() {
        // Synthetic: hand-written drift case; no capture has shown a
        // token_count without last_token_usage.
        let file = transcript(&[
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100},"last_token_usage":{"total_tokens":80},"model_context_window":200000}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":200},"model_context_window":200000}}}"#,
        ]);

        let usage = read_token_usage(file.path())
            .expect("read transcript")
            .expect("previous valid usage");
        assert_eq!(usage.total_tokens, Some(100));
        assert_eq!(usage.current_tokens, Some(80));
    }

    #[test]
    fn scan_is_bounded_and_discards_a_partial_first_line() {
        // Synthetic: an oversized hand-written record.
        let mut file = NamedTempFile::new().expect("create transcript");
        let oversized = "x".repeat(usize::try_from(MAX_TRAILING_BYTES).expect("bounded size"));
        writeln!(file, "{oversized}").expect("write oversized record");
        let usage_line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":10},"last_token_usage":{"total_tokens":5},"model_context_window":100}}}"#;
        writeln!(file, "{usage_line}").expect("write token usage");

        let usage = read_token_usage(file.path())
            .expect("read transcript")
            .expect("token usage");
        assert_eq!(usage.current_tokens, Some(5));
    }

    #[test]
    fn out_of_range_context_window_is_ignored() {
        // Synthetic: a hand-written out-of-range window.
        let file = transcript(&[
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":10},"last_token_usage":{"total_tokens":5},"model_context_window":4294967296}}}"#,
        ]);

        let usage = read_token_usage(file.path())
            .expect("read transcript")
            .expect("token usage");
        assert_eq!(usage.context_window_size, None);
        assert_eq!(usage.current_tokens, Some(5));
    }

    #[test]
    fn captured_rollout_usage_over_one_session() {
        // Captured: codex-cli 0.163.0, 2026-10-24, the rollout of the
        // codex_tui_session_0.163.0.jsonl run, trimmed to its task,
        // token_usage_record and token_count records (lines verbatim).
        // A fake provider supplied the usage numbers.
        let lines: Vec<&str> = include_str!("../tests/fixtures/codex_rollout_tokens_0.163.0.jsonl")
            .lines()
            .collect();
        let usage_after = |count: usize| {
            let file = transcript(lines.get(..count).expect("fixture prefix"));
            read_token_usage(file.path()).expect("read transcript")
        };

        // The hook-time snapshots below match what a logging hook read
        // from the live rollout at each hook in the capture runs; the
        // fixture itself carries no hook timing.
        //
        // While the first tool runs, the reply that asked for it has a
        // token_usage_record on disk but no token_count yet: codex
        // writes that after the tool output, so usage lags mid-turn.
        assert_eq!(usage_after(2), None);
        // At Stop both of the turn's requests in this thread are counted
        // (task_complete, line 6, comes later). Codex's thread-title
        // request runs in a separate thread and is in no rollout here.
        assert_eq!(
            usage_after(5),
            Some(CodexTokenUsage {
                total_tokens: Some(4552),
                current_tokens: Some(3277),
                context_window_size: Some(258_400),
            })
        );
        // After /compact, last_token_usage has every field 0 except
        // total_tokens: codex's own estimate of the compacted history.
        assert_eq!(
            usage_after(lines.len()),
            Some(CodexTokenUsage {
                total_tokens: Some(8830),
                current_tokens: Some(7305),
                context_window_size: Some(258_400),
            })
        );
    }
}
