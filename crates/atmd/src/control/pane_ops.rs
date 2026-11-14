//! Fixed reserved tmux operations. The owner actor supplies the checked fence.
//! Framing was verified against tmux 3.8; see fixtures/tmux_3.8_reserved_observations.json.
use super::{
    state::{Fence, State},
    CaptureFormat, ExpectedTag, PaneCapture, PaneId, PaneInspection, PaneKey, ReservationError,
    ReservationKind,
};
use crate::control::{ControlError, SpawnContext};
use atm_org::AgentId;
use atm_tmux::{PaneDirection, PaneInfo, TmuxClient, TmuxError};
use tokio::time::Instant;

pub(super) const FORMAT: &str = "ATM-PANE-v1\t#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{pane_dead}\t#{pane_in_mode}\t#{bracket_paste_flag}\t#{cursor_x}\t#{cursor_y}";
const END: &str = "ATM-END\t#{pane_id}";
pub(in crate::control) type Guard = Box<dyn FnOnce(&PaneCapture) -> bool + Send>;
pub(in crate::control) enum Input {
    Key(PaneKey),
    Paste(String),
}
pub(in crate::control) enum Action {
    Inspect(PaneId),
    Capture(PaneId, CaptureFormat),
    Tag(PaneId, ExpectedTag),
    Untag(PaneId, AgentId),
    Input {
        pane: PaneId,
        expected: AgentId,
        input: Input,
        guard: Option<(CaptureFormat, Guard)>,
    },
    Split(SpawnContext, PaneDirection, String),
    Kill(
        PaneId,
        AgentId,
        serde_json::Value,
        chrono::DateTime<chrono::Utc>,
    ),
}
pub(in crate::control) enum Output {
    Inspection(Option<PaneInspection>),
    Capture(PaneCapture),
    Pane(PaneId),
    Audit(std::sync::Arc<super::kill_audit::KillAudit>),
    Done,
}
impl Action {
    pub(super) fn audit(&self) -> Option<(atm_org::OrgCommand, chrono::DateTime<chrono::Utc>)> {
        match self {
            Self::Kill(pane, _, action, now) => Some((
                atm_org::OrgCommand::ControlKilled(atm_org::ControlAudit {
                    pane: pane.to_string(),
                    action: action.clone(),
                }),
                *now,
            )),
            _ => None,
        }
    }

    fn owned_pane(&self) -> Option<&PaneId> {
        match self {
            Self::Inspect(_) | Self::Split(..) => None,
            Self::Capture(p, _) | Self::Tag(p, _) | Self::Untag(p, _) | Self::Kill(p, ..) => {
                Some(p)
            }
            Self::Input { pane, .. } => Some(pane),
        }
    }
    fn expected(&self) -> Option<&AgentId> {
        match self {
            Self::Tag(_, ExpectedTag::Agent(agent))
            | Self::Untag(_, agent)
            | Self::Kill(_, agent, ..) => Some(agent),
            Self::Input { expected, .. } => Some(expected),
            _ => None,
        }
    }
    fn allowed(&self, kind: ReservationKind) -> bool {
        use ReservationKind::*;
        match self {
            Self::Inspect(_) | Self::Capture(..) => true,
            Self::Tag(..) => matches!(kind, Up | Adopt | Respawn),
            Self::Untag(..) | Self::Kill(..) => matches!(kind, Down | Respawn),
            Self::Input { .. } => matches!(kind, Up | Respawn | Clear | Delivery),
            Self::Split(..) => matches!(kind, Up | Respawn),
        }
    }
}
fn invalid(reason: &str) -> ReservationError {
    ReservationError::InvalidRequest {
        reason: reason.into(),
    }
}
fn observation(pane: &PaneId, reason: &str) -> ReservationError {
    ReservationError::InvalidObservation {
        pane: pane.clone(),
        reason: reason.into(),
    }
}
fn tmux(pane: &PaneId, step: &str, source: TmuxError) -> ReservationError {
    ControlError::Tmux {
        context: format!("Failed to {step} for pane {pane}"),
        source,
    }
    .into()
}

