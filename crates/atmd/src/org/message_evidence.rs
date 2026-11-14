//! Exact receipts and receive-order activity, never a prompt transcript.
//!
//! The connection tap must first apply the translated event and prove original
//! parent/session/pane eligibility. Registry Ok alone is not that proof. In
//! particular, never repair an old event with a new pane occupant's identity.
//! Register/arm/observe share one short synchronous lock. Coalesced wakeups do
//! not discard retained facts; poisoned/exhausted retention fails closed.
//! Earlier scopes survive retries and final failure for late exact receipts.

use super::messages::{MessageClock, MessageError};
use crate::control::reservation::PaneId;
use atm_core::{Harness, LifecycleEvent, SessionId};
use atm_org::{AgentId, DeliveryReceipt, MessageId};
use chrono::{DateTime, Utc};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::Notify;

/// Scope recorded after Attempted is applied, before paste can be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptScope {
    pub message_id: MessageId,
    /// Org log sequence of the applied DeliveryAttempted, not its attempt number.
    pub attempt_seq: u64,
    pub recipient: AgentId,
    pub pane: PaneId,
    pub harness: Harness,
    /// Some requires this exact session and pane, not a later binding.
    pub session_id: Option<SessionId>,
    /// A lazy first prompt needs an eligible parent outside this baseline on the
    /// same owned pane/harness. The tap is responsible for parent eligibility.
    pub excluded_sessions: HashSet<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptEvidence {
    pub receipt: Option<DeliveryReceipt>,
    pub probably_landed: bool,
}

const MAX_MESSAGES: usize = 4096;
const MAX_SCOPES: usize = 16384;
const MAX_SESSIONS: usize = 4096;

#[derive(Debug)]
struct Scope {
    identity: AttemptScope,
    /// Freeze the first eligible lazy-session observation before pane reuse.
    lazy_session: Option<SessionId>,
}
#[derive(Debug)]
struct Entry {
    expected: String,
    scopes: Vec<Scope>,
    receipt: Option<DeliveryReceipt>,
    probably_landed: bool,
}

#[derive(Debug, Default)]
struct State {
    disabled: Option<String>,
    messages: HashMap<MessageId, Entry>,
    // Accepted receive time and original pane, never poll timestamps.
    sessions: HashMap<SessionId, (DateTime<Utc>, Option<PaneId>)>,
    scopes: usize,
}

/// Build once per daemon and clone to share retention and the clock origin.
#[derive(Debug, Clone)]
pub struct MessageEvidence {
    clock: MessageClock,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
}

