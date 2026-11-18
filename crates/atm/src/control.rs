//! Pure bookkeeping for an optional control-plane stream on the session connection.
//!
//! Cursors belong to one retained daemon event stream, not a socket or timestamp.
//! No stream epoch is available in protocol 1.1. Replacing that history requires
//! an explicit new watch; reconnect alone never resets its cursor.

use std::collections::BTreeMap;

use crate::client::HANDSHAKE_TIMEOUT;
use atm_protocol::{
    ClientMessage, DaemonMessage, MessageType, ProtocolVersion, ResponseError, ResponsePayload,
};
use tokio::time::Instant;

/// A local control failure. A timeout does not imply a mutation had no effect.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ControlFailure {
    #[error("daemon does not support {0}")]
    Unsupported(String),
    #[error("daemon request timed out; its outcome may be unknown")]
    Timeout,
    #[error("daemon disconnected; request was not retried")]
    Disconnected,
    #[error("control channel closed")]
    Closed,
    #[error("duplicate request id")]
    DuplicateId,
    #[error("control request ids exhausted")]
    IdExhausted,
    #[error("{message}", message = .0.message)]
    Watch(ResponseError),
}

/// Optional control-plane updates. They do not change the Sessions view.
#[derive(Debug, Clone)]
pub enum ControlNotice {
    /// Capabilities of the current connection, emitted only after opting in.
    Capabilities {
        protocol_version: ProtocolVersion,
        methods: Vec<String>,
    },
    /// A new watch event, retaining the protocol's opaque payload unchanged.
    Event(DaemonMessage),
    /// This watch is unavailable until explicitly requested or reconnected.
    WatchFailed(ControlFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RequestId(pub(crate) u64);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tui-{}", self.0)
    }
}

#[derive(Debug)]
pub(crate) enum ControlInput {
    Connected {
        protocol_version: ProtocolVersion,
        methods: Vec<String>,
    },
    Disconnected,
    Request {
        id: RequestId,
        message: ClientMessage,
        deadline: Instant,
    },
    Watch {
        topics: Vec<String>,
        cursor: u64,
        barrier: RequestId,
    },
    Unwatch,
    Message(DaemonMessage),
    Delivered(u64),
    Tick,
}

#[derive(Debug)]
pub(crate) enum ControlEffect {
    Send(ClientMessage, Option<Instant>),
    Complete(RequestId, Result<ResponsePayload, ControlFailure>),
    Notice(ControlNotice),
}

#[derive(Debug)]
struct Watch {
    topics: Vec<String>,
    cursor: u64,
}

#[derive(Debug, Default)]
enum WatchPhase {
    #[default]
    Off,
    Following,
    Barrier {
        seq: u64,
        deadline: Instant,
    },
}

#[derive(Debug, Default)]
pub(crate) struct ControlState {
    protocol: Option<ProtocolVersion>,
    methods: Vec<String>,
    opted_in: bool,
    watch: Option<Watch>,
    phase: WatchPhase,
    watched: bool,
    pending: BTreeMap<RequestId, Instant>,
}