pub(super) async fn execute(
    client: &dyn TmuxClient,
    state: &mut State,
    fence: &Fence,
    action: Action,
    now: Instant,
) -> Result<Output, ReservationError> {
    state.check(fence, action.owned_pane(), now)?;
    let (agent, kind) = state.owner(fence)?;
    if !action.allowed(kind) {
        return Err(invalid(
            "operation is not permitted by this reservation kind",
        ));
    }
    if action.expected().is_some_and(|expected| expected != &agent) {
        return Err(invalid("expected agent must equal the reservation agent"));
    }
    match action {
        Action::Inspect(pane) => Ok(Output::Inspection(inspect(client, &pane).await?)),
        Action::Capture(pane, format) => Ok(Output::Capture(capture(client, &pane, format).await?)),
        Action::Split(context, direction, size) => {
            state.can_split(fence)?;
            let pane = PaneId::parse(
                crate::control::split_spawn(client, &context, direction, &size).await?,
            )?;
            // No actor operation can interleave between creation and claiming.
            // A collision is uncertain ownership, never permission to kill it.
            state.claim(fence, pane.clone())?;
            Ok(Output::Pane(pane))
        }
        Action::Tag(pane, expected) => {
            if expected == ExpectedTag::Absent
                && kind != ReservationKind::Adopt
                && !state.created(fence, &pane)?
            {
                return Err(invalid(
                    "Absent is only permitted for adoption or this job's new pane",
                ));
            }
            check_tag(&pane, &expected, &present(client, &pane).await?)?;
            client
                .raw_command(
                    "set-option",
                    &["-p", "-t", pane.as_str(), "@atm-agent", &agent.to_string()],
                )
                .await
                .map_err(|source| tmux(&pane, "set local tag", source))?;
            Ok(Output::Done)
        }
        Action::Untag(pane, expected) => {
            check_tag(
                &pane,
                &ExpectedTag::Agent(expected),
                &present(client, &pane).await?,
            )?;
            client
                .raw_command(
                    "set-option",
                    &["-p", "-u", "-t", pane.as_str(), "@atm-agent"],
                )
                .await
                .map_err(|source| tmux(&pane, "remove local tag", source))?;
            Ok(Output::Done)
        }
        Action::Kill(pane, expected, _, _) => {
            check_tag(
                &pane,
                &ExpectedTag::Agent(expected),
                &present(client, &pane).await?,
            )?;
            client
                .kill_pane(pane.as_str())
                .await
                .map_err(|source| tmux(&pane, "kill", source))?;
            Ok(Output::Done)
        }
        Action::Input {
            pane,
            expected,
            input,
            guard,
        } => {
            let expected = ExpectedTag::Agent(expected);
            if let Some((format, check)) = guard {
                let fresh = capture(client, &pane, format).await?;
                check_tag(&pane, &expected, &fresh.inspection)?;
                check_input(&pane, &fresh.inspection, &input)?;
                // A consumer predicate is required to be pure and quick. Treat a
                // violated contract (panic) as rejection, not loss of the actor.
                let approved =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&fresh)))
                        .unwrap_or(false);
                if !approved {
                    return Err(ReservationError::ScreenChanged { pane });
                }
            } else {
                let fresh = present(client, &pane).await?;
                check_tag(&pane, &expected, &fresh)?;
                check_input(&pane, &fresh, &input)?;
            }
            match input {
                Input::Key(key) => {
                    client
                        .send_keys(
                            pane.as_str(),
                            match key {
                                PaneKey::Enter => "Enter",
                                PaneKey::Escape => "Escape",
                                PaneKey::Up => "Up",
                                PaneKey::Down => "Down",
                            },
                        )
                        .await
                }
                Input::Paste(text) => client.paste_text(pane.as_str(), &text).await,
            }
            .map_err(|source| tmux(&pane, "send reserved input", source))?;
            Ok(Output::Done)
        }
    }
}
fn check_input(
    pane: &PaneId,
    inspection: &PaneInspection,
    input: &Input,
) -> Result<(), ReservationError> {
    let text = match input {
        Input::Paste(text) => Some(text.as_str()),
        Input::Key(_) => None,
    };
    let reason = crate::control::send::input_refusal(
        inspection.in_mode,
        inspection.dead,
        inspection.bracketed_paste,
        text,
    )
    .or_else(|| {
        text.filter(|text| {
            text.chars()
                .any(|c| c.is_ascii_control() && !matches!(c, '\t' | '\n' | '\r'))
        })
        .map(|_| "paste text contains unsafe control bytes")
    });
    match reason {
        Some(reason) => Err(ControlError::SendRefused {
            pane: pane.to_string(),
            reason,
        }
        .into()),
        None => Ok(()),
    }
}

