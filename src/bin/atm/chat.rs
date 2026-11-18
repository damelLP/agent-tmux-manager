//! Dedicated chat connection/terminal controller. Message rows and IDs have one
//! home in atm-comms; tree/history envelopes remain checked JSON projections.
//! Tree freshness checks are not atomic with subsequent mutations: adoption in
//! that gap can change attribution. Never strip the actual caller pane.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

use atm_chat::{
    ChatConfig, ChatEffect, ChatFailure, ChatInput, ChatPage, ChatState, ChatTime, FrameReport,
    OpToken, SeatBinding, SeatCard, ThreadFacts, ThreadKey, WATCH_CAPABILITY,
};
use atm_comms::{Address, AgentId, AgentStatus, MessageId, SessionId};
use atm_core::{SessionStatus, SessionView};
use atm_protocol::{DaemonMessage, ProtocolVersion, ResponsePayload};
use atm_tmux::TmuxClient;
use atm_tui::{
    control::{ControlFailure, ControlNotice},
    daemon::ControlHandle,
    input::Event,
    ui::theme,
};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{backend::Backend, style::Color, Terminal};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

const READ_BUDGET: Duration = Duration::from_secs(17);
const MESSAGE_BUDGET: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatRunId(String);
impl ChatRunId {
    pub(crate) fn new(start: DateTime<Utc>, pid: u32) -> Self {
        Self(format!("{}-{pid}", start.to_rfc3339()))
    }
}
impl fmt::Display for ChatRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug)]
pub(crate) enum Completion {
    Tree(OpToken, Result<Value, ChatFailure>),
    Input(ChatInput),
}

fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn bad_reply(mutation: bool) -> ChatFailure {
    if mutation {
        ChatFailure::OutcomeUnknown("unreadable daemon success; outcome unknown".into())
    } else {
        ChatFailure::Unavailable("unreadable daemon response".into())
    }
}

pub(crate) fn response_value(
    response: Result<ResponsePayload, ControlFailure>,
    mutation: bool,
) -> Result<Value, ChatFailure> {
    match response {
        Ok(ResponsePayload::Result(value)) => Ok(value),
        Ok(ResponsePayload::Error(error)) => {
            let message = safe(&error.message);
            Err(
                if matches!(
                    error.code.as_str(),
                    "invalid_params" | "refused" | "not_found"
                ) {
                    ChatFailure::Refused(message)
                } else if mutation {
                    ChatFailure::OutcomeUnknown(message)
                } else {
                    ChatFailure::Unavailable(message)
                },
            )
        }
        Err(ControlFailure::Unsupported(method)) => {
            let message = format!("daemon does not support {}", safe(&method));
            Err(if mutation {
                ChatFailure::Refused(format!("not sent: {message}"))
            } else {
                ChatFailure::TooOld(message)
            })
        }
        Err(ControlFailure::DuplicateId | ControlFailure::IdExhausted) => Err(
            ChatFailure::Refused("not sent: control request ID unavailable".into()),
        ),
        Err(error) => Err(if mutation {
            ChatFailure::OutcomeUnknown(safe(&error.to_string()))
        } else {
            ChatFailure::Unavailable(safe(&error.to_string()))
        }),
    }
}
fn decode<T: DeserializeOwned>(value: Value, mutation: bool) -> Result<T, ChatFailure> {
    serde_json::from_value(value).map_err(|_| bad_reply(mutation))
}
fn field<T: DeserializeOwned>(value: &Value, key: &str) -> Result<T, ChatFailure> {
    decode(
        value.get(key).cloned().ok_or_else(|| bad_reply(false))?,
        false,
    )
}

pub(crate) fn decode_page(value: Value) -> Result<ChatPage, ChatFailure> {
    Ok(ChatPage {
        rows: field(&value, "messages")?,
        head: field(&value, "seq")?,
        fence: field(&value, "snapshot_seq")?,
        older: field(&value, "next_cursor")?,
    })
}