impl ControlState {
    pub(crate) fn reduce(&mut self, input: ControlInput, now: Instant) -> Vec<ControlEffect> {
        let mut effects = Vec::new();
        self.pending.retain(|id, deadline| {
            if *deadline <= now {
                effects.push(ControlEffect::Complete(*id, Err(ControlFailure::Timeout)));
                false
            } else {
                true
            }
        });
        if matches!(self.phase, WatchPhase::Barrier { deadline, .. } if deadline <= now) {
            self.phase = WatchPhase::Off;
            effects.push(ControlEffect::Notice(ControlNotice::WatchFailed(
                ControlFailure::Timeout,
            )));
        }
        if !self.opted_in
            && matches!(
                &input,
                ControlInput::Request { .. } | ControlInput::Watch { .. } | ControlInput::Unwatch
            )
        {
            self.opted_in = true;
            self.capabilities(&mut effects);
        }
        match input {
            ControlInput::Connected {
                protocol_version,
                methods,
            } => {
                self.protocol = Some(protocol_version);
                self.methods = methods;
                self.phase = WatchPhase::Off;
                self.watched = false;
                if self.opted_in {
                    self.capabilities(&mut effects);
                }
                self.follow(&mut effects);
            }
            ControlInput::Disconnected => {
                for id in std::mem::take(&mut self.pending).into_keys() {
                    effects.push(ControlEffect::Complete(
                        id,
                        Err(ControlFailure::Disconnected),
                    ));
                }
                self.protocol = None;
                self.methods.clear();
                self.phase = WatchPhase::Off;
                self.watched = false;
            }
            ControlInput::Request {
                id,
                message,
                deadline,
            } => {
                let method = match &message.message {
                    MessageType::Request { method, .. } => method.as_str(),
                    _ => "invalid request envelope",
                };
                let failure = if deadline <= now {
                    Some(ControlFailure::Timeout)
                } else if self.pending.contains_key(&id) {
                    Some(ControlFailure::DuplicateId)
                } else if self.protocol.is_none() {
                    Some(ControlFailure::Disconnected)
                } else if !self.supports_control() || !self.methods.iter().any(|m| m == method) {
                    Some(ControlFailure::Unsupported(method.into()))
                } else {
                    None
                };
                if let Some(failure) = failure {
                    effects.push(ControlEffect::Complete(id, Err(failure)));
                } else {
                    self.pending.insert(id, deadline);
                    effects.push(ControlEffect::Send(message, Some(deadline)));
                }
            }
            ControlInput::Watch {
                topics,
                cursor,
                barrier,
            } => {
                self.watch = Some(Watch { topics, cursor });
                self.phase = WatchPhase::Off;
                if self.watched && self.supports_control() {
                    if let Some(deadline) = now.checked_add(HANDSHAKE_TIMEOUT) {
                        self.phase = WatchPhase::Barrier {
                            seq: barrier.0,
                            deadline,
                        };
                        effects.push(ControlEffect::Send(ClientMessage::unwatch(), None));
                        effects.push(ControlEffect::Send(
                            ClientMessage::ping(barrier.0),
                            Some(deadline),
                        ));
                    } else {
                        effects.push(ControlEffect::Notice(ControlNotice::WatchFailed(
                            ControlFailure::Timeout,
                        )));
                    }
                } else {
                    self.follow(&mut effects);
                }
            }
            ControlInput::Unwatch => {
                self.watch = None;
                self.phase = WatchPhase::Off;
                if self.watched && self.supports_control() {
                    effects.push(ControlEffect::Send(ClientMessage::unwatch(), None));
                }
            }
            ControlInput::Delivered(seq) => {
                if matches!(self.phase, WatchPhase::Following) {
                    if let Some(watch) = &mut self.watch {
                        watch.cursor = watch.cursor.max(seq);
                    }
                }
            }
            ControlInput::Message(DaemonMessage::Response { id, payload }) => {
                if let Some(request_id) = id
                    .strip_prefix("tui-")
                    .and_then(|id| id.parse().ok())
                    .map(RequestId)
                {
                    if id == request_id.to_string() && self.pending.remove(&request_id).is_some() {
                        effects.push(ControlEffect::Complete(request_id, Ok(payload)));
                    }
                }
            }
            ControlInput::Message(DaemonMessage::Pong { seq }) => {
                if matches!(self.phase, WatchPhase::Barrier { seq: expected, .. } if seq == expected)
                {
                    self.phase = WatchPhase::Off;
                    self.follow(&mut effects);
                }
            }
            ControlInput::Message(message @ DaemonMessage::Event { .. }) => {
                if let (
                    WatchPhase::Following,
                    Some(watch),
                    DaemonMessage::Event { seq, kind, .. },
                ) = (&self.phase, &self.watch, &message)
                {
                    if *seq > watch.cursor
                        && (watch.topics.is_empty()
                            || watch.topics.iter().any(|p| kind.starts_with(p)))
                    {
                        effects.push(ControlEffect::Notice(ControlNotice::Event(message)));
                    }
                }
            }
            ControlInput::Message(DaemonMessage::Error {
                message,
                code: Some(code),
            }) if !matches!(self.phase, WatchPhase::Off)
                && matches!(code.as_str(), "unavailable" | "timeout") =>
            {
                self.phase = WatchPhase::Off;
                effects.push(ControlEffect::Send(ClientMessage::unwatch(), None));
                effects.push(ControlEffect::Notice(ControlNotice::WatchFailed(
                    ControlFailure::Watch(ResponseError { code, message }),
                )));
            }
            ControlInput::Message(_) | ControlInput::Tick => {}
        }
        effects
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .copied()
            .chain(match self.phase {
                WatchPhase::Barrier { deadline, .. } => Some(deadline),
                _ => None,
            })
            .min()
    }

