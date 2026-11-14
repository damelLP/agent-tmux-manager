//! Exact receipts and receive-order activity, never a prompt transcript.
//!
//! The connection tap must first apply the translated event and prove original
//! parent/session/pane eligibility. Registry Ok alone is not that proof. In
//! particular, never repair an old event with a new pane occupant's identity.
//!
//! A parent event whose proof is missing (the apply failed, the registry
//! stalled or closed, the row is absent or ambiguous) is never dropped. It
//! enters as unproven and reaches exactly the scopes the proven event would
//! have reached, judged by the event's own identity. There it can only hold:
//! it latches probably-landed, and never binds a lazy scope or confirms. So a
//! lost proof holds the messages that event could affect, until a new arm or
//! the owner's retry, and leaves every other message alone.
//! Register/arm/observe share one short synchronous lock. Coalesced wakeups do
//! not discard retained facts; poisoned/exhausted retention fails closed.
//! Earlier scopes survive retries and final failure for late exact receipts.
//!
//! Lock order: evidence, then history. The tap stamps an event's receive time
//! and forwards its Activity to history under one evidence lock acquisition, so
//! each event keeps its own time and nothing is retained per session. History
//! never calls evidence.
//!
//! Wakes are a `watch` counter: every consumer (A2's scheduler, B's confirmation
//! wait) holds its own receiver from `subscribe`, so none misses a change.

use super::message_history::{HistoryUpdate, MessageHistoryHandle};
use super::messages::{MessageClock, MessageError};
use crate::control::reservation::PaneId;
use atm_core::{Harness, LifecycleEvent, SessionId};
use atm_org::{AgentId, DeliveryReceipt, MessageId};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::watch;

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
/// Bounds one scope's excluded baseline, not the sessions the daemon sees.
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
    scopes: usize,
}

/// Build once per daemon and clone to share retention and the clock origin.
#[derive(Debug, Clone)]
pub struct MessageEvidence {
    clock: MessageClock,
    state: Arc<Mutex<State>>,
    wake: watch::Sender<u64>,
}

impl MessageEvidence {
    #[must_use]
    pub fn new(clock: MessageClock) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State::default())),
            wake: watch::Sender::new(0),
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
            wake: watch::Sender::new(0),
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

    /// One wake per consumer for every change; see the module docs.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    fn wake(&self) {
        self.wake
            .send_modify(|changes| *changes = changes.wrapping_add(1));
    }

    /// Cheap check for the tap: a disabled store needs no registry proof.
    pub(crate) fn available(&self) -> bool {
        self.state().is_ok()
    }

    /// Only for a broken internal invariant, never for one caller or event.
    /// Reached at most once: `state` refuses a disabled store.
    fn disable(&self, state: &mut State, reason: &str) -> MessageError {
        tracing::error!(reason, "Message evidence disabled until restart");
        state.disabled = Some(reason.into());
        self.wake();
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
        self.wake();
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
        self.credit(session, pane, Some(harness), prompt(event), true, None)
    }

    /// The connection tap's entry. A status line has no event and no harness.
    /// Stamps the receive time, credits the scopes and forwards the Activity
    /// to history under one lock acquisition; never substitute a later time.
    /// `proven` is the tap's parent proof; see the module docs for an event
    /// without it. Either way the receive fact is real activity for history.
    pub(crate) fn tap(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: Option<Harness>,
        event: Option<&LifecycleEvent>,
        proven: bool,
        history: Option<&MessageHistoryHandle>,
    ) -> Result<(), MessageError> {
        self.credit(
            session,
            pane,
            harness,
            event.and_then(prompt),
            proven,
            history,
        )
    }

    /// Genuine accepted status-line activity cannot create a receipt or bind a
    /// lazy scope (it lacks that scope's positive harness identity).
    pub fn observe_activity(
        &self,
        session: &SessionId,
        pane: Option<&str>,
    ) -> Result<(), MessageError> {
        self.credit(session, pane, None, None, true, None)
    }

    fn credit(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: Option<Harness>,
        prompt: Option<&str>,
        proven: bool,
        history: Option<&MessageHistoryHandle>,
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
                    if proven {
                        scope.lazy_session = Some(session.clone());
                    }
                }
                let exact = pane.is_some() && prompt == Some(entry.expected.as_str());
                // An unproven exact reference may be a lost late receipt of an
                // earlier scope: hold the message rather than lose the fact.
                if Some(identity.attempt_seq) == last_seq || (exact && !proven) {
                    entry.probably_landed = true;
                }
                if proven && exact && entry.receipt.is_none() {
                    entry.receipt = Some(DeliveryReceipt {
                        session_id: session.clone(),
                        confirmation: "prompt_submit_exact_reference".into(),
                    });
                }
            }
        }
        self.wake();
        if let Some(history) = history {
            let update = HistoryUpdate::Activity {
                at,
                session_id: session.clone(),
                pane,
            };
            if history.record(update, at).is_err() {
                return Err(self.disable(&mut state, "message activity history unavailable"));
            }
        }
        Ok(())
    }
}

fn prompt(event: &LifecycleEvent) -> Option<&str> {
    match event {
        LifecycleEvent::PromptSubmit { prompt } => prompt.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
#[path = "message_evidence_tests.rs"]
mod tests;