/// Enrich only by a unique current root SessionView; session identity is opaque.
fn seat_sessions(seats: &[SeatCard], sessions: &[SessionView]) -> Vec<SeatCard> {
    seats
        .iter()
        .map(|base| {
            let mut card = base.clone();
            let mut roots = sessions.iter().filter(|row| {
                row.parent_session_id.is_none()
                    && row.tmux_pane.is_some()
                    && row.tmux_pane == base.binding.pane
            });
            let root = roots.next().filter(|_| roots.next().is_none());
            card.binding.session_id = root.map(|row| row.id.to_string());
            card.working = root.is_some_and(|row| row.status == SessionStatus::Working);
            if let Some(row) = root {
                card.activity_label = row.status_label.clone();
                card.activity_color = theme::status_color(row.status);
                card.context_percentage = Some(row.context_percentage);
            } else {
                card.activity_label = "session unavailable".into();
                card.activity_color = Color::DarkGray;
                card.context_percentage = None;
            }
            card
        })
        .collect()
}

pub(crate) fn decode_tree(
    value: Value,
    pane: Option<&str>,
    sessions: &[SessionView],
) -> Result<ThreadFacts, ChatFailure> {
    let seq = field(&value, "seq")?;
    let projects = value
        .get("projects")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_reply(false))?;
    let needs = value.get("needs_owner").ok_or_else(|| bad_reply(false))?;
    let items = needs
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_reply(false))?;
    let total: usize = field(needs, "items_total")?;
    let mut waiting_decisions = 0;
    for item in items {
        let kind: String = field(item, "kind")?;
        waiting_decisions += usize::from(matches!(
            kind.as_str(),
            "decision_pending" | "decision_provisional"
        ));
    }
    let mut seats = Vec::new();
    let mut caller_bound = false;
    for project in projects {
        let entries = project
            .get("seats")
            .and_then(Value::as_array)
            .ok_or_else(|| bad_reply(false))?;
        for entry in entries {
            let agent: AgentId = field(entry, "agent_id")?;
            let state: AgentStatus = field(entry, "state")?;
            let seat_pane: Option<String> = field(entry, "pane")?;
            let activity: Option<SessionStatus> = field(entry, "activity")?;
            let context_percentage: Option<f64> = field(entry, "context_percentage")?;
            caller_bound |= pane.is_some() && seat_pane.as_deref() == pane;
            seats.push(SeatCard {
                agent,
                state,
                binding: SeatBinding {
                    pane: seat_pane,
                    session_id: None,
                },
                activity_label: activity
                    .map(|s| s.label().to_owned())
                    .unwrap_or_else(|| "unavailable".into()),
                activity_color: activity.map(theme::status_color).unwrap_or(Color::DarkGray),
                working: false,
                context_percentage,
            });
        }
    }
    seats.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(ThreadFacts {
        tree_seq: seq,
        seats: seat_sessions(&seats, sessions),
        waiting_decisions,
        decisions_truncated: total > items.len(),
        caller_bound,
    })
}