fn check_tag(
    pane: &PaneId,
    expected: &ExpectedTag,
    inspection: &PaneInspection,
) -> Result<(), ReservationError> {
    let wanted = match expected {
        ExpectedTag::Absent => None,
        ExpectedTag::Agent(agent) => Some(agent.to_string()),
    };
    if inspection.local_agent_tag == wanted {
        Ok(())
    } else {
        Err(ReservationError::TagMismatch {
            pane: pane.clone(),
            expected: expected.clone(),
            actual: inspection.local_agent_tag.clone(),
        })
    }
}
async fn metadata(
    client: &dyn TmuxClient,
    pane: &PaneId,
) -> Result<Option<PaneInspection>, ReservationError> {
    let filter = format!("#{{==:#{{pane_id}},{pane}}}");
    let output = client
        .raw_command("list-panes", &["-a", "-f", &filter, "-F", FORMAT])
        .await
        .map_err(|source| tmux(pane, "read pane metadata", source))?;
    if output.is_empty() {
        Ok(None)
    } else {
        decode_metadata(pane, &output).map(Some)
    }
}
async fn tag(client: &dyn TmuxClient, pane: &PaneId) -> Result<Option<String>, ReservationError> {
    let output = client
        .raw_command(
            "show-options",
            &["-p", "-q", "-v", "-t", pane.as_str(), "@atm-agent"],
        )
        .await
        .map_err(|source| tmux(pane, "read local tag", source))?;
    if output.is_empty() {
        return Ok(None);
    }
    output
        .strip_suffix('\n')
        .map(|s| Some(s.to_owned()))
        .ok_or_else(|| observation(pane, "unterminated local tag"))
}
pub(in crate::control) async fn inspect(
    client: &dyn TmuxClient,
    pane: &PaneId,
) -> Result<Option<PaneInspection>, ReservationError> {
    let Some(mut inspection) = metadata(client, pane).await? else {
        return Ok(None);
    };
    inspection.local_agent_tag = tag(client, pane).await?;
    Ok(Some(inspection))
}
async fn present(
    client: &dyn TmuxClient,
    pane: &PaneId,
) -> Result<PaneInspection, ReservationError> {
    inspect(client, pane)
        .await?
        .ok_or_else(|| ReservationError::PaneGone { pane: pane.clone() })
}
async fn capture(
    client: &dyn TmuxClient,
    pane: &PaneId,
    format: CaptureFormat,
) -> Result<PaneCapture, ReservationError> {
    metadata(client, pane)
        .await?
        .ok_or_else(|| ReservationError::PaneGone { pane: pane.clone() })?;
    let mut args = vec![
        "-p",
        "-t",
        pane.as_str(),
        FORMAT,
        ";",
        "capture-pane",
        "-p",
        "-N",
    ];
    if format == CaptureFormat::Ansi {
        args.push("-e");
    }
    args.extend([
        "-t",
        pane.as_str(),
        ";",
        "display-message",
        "-p",
        "-t",
        pane.as_str(),
        END,
    ]);
    let output = client
        .raw_command("display-message", &args)
        .await
        .map_err(|source| tmux(pane, "capture viewport", source))?;
    let mut capture = decode_capture(pane, &output, format)?;
    capture.inspection.local_agent_tag = tag(client, pane).await?;
    Ok(capture)
}

fn decode_metadata(pane: &PaneId, output: &str) -> Result<PaneInspection, ReservationError> {
    let fail = || observation(pane, "malformed pane metadata");
    let mut lines = output.strip_suffix('\n').ok_or_else(fail)?.split('\n');
    let first = decode_metadata_line(pane, lines.next().ok_or_else(fail)?)?;
    for line in lines {
        let mut next = decode_metadata_line(pane, line)?;
        // Linked/grouped windows list the same pane once per session. Preserve
        // the first location, but require every pane-level observation to agree.
        next.pane.session_name.clone_from(&first.pane.session_name);
        next.pane.window_index = first.pane.window_index;
        if next != first {
            return Err(fail());
        }
    }
    Ok(first)
}
fn decode_metadata_line(pane: &PaneId, line: &str) -> Result<PaneInspection, ReservationError> {
    let fail = || observation(pane, "malformed pane metadata");
    if line.contains(['\r', '\0']) {
        return Err(fail());
    }
    let mut fields = line.split('\t');
    if fields.next() != Some("ATM-PANE-v1") || fields.next() != Some(pane.as_str()) {
        return Err(fail());
    }
    let session_name = fields
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(fail)?
        .to_owned();
    let window_index = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    let pane_pid = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    let width: u16 = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    let height: u16 = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    let boolean = |s: Option<&str>| match s {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(fail()),
    };
    let is_active = boolean(fields.next())?;
    let dead = boolean(fields.next())?;
    let in_mode = fields
        .next()
        .ok_or_else(fail)?
        .parse::<u32>()
        .map_err(|_| fail())?
        != 0;
    let bracketed_paste = match fields.next() {
        Some("") => None,
        value => Some(boolean(value)?),
    };
    let cursor_column: u32 = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    let cursor_row: u32 = fields
        .next()
        .ok_or_else(fail)?
        .parse()
        .map_err(|_| fail())?;
    if fields.next().is_some()
        || width == 0
        || height == 0
        || cursor_column > u32::from(width)
        || cursor_row >= u32::from(height)
    {
        return Err(fail());
    }
    Ok(PaneInspection::new(
        PaneInfo {
            pane_id: pane.to_string(),
            session_name,
            window_index,
            pane_pid,
            width,
            height,
            is_active,
        },
        None,
        dead,
        in_mode,
        bracketed_paste,
        cursor_column,
        cursor_row,
    ))
}
fn decode_capture(
    pane: &PaneId,
    output: &str,
    format: CaptureFormat,
) -> Result<PaneCapture, ReservationError> {
    let (metadata, rest) = output
        .split_once('\n')
        .ok_or_else(|| observation(pane, "missing frame metadata"))?;
    let inspection = decode_metadata(pane, &format!("{metadata}\n"))?;
    let end = format!("ATM-END\t{pane}\n");
    let text = rest
        .strip_suffix(&end)
        .ok_or_else(|| observation(pane, "missing frame end marker"))?;
    if !text.ends_with('\n')
        || text.split_terminator('\n').count() != usize::from(inspection.pane.height)
        || text.contains('\0')
    {
        return Err(observation(pane, "incomplete viewport"));
    }
    Ok(PaneCapture::new(text.into(), inspection, format))
}

#[cfg(test)]
#[path = "pane_ops_tests.rs"]
mod tests;
