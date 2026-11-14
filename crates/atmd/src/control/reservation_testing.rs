//! Public consumer test support for the reserved-operation read protocol.
//!
//! This module is doc-hidden, not `cfg(test)` or feature-gated: integration tests
//! and U8/U9b consumers can use the same scripting surface without extra features.
//! It wraps [`MockTmuxClient`] for effect recording and dispatches scripted reads
//! by canonical pane, never a global raw-output FIFO. It introduces no new trait,
//! dependency or real tmux I/O. Never use it as a production tmux client.
//!
//! # Read contract
//!
//! Implemented operations consume these sequences. Fixed tag writes
//! are recorded in mock() and need no extra observation script:
//!
//! | Operation | Ordered reads before any effect | Script |
//! |---|---|---|
//! | inspect, present pane | filtered metadata; pane-local tag (2) | script_inspect(Some) |
//! | inspect, missing pane | successful empty filtered metadata only (1) | script_inspect(None) |
//! | tag / untag / unguarded keys / paste / kill | filtered metadata; pane-local tag (2), then effect | script_tag_check |
//! | capture / observe | filtered metadata; single metadata/capture/clients/end command list; pane-local tag (3) | script_capture / script_observe |
//! | keys_guarded / paste_guarded | same 3 reads as capture, pure check, then effect if approved | script_capture |
//! | split | no observation reads; SplitWindow, then claim before replying | mock().set_next_pane_id |
//! | legacy send / spawn_type, unavailable org | ownership: filtered metadata; pane-local tag (2), before input | script_tag_check |
//!
//! A missing pane stops after the first read. Fence, kind or expected-agent
//! rejection can stop before any read. Metadata/capture framing matches the
//! private tmux 3.8 probe fixture; `-N` preserves trailing spaces and viewport
//! rows, `-e` preserves style. The final tag read distinguishes no bytes (unset)
//! from a newline (present but empty), without inheriting global/window options.
//! Use a full newline-terminated viewport for script_capture.
//!
//! Script each operation separately. Scripts are FIFO **within a pane**; other
//! panes can interleave freely. A missing script, wrong read order/format, or
//! unsupported raw command returns TmuxError, never a plausible empty answer.
//! Non-raw effects delegate to mock(); use its calls/errors/unique split IDs,
//! not set_next_raw_output, which cannot script these reserved reads.
//!
//! Legacy ownership is the only legacy read this client serves. It cannot serve
//! U7 send's `#{pane_in_mode} #{bracket_paste_flag}` state read or a noncanonical
//! target's `#{pane_id}` resolution read: both are rejected, not answered by the
//! mock's raw-output FIFO. Of legacy input, only canonical-target spawn_type is
//! supported end-to-end (script an
//! untagged ownership check when the org is unavailable); use MockTmuxClient for
//! legacy sends or alias resolution.
use super::{
    pane_ops::{CLIENTS, CLIENTS_MARKER, COMMAND, FORMAT},
    CaptureFormat, PaneCapture, PaneId, PaneInspection,
};
use atm_tmux::{MockTmuxClient, PaneDirection, PaneInfo, TmuxClient, TmuxError};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripted_default_matches_real_v2_frames_including_absent_process_facts() {
        for (fixture, bracketed) in [
            (include_str!("fixtures/tmux_3.6_reserved_v2.json"), None),
            (
                include_str!("fixtures/tmux_3.8_reserved_v2.json"),
                Some(false),
            ),
        ] {
            let data: serde_json::Value = serde_json::from_str(fixture).unwrap();
            for sample in data["samples"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["label"] != "paused")
            {
                let inspection = PaneInspection::new(
                    PaneInfo {
                        pane_id: "%0".into(),
                        session_name: "probe".into(),
                        window_index: 0,
                        pane_pid: 123,
                        width: 40,
                        height: 6,
                        is_active: true,
                    },
                    None,
                    false,
                    false,
                    bracketed,
                    0,
                    0,
                )
                .with_foreground_process(
                    None,
                    (sample["argv0"] == "neutral-program").then(|| "neutral-program".into()),
                );
                assert_eq!(
                    metadata(&inspection).output,
                    sample["metadata"].as_str().unwrap()
                );
                let capture =
                    PaneCapture::new("\n\n\n\n\n\n".into(), inspection, CaptureFormat::Plain)
                        .with_focus(Some(sample["label"] == "attached"));
                let client = ScriptedTmuxClient::new();
                client.script_capture(&capture).unwrap();
                let scripts = client.scripts.lock().unwrap();
                let reads = scripts.get(&PaneId::parse("%0").unwrap()).unwrap();
                assert_eq!(
                    reads.get(1).unwrap().output,
                    sample["frame"].as_str().unwrap()
                );
            }
        }
    }
}