/// No reserved daemon parser is reused or moved. Validate the whole observation,
/// exclude control clients and modes, and never trust sticky focus with focus off.
/// Verified tmux 3.6a/3.8, 2026-11-18: pane_in_mode is 0/1 for normal/copy.
/// Popup-over-chat has identical formats on both; that known limit is in README.
/// Verified tmux 3.8, 2026-11-18: focus-events expands as 0/1 in client rows.
/// An empty expansion checks the option separately for older tmux compatibility.
pub(crate) async fn visibility<C: TmuxClient>(
    tmux: &C,
    pane: Option<&str>,
) -> (bool, Option<String>) {
    let unavailable = || {
        (
            false,
            Some("auto-ack unavailable: reliable tmux focus required".into()),
        )
    };
    let Some(pane) = pane else {
        return unavailable();
    };
    let Ok(clients) = tmux
        .raw_command(
            "list-clients",
            &[
                "-F",
                "#{client_flags}\t#{pane_id}\t#{pane_in_mode}\t#{focus-events}",
            ],
        )
        .await
    else {
        return unavailable();
    };
    if !clients.is_empty() && !clients.ends_with('\n') {
        return unavailable();
    }
    let mut visible = false;
    let mut focus_setting = None;
    for row in clients.lines() {
        let parts: Vec<_> = row.split('\t').collect();
        let [flags, active, mode, focus] = parts.as_slice() else {
            return unavailable();
        };
        if !matches!(*focus, "" | "0" | "1")
            || focus_setting.is_some_and(|setting| setting != *focus)
        {
            return unavailable();
        }
        focus_setting = Some(*focus);
        if mode.is_empty() || !mode.bytes().all(|byte| byte.is_ascii_digit()) {
            return unavailable();
        }
        if !active
            .strip_prefix('%')
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            return unavailable();
        }
        let flags: Vec<_> = flags.split(',').collect();
        let names: Option<Vec<_>> = flags
            .iter()
            .map(|flag| {
                let mut fields = flag.splitn(2, '=');
                let name = fields.next()?;
                let valid = !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && fields.next().is_none_or(|value| {
                        !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
                    });
                valid.then_some(name)
            })
            .collect();
        let Some(names) = names else {
            return unavailable();
        };
        let control = names
            .iter()
            .any(|name| matches!(*name, "control-mode" | "control"));
        visible |= !control && flags.contains(&"focused") && *active == pane && *mode == "0";
    }
    if focus_setting == Some("") {
        let Ok(option) = tmux
            .raw_command("show-options", &["-s", "-v", "focus-events"])
            .await
        else {
            return unavailable();
        };
        if option.trim() != "on" {
            return unavailable();
        }
    } else if focus_setting == Some("0") {
        return unavailable();
    }
    if visible {
        (true, None)
    } else {
        (
            false,
            Some("auto-ack paused: chat is unfocused or in a pane mode".into()),
        )
    }
}

async fn request(
    control: &ControlHandle,
    method: &str,
    params: Value,
    pane: Option<String>,
    mutation: bool,
) -> Result<Value, ChatFailure> {
    let budget = if matches!(method, "org.tree" | "msg.history") {
        READ_BUDGET
    } else {
        MESSAGE_BUDGET
    };
    response_value(
        control.request(method.into(), params, pane, budget).await,
        mutation,
    )
}
async fn typed_request<T: DeserializeOwned>(
    control: &ControlHandle,
    method: &str,
    params: impl Serialize,
    pane: Option<String>,
    mutation: bool,
) -> Result<T, ChatFailure> {
    let params = serde_json::to_value(params)
        .map_err(|_| ChatFailure::Refused("not sent: invalid local parameters".into()))?;
    decode(
        request(control, method, params, pane, mutation).await?,
        mutation,
    )
}
async fn precheck(control: &ControlHandle, pane: Option<String>) -> Result<(), ChatFailure> {
    let result = request(control, "org.tree", json!({}), pane.clone(), false)
        .await
        .and_then(|value| decode_tree(value, pane.as_deref(), &[]));
    match result {
        Ok(facts) if !facts.caller_bound => Ok(()),
        Ok(_) => Err(ChatFailure::Refused(
            "not sent: caller pane is bound".into(),
        )),
        Err(_) => Err(ChatFailure::Refused(
            "not sent: caller binding could not be checked".into(),
        )),
    }
}
fn outcome<T>(result: &Result<T, ChatFailure>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(ChatFailure::OutcomeUnknown(_)) => "unknown",
        Err(_) => "refused_or_unavailable",
    }
}

