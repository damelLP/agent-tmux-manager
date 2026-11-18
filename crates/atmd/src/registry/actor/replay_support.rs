//! Test-only ordered actor boundary. No tickers, select race, or git fallback.

use std::collections::BTreeMap;
use std::sync::{mpsc as blocking, Arc};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{replay_commands, replay_inputs::InputTape, RegistryActor};
use crate::registry::git::GitContext;
use crate::registry::{RegistryCommand, RegistryHandle, SessionEvent};

type Lookup = (String, blocking::Sender<Option<GitContext>>);

pub(crate) struct Driver {
    pub(super) actor: RegistryActor,
    pub(crate) handle: RegistryHandle,
    arrivals: mpsc::UnboundedReceiver<Lookup>,
    held: BTreeMap<String, blocking::Sender<Option<GitContext>>>,
    inputs: Option<Arc<InputTape>>,
    events: tokio::sync::broadcast::Receiver<SessionEvent>,
    pub(crate) origin: Instant,
    pub(crate) turns: Vec<Value>,
}

impl Driver {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let (events, event_tap) = tokio::sync::broadcast::channel(4096);
        let handle = RegistryHandle::new(sender, events.clone());
        let (entered, arrivals) = mpsc::unbounded_channel();
        let actor = RegistryActor::with_git_resolver(receiver, events, move |cwd| {
            let (release, held) = blocking::channel();
            if entered.send((cwd.to_owned(), release)).is_err() {
                return (None, None, None);
            }
            match held.recv() {
                Ok(Some(context)) => context,
                Ok(None) => panic!("Synthetic: scripted git resolver failure"),
                Err(_) => (None, None, None),
            }
        });
        Self {
            actor,
            handle,
            arrivals,
            held: BTreeMap::new(),
            inputs: None,
            events: event_tap,
            origin: Instant::now(),
            turns: Vec::new(),
        }
    }

    pub(crate) fn recording() -> Self {
        let mut driver = Self::new();
        let inputs = InputTape::record();
        driver.origin = inputs.origin;
        driver.actor.clock = inputs.clone();
        driver.actor.process_info = inputs.clone();
        driver.inputs = Some(inputs);
        driver
    }

    fn record_turn(&mut self, action: Value) {
        // Every publication is drained at the command/result barrier. A lag is
        // a failed capture, not a fixture that silently omits state changes.
        loop {
            match self.events.try_recv() {
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                other => panic!("registry event capture lost: {other:?}"),
            }
        }
        if let Some(inputs) = self.inputs.as_ref().filter(|inputs| inputs.recording) {
            let comparison = Utc::now();
            self.turns
                .push(json!({"action":action, "inputs":inputs.take(),
                "comparison":comparison, "checkpoint":self.checkpoint(self.origin, comparison)}));
        }
    }

    pub(crate) async fn replay_turn(&mut self, turn: &Value) -> Value {
        let inputs = InputTape::replay(self.origin, &turn["inputs"]);
        self.actor.clock = inputs.clone();
        self.actor.process_info = inputs.clone();
        self.inputs = Some(inputs.clone());
        if let Some(git) = turn["action"].get("git") {
            let cwd = git["cwd"].as_str().unwrap();
            self.step_git(cwd, serde_json::from_value(git["context"].clone()).unwrap())
                .await;
        } else {
            self.actor
                .handle_command(replay_commands::decode(&turn["action"]["command"]));
            self.record_turn(Value::Null);
        }
        inputs.assert_consumed();
        self.checkpoint(
            self.origin,
            serde_json::from_value(turn["comparison"].clone()).unwrap(),
        )
    }

    /// Resolve a capture's owned private repository on the I/O boundary. Replay
    /// never calls this: it releases the worker with the recorded answer.
    pub(crate) async fn capture_git(&mut self, cwd: &str) {
        let owned = cwd.to_owned();
        let context = tokio::task::spawn_blocking(move || super::resolve_git_context(&owned))
            .await
            .unwrap();
        self.step_git(cwd, Some(context)).await;
    }

    pub(crate) fn accept(&mut self, command: RegistryCommand) {
        let action = json!({"command": replay_commands::encode(&command)});
        self.actor.handle_command(command);
        self.record_turn(action);
    }

    pub(crate) async fn capture_pending_git(&mut self) {
        let state = self.actor.git_lookups.replay_state(self.origin);
        let cwds: Vec<_> = state.as_object().unwrap().keys().cloned().collect();
        for cwd in cwds {
            self.capture_git(&cwd).await;
        }
    }

    /// Accept exactly one queued command through the production dispatcher.
    /// The response is the barrier: all its mutation/publication is complete.
    pub(crate) async fn step_command(&mut self) {
        let command: RegistryCommand = self.actor.receiver.recv().await.unwrap();
        self.accept(command);
    }

    /// Release only this directory's worker; arrival order is not apply order.
    pub(crate) async fn step_git(&mut self, cwd: &str, context: Option<GitContext>) {
        while !self.held.contains_key(cwd) {
            let (directory, release) = self.arrivals.recv().await.unwrap();
            assert!(self.held.insert(directory, release).is_none());
        }
        self.held
            .remove(cwd)
            .unwrap()
            .send(context.clone())
            .unwrap();
        let (directory, observed) = self.actor.git_results.recv().await.unwrap();
        assert_eq!((directory.as_str(), &observed), (cwd, &context));
        self.actor.apply_git_result(directory, observed);
        self.record_turn(json!({"git":{"cwd":cwd, "context":context}}));
    }

    /// Canonicalize only unordered containers. Domain values, vector order,
    /// timestamps, both indexes and hidden continuation state are untouched.
    pub(crate) fn checkpoint(&self, origin: Instant, comparison: DateTime<Utc>) -> Value {
        let rows: BTreeMap<_, _> = self
            .actor
            .sessions_by_pid
            .iter()
            .map(|(pid, (domain, infra))| {
                (
                    pid.to_string(),
                    json!({"domain": domain, "infra": infra.replay_state(),
                "view": atm_core::SessionView::from_domain_at(domain, comparison)}),
                )
            })
            .collect();
        let ids: BTreeMap<_, _> = self
            .actor
            .session_id_to_pid
            .iter()
            .map(|(id, pid)| (id.to_string(), pid))
            .collect();
        let mut aliases: Vec<_> = self
            .actor
            .child_refs
            .iter()
            .map(|((parent, reference), child)| {
                let reference = replay_commands::reference(reference);
                json!([parent, reference, child])
            })
            .collect();
        aliases.sort_by_key(Value::to_string);
        let mut ended: Vec<_> = self
            .actor
            .ended_sessions
            .iter()
            .map(|((id, pid), at)| json!([id, pid, at]))
            .collect();
        ended.sort_by_key(Value::to_string);
        json!({"rows": rows, "ids": ids, "aliases": aliases, "ended": ended,
            "git": self.actor.git_lookups.replay_state(origin)})
    }
}