impl MessageEvidence {
    #[must_use]
    pub fn new(clock: MessageClock) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State::default())),
            wake: Arc::new(Notify::new()),
        }
    }

    #[must_use]
    pub fn disabled(clock: MessageClock, reason: impl Into<String>) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State {
                disabled: Some(reason.into()),
                ..State::default()
            })),
            wake: Arc::new(Notify::new()),
        }
    }

    #[must_use]
    pub fn clock(&self) -> &MessageClock {
        &self.clock
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, MessageError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MessageError::Unavailable("message evidence lock poisoned".into()))?;
        if let Some(reason) = &state.disabled {
            return Err(MessageError::Unavailable(reason.clone()));
        }
        Ok(state)
    }

    fn disable(&self, state: &mut State, reason: &str) -> MessageError {
        state.disabled = Some(reason.into());
        self.wake.notify_one();
        MessageError::Unavailable(reason.into())
    }

    /// Register the one shared reference, never the body or a token substring.
    /// A wrong text or a full store refuses this caller only: nothing is stored
    /// or evicted, and no input may follow a failed registration.
    pub fn register(&self, id: MessageId, expected_prompt: String) -> Result<(), MessageError> {
        let mut state = self.state()?;
        if expected_prompt != super::message_delivery::inbox_reference(id) {
            return Err(MessageError::Refused(
                "message reference does not match its ID".into(),
            ));
        }
        if state.messages.contains_key(&id) {
            return Ok(());
        }
        if state.messages.len() >= MAX_MESSAGES {
            return Err(MessageError::Unavailable(
                "message evidence reference capacity exhausted".into(),
            ));
        }
        state.messages.insert(
            id,
            Entry {
                expected: expected_prompt,
                scopes: Vec::new(),
                receipt: None,
                probably_landed: false,
            },
        );
        Ok(())
    }

    /// A stale, inconsistent or ineligible scope, or a full store, refuses this
    /// caller only and leaves every retained scope, latch and receipt as it was.
    pub fn arm(&self, scope: AttemptScope) -> Result<(), MessageError> {
        let mut state = self.state()?;
        let entry = state
            .messages
            .get(&scope.message_id)
            .ok_or_else(|| MessageError::Unavailable("message reference not registered".into()))?;
        if let Some(last) = entry.scopes.last() {
            if last.identity == scope {
                return Ok(());
            }
            if scope.attempt_seq <= last.identity.attempt_seq
                || scope.recipient != last.identity.recipient
            {
                return Err(MessageError::Refused(
                    "message attempt scope is inconsistent".into(),
                ));
            }
        }
        if scope.attempt_seq == 0
            || scope
                .session_id
                .as_ref()
                .is_some_and(|id| id.is_pending() || scope.excluded_sessions.contains(id))
        {
            return Err(MessageError::Refused(
                "message attempt scope is ineligible".into(),
            ));
        }
        if state.scopes >= MAX_SCOPES || scope.excluded_sessions.len() > MAX_SESSIONS {
            return Err(MessageError::Unavailable(
                "message evidence scope capacity exhausted".into(),
            ));
        }
        if let Some(entry) = state.messages.get_mut(&scope.message_id) {
            entry.scopes.push(Scope {
                identity: scope,
                lazy_session: None,
            });
            entry.probably_landed = false;
            state.scopes += 1;
        }
        self.wake.notify_one();
        Ok(())
    }

    /// An unknown or acknowledged ID is unavailable, as in `snapshot`: Ok(None)
    /// could incorrectly authorize an automatic retry.
    pub fn receipt(&self, id: MessageId) -> Result<Option<DeliveryReceipt>, MessageError> {
        self.snapshot(id).map(|facts| facts.receipt)
    }

    pub fn snapshot(&self, id: MessageId) -> Result<AttemptEvidence, MessageError> {
        let state = self.state()?;
        let entry = state
            .messages
            .get(&id)
            .ok_or_else(|| MessageError::Unavailable("message reference not registered".into()))?;
        Ok(AttemptEvidence {
            receipt: entry.receipt.clone(),
            probably_landed: entry.probably_landed,
        })
    }

    /// Call only after Delivered is committed and history.folded has completed.
    /// Failed append/reply loss must leave the receipt and scopes retained.
    pub fn acknowledge(
        &self,
        id: MessageId,
        receipt: &DeliveryReceipt,
    ) -> Result<(), MessageError> {
        let mut state = self.state()?;
        if let Some(entry) = state.messages.get(&id) {
            if entry.receipt.as_ref() != Some(receipt) {
                return Err(MessageError::Conflict(
                    "message receipt does not match retained evidence".into(),
                ));
            }
            state.scopes = state.scopes.saturating_sub(entry.scopes.len());
            state.messages.remove(&id);
        }
        Ok(())
    }

    /// Only the tap's accepted original parent identity may enter here.
    pub fn observe(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: Harness,
        event: &LifecycleEvent,
    ) -> Result<(), MessageError> {
        let prompt = match event {
            LifecycleEvent::PromptSubmit { prompt } => prompt.as_deref(),
            _ => None,
        };
        self.observe_inner(session, pane, Some(harness), prompt)
    }

    /// The tap forwards retained receive-time provenance through the same short
    /// ordering lock. A coalesced/newer observation may subsume an earlier one;
    /// never substitute the later drain time for its receive timestamp.
    pub(crate) fn record_activity(
        &self,
        session: &SessionId,
        history: &super::message_history::MessageHistoryHandle,
    ) -> Result<(), MessageError> {
        let mut state = self.state()?;
        if let Some((at, pane)) = state.sessions.get(session) {
            let update = super::message_history::HistoryUpdate::Activity {
                at: *at,
                session_id: session.clone(),
                pane: pane.clone(),
            };
            if history.record(update, *at).is_err() {
                return Err(self.disable(&mut state, "message activity history unavailable"));
            }
        }
        Ok(())
    }

    pub(crate) fn invalidate(&self, reason: &str) -> MessageError {
        match self.state() {
            Ok(mut state) => self.disable(&mut state, reason),
            Err(error) => error,
        }
    }

    /// Genuine accepted status-line activity cannot create a receipt or bind a
    /// lazy scope (it lacks that scope's positive harness identity).
    pub fn observe_activity(
        &self,
        session: &SessionId,
        pane: Option<&str>,
    ) -> Result<(), MessageError> {
        self.observe_inner(session, pane, None, None)
    }

    fn observe_inner(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: Option<Harness>,
        prompt: Option<&str>,
    ) -> Result<(), MessageError> {
        let mut state = self.state()?;
        if session.is_pending() {
            return Ok(());
        }
        let pane = pane.map(PaneId::parse).transpose()?;
        let at = self
            .clock
            .now()
            .map_err(|_| self.disable(&mut state, "message evidence clock unavailable"))?;
        if !state.sessions.contains_key(session) && state.sessions.len() >= MAX_SESSIONS {
            return Err(self.disable(&mut state, "message evidence activity capacity exhausted"));
        }
        state.sessions.insert(session.clone(), (at, pane.clone()));
        for entry in state.messages.values_mut() {
            let last_seq = entry.scopes.last().map(|s| s.identity.attempt_seq);
            for scope in &mut entry.scopes {
                let identity = &scope.identity;
                if harness.is_some_and(|h| h != identity.harness)
                    || pane.as_ref().is_some_and(|p| p != &identity.pane)
                    || identity.excluded_sessions.contains(session)
                {
                    continue;
                }
                let bound = identity.session_id.as_ref().or(scope.lazy_session.as_ref());
                if let Some(bound) = bound {
                    if bound != session {
                        continue;
                    }
                } else {
                    if pane.is_none() || harness.is_none() {
                        continue;
                    }
                    scope.lazy_session = Some(session.clone());
                }
                if Some(identity.attempt_seq) == last_seq {
                    entry.probably_landed = true;
                }
                if pane.is_some()
                    && prompt == Some(entry.expected.as_str())
                    && entry.receipt.is_none()
                {
                    entry.receipt = Some(DeliveryReceipt {
                        session_id: session.clone(),
                        confirmation: "prompt_submit_exact_reference".into(),
                    });
                }
            }
        }
        self.wake.notify_one();
        Ok(())
    }
}

#[cfg(test)]
#[path = "message_evidence_tests.rs"]
mod tests;