/// One admitted operation, never retried. The owner aborts and joins these futures.
pub(crate) async fn execute_request(
    effect: ChatEffect,
    control: ControlHandle,
    pane: Option<String>,
    out: mpsc::UnboundedSender<Completion>,
    run: ChatRunId,
) {
    let input = match effect {
        ChatEffect::LoadThreads { token } => {
            let result = request(&control, "org.tree", json!({}), pane, false).await;
            let _ = out.send(Completion::Tree(token, result));
            return;
        }
        ChatEffect::LoadPage { token, query } => ChatInput::PageLoaded(
            token,
            typed_request::<Value>(&control, "msg.history", query, pane, false)
                .await
                .and_then(decode_page),
        ),
        ChatEffect::LoadInbox { token, params } => ChatInput::InboxLoaded(
            token,
            typed_request(&control, "msg.inbox", params, pane, false).await,
        ),
        ChatEffect::Send { token, params } => {
            let result = match precheck(&control, pane.clone()).await {
                Ok(()) => {
                    if out
                        .send(Completion::Input(ChatInput::SendDispatched(token)))
                        .is_err()
                    {
                        return;
                    }
                    typed_request(&control, "msg.send", params, pane, true).await
                }
                Err(error) => Err(error),
            };
            tracing::info!(target:"atm::chat", run_id=%run, token=token.0, outcome=outcome(&result), "chat.send_result");
            ChatInput::SendResult(token, result)
        }
        ChatEffect::Ack { token, params } => {
            let ids: Vec<_> = params.ids.iter().map(ToString::to_string).collect();
            let result: Result<atm_comms::message_types::AckResult, _> =
                match precheck(&control, pane.clone()).await {
                    Ok(()) => typed_request(&control, "msg.ack", params, pane, true).await,
                    Err(error) => Err(error),
                };
            tracing::info!(target:"atm::chat", run_id=%run, token=token.0, ids=?ids,
                count=result.as_ref().map(|r|r.count).unwrap_or(0), outcome=outcome(&result), "chat.ack_result");
            ChatInput::AckResult(token, result)
        }
        _ => return,
    };
    let _ = out.send(Completion::Input(input));
}

pub(crate) fn trace_effect(run: &ChatRunId, effect: &ChatEffect) {
    match effect {
        ChatEffect::Send { token, params } => tracing::info!(target:"atm::chat", run_id=%run,
            token=token.0, target=%params.to, word_count=params.body.split_whitespace().count(), "chat.send_requested"),
        ChatEffect::RecordSend {
            token,
            message_id,
            word_count,
        } => tracing::info!(target:"atm::chat", run_id=%run,
            token=token.0, %message_id, word_count, "chat.send_committed"),
        _ => {}
    }
}
pub(crate) type FrameIds = (BTreeSet<MessageId>, BTreeSet<MessageId>);

pub(crate) fn trace_frame(run: &ChatRunId, report: &FrameReport, previous: &mut Option<FrameIds>) {
    let ids = (
        report.read_ids.iter().cloned().collect(),
        report.visible_ids.iter().cloned().collect(),
    );
    if previous.as_ref() == Some(&ids) {
        return;
    }
    *previous = Some(ids);
    tracing::info!(target:"atm::chat",run_id=%run,generation=report.generation,read_ids=?report.read_ids,"chat.frame");
}

pub(crate) fn trace_presented(
    run: &ChatRunId,
    report: &FrameReport,
    visible: bool,
    previous: &mut Option<BTreeSet<MessageId>>,
) {
    let eligible = if visible {
        report.visible_ids.iter().cloned().collect()
    } else {
        BTreeSet::new()
    };
    if previous.as_ref() == Some(&eligible) {
        return;
    }
    *previous = Some(eligible);
    let ids: Vec<_> = report.visible_ids.iter().map(ToString::to_string).collect();
    tracing::info!(target:"atm::chat",run_id=%run,message_ids=?ids,visible,"chat.presented");
}

pub(crate) fn trace_connection(run: &ChatRunId, online: bool, watch_available: bool) {
    tracing::info!(target:"atm::chat", run_id=%run, online, watch_available, "chat.connection");
}