const LEGACY_FORMAT: &str = "ATM-PANE-v1\t#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{pane_dead}\t#{pane_in_mode}\t#{bracket_paste_flag}\t#{cursor_x}\t#{cursor_y}";
const END: &str = "ATM-END\t#{pane_id}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadKind {
    Metadata,
    Tag,
    Frame(CaptureFormat),
}

#[derive(Debug)]
struct Read {
    kind: ReadKind,
    output: String,
}

/// A per-pane observation script with the existing mock's effect recorder.
#[derive(Debug, Clone, Default)]
pub struct ScriptedTmuxClient {
    mock: MockTmuxClient,
    scripts: Arc<Mutex<HashMap<PaneId, VecDeque<Read>>>>,
}

fn error(reason: impl Into<String>) -> TmuxError {
    TmuxError::ParseError(reason.into())
}
fn pane_id(value: &str) -> Result<PaneId, TmuxError> {
    PaneId::parse(value).map_err(|e| error(e.to_string()))
}

impl ScriptedTmuxClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared effect recorder and fault/split configuration. Reserved raw reads
    /// come from scripts, never the mock's global set_next_raw_output queue.
    #[must_use]
    pub fn mock(&self) -> MockTmuxClient {
        self.mock.clone()
    }

    /// Queue one inspection (two reads), or positive missing-pane evidence (one).
    pub fn script_inspect(
        &self,
        pane: &PaneId,
        inspection: Option<&PaneInspection>,
    ) -> Result<(), TmuxError> {
        let reads = match inspection {
            Some(inspection) => {
                if pane.as_str() != inspection.pane.pane_id {
                    return Err(error("inspection belongs to a different pane"));
                }
                vec![metadata(inspection), local_tag(inspection)]
            }
            None => vec![Read {
                kind: ReadKind::Metadata,
                output: String::new(),
            }],
        };
        self.push(pane.clone(), reads)
    }

    /// Queue the two mutation-time ownership reads, including fresh metadata.
    /// A separate prior inspection is not reused as permission to mutate.
    pub fn script_tag_check(&self, inspection: &PaneInspection) -> Result<(), TmuxError> {
        self.script_inspect(&pane_id(&inspection.pane.pane_id)?, Some(inspection))
    }

    /// Queue one read-only observation through ControlHandle, with the same
    /// framing/metadata/focus as a reserved capture. No reservation is scripted.
    pub fn script_observe(&self, capture: &PaneCapture) -> Result<(), TmuxError> {
        self.script_capture(capture)
    }

    /// Queue one capture/guard (three reads). Requires exactly height newline-
    /// terminated rows, preserving text/style bytes and meaningful whitespace.
    pub fn script_capture(&self, capture: &PaneCapture) -> Result<(), TmuxError> {
        let pane = pane_id(&capture.inspection.pane.pane_id)?;
        if !capture.text.ends_with('\n')
            || capture.text.split_terminator('\n').count()
                != usize::from(capture.inspection.pane.height)
        {
            return Err(error(
                "capture script requires a full newline-terminated viewport",
            ));
        }
        let first = metadata(&capture.inspection);
        let clients = match capture.focused {
            Some(true) => {
                format!("{CLIENTS_MARKER}\nattached,focused,control-mode,UTF-8\t{pane}\n")
            }
            Some(false) => format!("{CLIENTS_MARKER}\n"),
            // Unknown output is explicitly malformed, never observed absence.
            None => format!("{CLIENTS_MARKER}\n\t{pane}\n"),
        };
        let output = format!(
            "{}{}{clients}ATM-END\t{}\n",
            first.output, capture.text, pane
        );
        self.push(
            pane,
            vec![
                first,
                Read {
                    kind: ReadKind::Frame(capture.format),
                    output,
                },
                local_tag(&capture.inspection),
            ],
        )
    }

    /// Unconsumed reads for this pane, useful after an awaited operation reply.
    pub fn pending_reads(&self, pane: &PaneId) -> Result<usize, TmuxError> {
        let scripts = self
            .scripts
            .lock()
            .map_err(|_| error("observation script lock poisoned"))?;
        Ok(scripts.get(pane).map_or(0, VecDeque::len))
    }

    fn push(&self, pane: PaneId, reads: Vec<Read>) -> Result<(), TmuxError> {
        self.scripts
            .lock()
            .map_err(|_| error("observation script lock poisoned"))?
            .entry(pane)
            .or_default()
            .extend(reads);
        Ok(())
    }

    fn read(&self, subcommand: &str, args: &[&str]) -> Result<String, TmuxError> {
        let (pane, kind) = requested_read(subcommand, args)?;
        let mut scripts = self
            .scripts
            .lock()
            .map_err(|_| error("observation script lock poisoned"))?;
        let reads = scripts
            .get_mut(&pane)
            .ok_or_else(|| error(format!("no observation script for {pane}")))?;
        if reads.front().is_none_or(|read| read.kind != kind) {
            return Err(error(format!(
                "unexpected {kind:?} read for {pane}; script missing or out of order"
            )));
        }
        reads
            .pop_front()
            .map(|read| read.output)
            .ok_or_else(|| error("observation script exhausted"))
    }
}