    fn supports_control(&self) -> bool {
        self.protocol
            .as_ref()
            .is_some_and(|v| v.major == ProtocolVersion::CURRENT.major && v.minor >= 1)
    }

    fn capabilities(&self, effects: &mut Vec<ControlEffect>) {
        if let Some(protocol_version) = &self.protocol {
            effects.push(ControlEffect::Notice(ControlNotice::Capabilities {
                protocol_version: protocol_version.clone(),
                methods: self.methods.clone(),
            }));
        }
    }

    fn follow(&mut self, effects: &mut Vec<ControlEffect>) {
        let Some(watch) = &self.watch else { return };
        if self.protocol.is_none() {
            return;
        }
        if self.supports_control() {
            effects.push(ControlEffect::Send(
                ClientMessage::watch(watch.topics.clone(), watch.cursor),
                None,
            ));
            self.phase = WatchPhase::Following;
            self.watched = true;
        } else {
            effects.push(ControlEffect::Notice(ControlNotice::WatchFailed(
                ControlFailure::Unsupported("watch".into()),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_protocol::MessageType;
    use serde_json::json;
    use std::time::Duration;

    fn connect(state: &mut ControlState, now: Instant, minor: u16, methods: &[&str]) {
        state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::new(1, minor),
                methods: methods.iter().map(|s| (*s).into()).collect(),
            },
            now,
        );
    }

    fn request(id: u64, deadline: Instant) -> ControlInput {
        ControlInput::Request {
            id: RequestId(id),
            message: ClientMessage::request(
                format!("tui-{id}"),
                "task.list".into(),
                json!({}),
                Some("%7".into()),
            ),
            deadline,
        }
    }

    fn watch(cursor: u64, barrier: u64) -> ControlInput {
        ControlInput::Watch {
            topics: vec!["task.".into()],
            cursor,
            barrier: RequestId(barrier),
        }
    }

    fn event(seq: u64, kind: &str) -> DaemonMessage {
        DaemonMessage::event(
            "task".into(),
            seq,
            "2026-01-01T00:00:00Z".into(),
            kind.into(),
            json!({"future": true}),
        )
    }

    fn sent(effects: &[ControlEffect]) -> Vec<&MessageType> {
        effects
            .iter()
            .filter_map(|e| match e {
                ControlEffect::Send(message, _) => Some(&message.message),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn control_gates_requests_and_watch_separately() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 0, &[]);
        let refused = state.reduce(request(1, now + Duration::from_secs(30)), now);
        assert!(refused.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(1), Err(ControlFailure::Unsupported(_)))
        )));
        assert!(sent(&refused).is_empty());
        assert!(sent(&state.reduce(watch(0, 10), now)).is_empty());
        connect(&mut state, now, 1, &[]);
        let resumed = state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec![],
            },
            now,
        );
        assert!(matches!(
            sent(&resumed).as_slice(),
            [MessageType::Watch { cursor: 0, .. }]
        ));
        assert!(sent(&state.reduce(request(2, now + Duration::from_secs(30)), now)).is_empty());
    }

    #[test]
    fn control_cursor_advances_only_after_delivery_and_drops_overlap() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &[]);
        state.reduce(watch(0, 10), now);
        let mut delivered = Vec::new();
        for seq in [40, 42, 42, 41, 47] {
            let effects = state.reduce(ControlInput::Message(event(seq, "task.future")), now);
            if effects
                .iter()
                .any(|e| matches!(e, ControlEffect::Notice(ControlNotice::Event(_))))
            {
                delivered.push(seq);
                state.reduce(ControlInput::Delivered(seq), now);
            }
        }
        assert_eq!(delivered, vec![40, 42, 47]);
        state.reduce(ControlInput::Disconnected, now);
        let effects = state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec![],
            },
            now,
        );
        assert!(matches!(
            sent(&effects).as_slice(),
            [MessageType::Watch { cursor: 47, .. }]
        ));

        let mut uncommitted = ControlState::default();
        connect(&mut uncommitted, now, 1, &[]);
        uncommitted.reduce(watch(5, 11), now);
        uncommitted.reduce(ControlInput::Message(event(99, "task.future")), now);
        uncommitted.reduce(ControlInput::Disconnected, now);
        let effects = uncommitted.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec![],
            },
            now,
        );
        assert!(matches!(
            sent(&effects).as_slice(),
            [MessageType::Watch { cursor: 5, .. }]
        ));
    }

    #[test]
    fn control_rewatch_drains_old_scope_before_resetting_cursor() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &[]);
        state.reduce(watch(80, 10), now);
        let effects = state.reduce(watch(0, 11), now);
        assert!(matches!(
            sent(&effects).as_slice(),
            [MessageType::Unwatch, MessageType::Ping { seq: 11 }]
        ));
        assert!(state
            .reduce(ControlInput::Message(event(90, "task.old")), now)
            .is_empty());
        assert!(state
            .reduce(ControlInput::Message(DaemonMessage::pong(10)), now)
            .is_empty());
        let effects = state.reduce(ControlInput::Message(DaemonMessage::pong(11)), now);
        assert!(matches!(
            sent(&effects).as_slice(),
            [MessageType::Watch { cursor: 0, .. }]
        ));
        let effects = state.reduce(ControlInput::Message(event(1, "task.new")), now);
        assert!(effects.iter().any(|e| matches!(e, ControlEffect::Notice(ControlNotice::Event(DaemonMessage::Event { seq: 1, body, ts, .. })) if body == &json!({"future":true}) && ts == "2026-01-01T00:00:00Z")));
    }

    #[test]
    fn control_barrier_expires_but_quiet_watch_does_not() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &[]);
        state.reduce(watch(0, 10), now);
        assert!(state.next_deadline().is_none());
        assert!(state
            .reduce(ControlInput::Tick, now + Duration::from_secs(3600))
            .is_empty());
        state.reduce(watch(0, 11), now);
        assert_eq!(state.next_deadline(), Some(now + Duration::from_secs(10)));
        let effects = state.reduce(ControlInput::Tick, now + Duration::from_secs(10));
        assert!(effects.iter().any(|e| matches!(
            e,
            ControlEffect::Notice(ControlNotice::WatchFailed(ControlFailure::Timeout))
        )));
        assert!(state
            .reduce(
                ControlInput::Message(DaemonMessage::pong(11)),
                now + Duration::from_secs(11)
            )
            .is_empty());
    }

    #[test]
    fn control_unwatch_does_not_resume_or_accept_buffered_events() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &[]);
        state.reduce(watch(7, 10), now);
        assert!(matches!(
            sent(&state.reduce(ControlInput::Unwatch, now)).as_slice(),
            [MessageType::Unwatch]
        ));
        assert!(state
            .reduce(ControlInput::Message(event(8, "task.old")), now)
            .is_empty());
        state.reduce(ControlInput::Disconnected, now);
        let effects = state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec![],
            },
            now,
        );
        assert!(sent(&effects).is_empty());
    }

    #[test]
    fn control_request_deadlines_cover_admission_response_and_disconnect() {
        let now = Instant::now();
        let end = now + Duration::from_secs(30);
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &["task.list"]);
        let effects = state.reduce(request(1, now), now);
        assert!(effects.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(1), Err(ControlFailure::Timeout))
        )));
        assert!(sent(&effects).is_empty());
        let effects = state.reduce(request(2, end), now);
        assert!(
            matches!(sent(&effects).as_slice(), [MessageType::Request { id, from_pane: Some(pane), .. }] if id == "tui-2" && pane == "%7")
        );
        assert_eq!(state.next_deadline(), Some(end));
        let duplicate = state.reduce(request(2, end), now);
        assert!(duplicate.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(2), Err(ControlFailure::DuplicateId))
        )));
        assert!(sent(&duplicate).is_empty());
        let late = state.reduce(
            ControlInput::Message(DaemonMessage::response_ok("tui-2".into(), json!(null))),
            end,
        );
        assert!(late.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(2), Err(ControlFailure::Timeout))
        )));
        state.reduce(request(3, end), now);
        let gone = state.reduce(ControlInput::Disconnected, now);
        assert!(gone.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(3), Err(ControlFailure::Disconnected))
        )));
        let reconnect = state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec!["task.list".into()],
            },
            now,
        );
        assert!(sent(&reconnect).is_empty());
        assert!(state
            .reduce(
                ControlInput::Message(DaemonMessage::response_ok("tui-3".into(), json!(1))),
                now
            )
            .is_empty());
    }

    #[test]
    fn control_correlates_out_of_order_null_and_error_payloads() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &["task.list"]);
        state.reduce(request(1, now + Duration::from_secs(30)), now);
        state.reduce(request(2, now + Duration::from_secs(30)), now);
        assert!(state
            .reduce(
                ControlInput::Message(DaemonMessage::response_ok("other".into(), json!(9))),
                now
            )
            .is_empty());
        let effects = state.reduce(
            ControlInput::Message(DaemonMessage::response_err("tui-2".into(), "refused", "no")),
            now,
        );
        assert!(
            matches!(effects.as_slice(), [ControlEffect::Complete(RequestId(2), Ok(ResponsePayload::Error(error)))] if error.code == "refused")
        );
        let effects = state.reduce(
            ControlInput::Message(DaemonMessage::response_ok("tui-1".into(), json!(null))),
            now,
        );
        assert!(
            matches!(effects.as_slice(), [ControlEffect::Complete(RequestId(1), Ok(ResponsePayload::Result(value)))] if value.is_null())
        );
    }

    #[test]
    fn control_response_id_requires_exact_wire_identity() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &["task.list"]);
        state.reduce(request(1, now + Duration::from_secs(30)), now);
        for id in ["tui-01", "tui-+1", "tui-0001"] {
            let effects = state.reduce(
                ControlInput::Message(DaemonMessage::response_ok(id.into(), json!("wrong call"))),
                now,
            );
            assert!(
                effects.is_empty(),
                "non-matching wire id {id} completed our call"
            );
        }
        let effects = state.reduce(
            ControlInput::Message(DaemonMessage::response_ok("tui-1".into(), json!(null))),
            now,
        );
        assert!(
            matches!(effects.as_slice(), [ControlEffect::Complete(RequestId(1), Ok(ResponsePayload::Result(value)))] if value.is_null())
        );
    }

    #[test]
    fn control_rechecks_capabilities_on_reconnect_without_legacy_notices() {
        let now = Instant::now();
        let mut state = ControlState::default();
        assert!(state
            .reduce(
                ControlInput::Connected {
                    protocol_version: ProtocolVersion::CURRENT,
                    methods: vec!["task.list".into()]
                },
                now
            )
            .is_empty());
        state.reduce(request(1, now + Duration::from_secs(30)), now);
        state.reduce(ControlInput::Disconnected, now);
        connect(&mut state, now, 1, &["other"]);
        let effects = state.reduce(request(2, now + Duration::from_secs(30)), now);
        assert!(sent(&effects).is_empty());
        assert!(effects.iter().any(|e| matches!(
            e,
            ControlEffect::Complete(RequestId(2), Err(ControlFailure::Unsupported(_)))
        )));
    }

    #[test]
    fn control_watch_failure_preserves_desire_until_reconnect() {
        let now = Instant::now();
        let mut state = ControlState::default();
        connect(&mut state, now, 1, &[]);
        state.reduce(watch(10, 1), now);
        let effects = state.reduce(
            ControlInput::Message(DaemonMessage::error_with_code(
                "reader failed",
                "unavailable",
            )),
            now,
        );
        assert!(matches!(sent(&effects).as_slice(), [MessageType::Unwatch]));
        assert!(effects
            .iter()
            .any(|e| matches!(e, ControlEffect::Notice(ControlNotice::WatchFailed(_)))));
        assert!(state
            .reduce(ControlInput::Message(event(11, "task.new")), now)
            .is_empty());
        state.reduce(ControlInput::Disconnected, now);
        let effects = state.reduce(
            ControlInput::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                methods: vec![],
            },
            now,
        );
        assert!(matches!(
            sent(&effects).as_slice(),
            [MessageType::Watch { cursor: 10, .. }]
        ));
    }
}