pub(crate) fn connection_notice(
    run: &ChatRunId,
    notice: ControlNotice,
    cached: &mut Vec<String>,
) -> Option<ChatInput> {
    match notice {
        ControlNotice::Capabilities {
            protocol_version,
            mut methods,
        } => {
            let watch_available = protocol_version.major == ProtocolVersion::CURRENT.major
                && protocol_version.minor >= 1;
            // Internal A/B capability marker, never an RPC method on the wire.
            methods.retain(|method| method != WATCH_CAPABILITY);
            if watch_available {
                methods.push(WATCH_CAPABILITY.into());
            }
            *cached = methods.clone();
            trace_connection(run, true, watch_available);
            Some(ChatInput::Connection {
                online: true,
                methods,
                watch_available,
            })
        }
        ControlNotice::WatchFailed(failure) => {
            // Free-form daemon messages can contain bodies/paths. Preserve the
            // actual failure class, not that untrusted content, in telemetry.
            let reason = match failure {
                ControlFailure::Watch(error) => match error.code.as_str() {
                    "timeout" => "timeout",
                    "unavailable" => "unavailable",
                    _ => "watch_error",
                },
                ControlFailure::Timeout => "timeout",
                ControlFailure::Unsupported(_) => "unsupported",
                ControlFailure::Disconnected => "disconnected",
                ControlFailure::Closed => "closed",
                ControlFailure::DuplicateId => "duplicate_id",
                ControlFailure::IdExhausted => "id_exhausted",
            };
            trace_connection(run, true, false);
            if cached.iter().any(|method| method == WATCH_CAPABILITY) {
                tracing::warn!(target:"atm::chat",run_id=%run,reason,"chat.watch_failed: live updates stopped; retrying");
            } else {
                tracing::warn!(target:"atm::chat",run_id=%run,reason,"chat.watch_failed: watch envelope unavailable");
            }
            Some(ChatInput::Connection {
                online: true,
                methods: cached.clone(),
                watch_available: false,
            })
        }
        _ => None,
    }
}