fn metadata(inspection: &PaneInspection) -> Read {
    let pane = &inspection.pane;
    let output = format!(
        "ATM-PANE-v2\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        pane.pane_id,
        pane.session_name,
        pane.window_index,
        pane.pane_pid,
        pane.width,
        pane.height,
        u8::from(pane.is_active),
        u8::from(inspection.dead),
        u8::from(inspection.in_mode),
        inspection
            .bracketed_paste
            .map(|b| u8::from(b).to_string())
            .unwrap_or_default(),
        inspection.cursor_column,
        inspection.cursor_row,
        inspection
            .foreground_pid
            .map(|pid| pid.to_string())
            .unwrap_or_default(),
        inspection.foreground_command.as_deref().unwrap_or_default(),
        u8::from(inspection.synchronized),
        u8::from(inspection.input_off),
    );
    Read {
        kind: ReadKind::Metadata,
        output,
    }
}

fn local_tag(inspection: &PaneInspection) -> Read {
    Read {
        kind: ReadKind::Tag,
        output: inspection
            .local_agent_tag
            .as_ref()
            .map(|tag| format!("{tag}\n"))
            .unwrap_or_default(),
    }
}

fn requested_read(subcommand: &str, args: &[&str]) -> Result<(PaneId, ReadKind), TmuxError> {
    match (subcommand, args) {
        ("list-panes", ["-a", "-f", filter, "-F", format])
            if matches!(*format, FORMAT | LEGACY_FORMAT)
                || *format == FORMAT.replace(COMMAND, "") =>
        {
            let pane = filter
                .strip_prefix("#{==:#{pane_id},")
                .and_then(|s| s.strip_suffix('}'))
                .ok_or_else(|| error("unexpected pane filter"))?;
            Ok((pane_id(pane)?, ReadKind::Metadata))
        }
        ("show-options", ["-p", "-q", "-v", "-t", pane, "@atm-agent"]) => {
            Ok((pane_id(pane)?, ReadKind::Tag))
        }
        ("display-message", ["-p", "-t", pane, ..]) => {
            let pane_id = pane_id(pane)?;
            for metadata in [FORMAT, LEGACY_FORMAT] {
                for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
                    let mut expected =
                        vec!["-p", "-t", pane, metadata, ";", "capture-pane", "-p", "-N"];
                    if format == CaptureFormat::Ansi {
                        expected.push("-e");
                    }
                    expected.extend(["-t", pane, ";", "display-message", "-p", "-t", pane]);
                    if metadata == FORMAT {
                        expected.extend([
                            CLIENTS_MARKER,
                            ";",
                            "list-clients",
                            "-F",
                            CLIENTS,
                            ";",
                            "display-message",
                            "-p",
                            "-t",
                            pane,
                        ]);
                    }
                    expected.push(END);
                    if args == expected {
                        return Ok((pane_id, ReadKind::Frame(format)));
                    }
                }
            }
            Err(error("unexpected capture command list"))
        }
        _ => Err(error("unsupported reserved observation read")),
    }
}

// TmuxClient uses async-trait's boxed-future ABI. Implement that existing trait
// directly rather than adding a dependency solely for this consumer test fake.
type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
macro_rules! forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
        where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
            Box::pin(self.mock.$method($($arg),*))
        }
    };
}

impl TmuxClient for ScriptedTmuxClient {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        subcommand: &'c str,
        args: &'a [&'v str],
    ) -> Call<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            // Keep mock fault injection and call recording, but not its raw FIFO.
            let output = self.mock.raw_command(subcommand, args).await?;
            if matches!(
                (subcommand, args),
                ("set-option", ["-p", "-t", _, "@atm-agent", _])
                    | ("set-option", ["-p", "-u", "-t", _, "@atm-agent"])
            ) {
                return Ok(output);
            }
            self.read(subcommand, args)
        })
    }
    forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>) -> String);
    forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    forward!(kill_pane<'a>(pane: &'a str) -> ());
    forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
    forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
    forward!(list_panes<>() -> Vec<PaneInfo>);
    forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    forward!(select_pane<'a>(pane: &'a str) -> ());
    forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    forward!(new_session<'a>(name: &'a str) -> String);
}