pub(crate) type SeatObservations = HashMap<AgentId, (SeatBinding, &'static str)>;

pub(crate) fn trace_seats(run: &ChatRunId, seats: &[SeatCard], previous: &mut SeatObservations) {
    use std::hash::{Hash, Hasher};
    previous.retain(|agent, _| seats.iter().any(|seat| &seat.agent == agent));
    for seat in seats {
        let phase = if seat.binding.session_id.is_none() {
            "unknown"
        } else if seat.working {
            "working"
        } else if seat.activity_label == SessionStatus::Idle.label() {
            "idle"
        } else {
            "attention"
        };
        if previous
            .get(&seat.agent)
            .is_some_and(|(binding, old_phase)| binding == &seat.binding && *old_phase == phase)
        {
            continue;
        }
        previous.insert(seat.agent.clone(), (seat.binding.clone(), phase));
        // Diagnostic only, not identity/authentication: compare full bindings
        // above, but never record an opaque session path in the log.
        let mut binding = std::collections::hash_map::DefaultHasher::new();
        seat.binding.pane.hash(&mut binding);
        seat.binding.session_id.hash(&mut binding);
        let binding_key = binding.finish();
        tracing::info!(target:"atm::chat", run_id=%run, seat=%seat.agent, binding_key, phase, "chat.turn_observed");
    }
}

struct Submission {
    target: Address,
    unknown: bool,
}
type FocusFuture<'a> =
    Pin<Box<dyn Future<Output = (FrameReport, (bool, Option<String>))> + Send + 'a>>;
type CopyFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ChatFailure>> + Send + 'a>>;

/// Same controller for the real terminal and the private TestBackend composition.
/// Operation tasks are joined on every ordinary/error exit; JoinSet also aborts
/// on unwind. No task owns a terminal or a borrowed tmux client after this ends.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive<B, C, F>(
    terminal: &mut Terminal<B>,
    config: ChatConfig,
    control: ControlHandle,
    mut events: mpsc::UnboundedReceiver<Event>,
    tmux: &C,
    cancel: CancellationToken,
    run: ChatRunId,
    mut on_frame: F,
) -> anyhow::Result<()>
where
    B: Backend,
    C: TmuxClient,
    F: FnMut(&FrameReport),
{
    let pane = config.caller_pane.clone();
    let mut state = ChatState::new(config);
    let start = Instant::now();
    let mut jobs = JoinSet::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let mut sessions: HashMap<SessionId, SessionView> = HashMap::new();
    let mut seats = Vec::new();
    let mut observed_seats = SeatObservations::new();
    let mut connection_methods = Vec::new();
    let mut submissions: HashMap<OpToken, Submission> = HashMap::new();
    let mut started = false;
    let mut inputs = VecDeque::new();
    let size = terminal.size()?;
    inputs.push_back(ChatInput::Resize {
        width: size.width,
        height: size.height,
    });
    let mut focus: Option<FocusFuture<'_>> = None;
    let mut focus_pending = None;
    let mut copy: Option<CopyFuture<'_>> = None;
    let mut copy_pending = None;
    let mut previous_frame: Option<FrameReport> = None;
    let mut drawn = None;
    let mut traced_presented = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async {
        loop {
            let input = if let Some(input) = inputs.pop_front() { input } else {
                tokio::select! {
                    biased;
                    _=cancel.cancelled()=>ChatInput::Close,
                    completion=done_rx.recv()=>match completion {
                        Some(Completion::Input(input))=>input,
                        Some(Completion::Tree(token,result))=> {
                            let facts=result.and_then(|value|decode_tree(value,pane.as_deref(),&[])).map(|mut facts| {
                                seats=facts.seats.clone();
                                facts.seats=seat_sessions(&seats,&sessions.values().cloned().collect::<Vec<_>>());
                                trace_seats(&run,&facts.seats,&mut observed_seats);
                                facts
                            });
                            ChatInput::ThreadsLoaded(token,facts)
                        }
                        None=>ChatInput::Close,
                    },
                    event=events.recv()=>match event {
                        Some(Event::Key(key))=>ChatInput::Key(key),
                        Some(Event::Resize(width,height))=>ChatInput::Resize {width,height},
                        Some(Event::SessionListReplace(rows))=> {
                            sessions=rows.into_iter().map(|row|(row.id.clone(),row)).collect();
                            if !started {started=true;inputs.push_back(ChatInput::Start);}
                            let cards=seat_sessions(&seats,&sessions.values().cloned().collect::<Vec<_>>());
                            trace_seats(&run,&cards,&mut observed_seats);
                            ChatInput::SessionUpdate(cards)
                        }
                        Some(Event::SessionUpdate(rows))=> {
                            for row in rows {sessions.insert(row.id.clone(),row);}
                            let cards=seat_sessions(&seats,&sessions.values().cloned().collect::<Vec<_>>());
                            trace_seats(&run,&cards,&mut observed_seats);
                            ChatInput::SessionUpdate(cards)
                        }
                        Some(Event::SessionRemoved(id))=> {
                            sessions.retain(|key,_|key.as_str()!=id);
                            let cards=seat_sessions(&seats,&sessions.values().cloned().collect::<Vec<_>>());
                            trace_seats(&run,&cards,&mut observed_seats);
                            ChatInput::SessionUpdate(cards)
                        }
                        Some(Event::Control(ControlNotice::Event(DaemonMessage::Event {seq,kind,..})))=>ChatInput::WatchEvent {seq,kind},
                        Some(Event::Control(notice))=>match connection_notice(&run,notice,&mut connection_methods) {
                            Some(input)=>input,None=>continue,
                        },
                        Some(Event::DaemonDisconnected(_))=> {
                            connection_methods.clear();
                            trace_connection(&run,false,false);
                            ChatInput::Connection {online:false,methods:vec![],watch_available:false}
                        },
                        None=>ChatInput::Close,
                        _=>continue,
                    },
                    output=async {match focus.as_mut() {Some(future)=>future.await,None=>std::future::pending().await}},if focus.is_some()=> {
                        focus=None;
                        let (report,(visible,hint))=output;
                        trace_presented(&run,&report,visible,&mut traced_presented);
                        ChatInput::Presented {generation:report.generation,visible_ids:report.visible_ids,visible,hint}
                    },
                    output=async {match copy.as_mut() {Some(future)=>future.await,None=>std::future::pending().await}},if copy.is_some()=> {
                        copy=None;ChatInput::CopyResult(output)
                    },
                    result=jobs.join_next(),if !jobs.is_empty()=> {
                        if let Some(Err(_))=result {anyhow::bail!("chat request task failed")}
                        continue;
                    },
                    _=tick.tick()=>ChatInput::Tick,
                }
            };
            let force_focus=matches!(input,ChatInput::Tick | ChatInput::Resize {..});
            let unlock=if matches!(&input,ChatInput::Key(key) if key.code==KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                submissions.iter().find_map(|(token,send)| (send.unknown && previous_frame.as_ref().is_some_and(|f|f.thread==ThreadKey::Direct(send.target.clone()))).then_some(*token))
            } else {None};
            if let ChatInput::SendResult(token,result)=&input {
                match result {
                    Err(ChatFailure::OutcomeUnknown(_))=>if let Some(send)=submissions.get_mut(token) {send.unknown=true},
                    Err(ChatFailure::Refused(_))=>{submissions.remove(token);},
                    _=>{},
                }
            }
            let now=ChatTime {utc:Utc::now(),elapsed:start.elapsed()};
            for effect in state.reduce(input,now) {
                trace_effect(&run,&effect);
                match effect {
                    ChatEffect::Watch {topics,cursor}=>if let Err(failure)=control.watch(topics,cursor) {
                        if let Some(input)=connection_notice(&run,ControlNotice::WatchFailed(failure),&mut connection_methods) {
                            inputs.push_back(input);
                        }
                    },
                    ChatEffect::Unwatch=>{let _=control.unwatch();},
                    ChatEffect::Quit {error}=>return match error {Some(error)=>Err(anyhow::Error::new(error)),None=>Ok(())},
                    ChatEffect::Copy {body}=>copy_pending=Some(body),
                    ChatEffect::RecordSend {token,..}=>{submissions.remove(&token);},
                    effect=> {
                        if let ChatEffect::Send {token,params}=&effect {
                            submissions.insert(*token,Submission {target:params.to.clone(),unknown:false});
                        }
                        jobs.spawn(execute_request(effect,control.clone(),pane.clone(),done_tx.clone(),run.clone()).with_current_subscriber());
                    }
                }
            }
            let size=terminal.size()?;
            let key=(state.generation(),size.width,size.height);
            let report=if drawn==Some(key) {
                previous_frame.clone().ok_or_else(||anyhow::anyhow!("chat has no cached frame"))?
            } else {
                let mut report=None;
                terminal.draw(|frame|report=Some(atm_chat::render(frame,&state)))?;
                let report=report.ok_or_else(||anyhow::anyhow!("chat draw returned no frame"))?;
                on_frame(&report);
                drawn=Some(key);
                report
            };
            if report.can_compose {
                if let Some(token)=unlock {
                    submissions.remove(&token);
                    tracing::info!(target:"atm::chat",run_id=%run,token=token.0,outcome_unknown=true,"chat.send_unlocked");
                }
            }
            if force_focus || previous_frame.as_ref()!=Some(&report) {focus_pending=Some(report.clone());}
            previous_frame=Some(report);
            if focus.is_none() {
                if let Some(report)=focus_pending.take() {
                    let pane=pane.clone();
                    focus=Some(Box::pin(async move {let result=visibility(tmux,pane.as_deref()).await;(report,result)}));
                }
            }
            if copy.is_none() {
                if let Some(body)=copy_pending.take() {
                    let available=pane.is_some();
                    copy=Some(Box::pin(async move {
                        if !available {return Err(ChatFailure::Unavailable("outside tmux".into()))}
                        tmux.load_buffer(&body).await.map_err(|_|ChatFailure::Unavailable("tmux buffer unavailable".into()))
                    }));
                }
            }
        }
    }.await;
    let _ = control.unwatch();
    jobs.abort_all();
    while let Some(result) = jobs.join_next().await {
        if result.is_err() {
            tracing::debug!(target:"atm::chat",run_id=%run,"chat.request_stopped");
        }
    }
    result
}
