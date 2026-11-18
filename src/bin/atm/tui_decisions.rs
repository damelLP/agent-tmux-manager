//! Decision I/O on the TUI's existing control connection, never a CLI/second socket.
use super::{
    away::{collect_pages, PageKind},
    org::OrgTree,
    tui_decision_popup::{DecisionPopup, Effect, Input, Screen},
};
use anyhow::{bail, Context, Result};
use atm_core::SessionView;
use atm_org::{Address, DecisionId, DecisionStatus};
use atm_protocol::{DaemonMessage, ResponsePayload};
use atm_tui::{
    control::{ControlFailure, ControlNotice},
    daemon::ControlHandle,
};
use atmd::org::decisions::{
    DecisionEntry, GetParams, GetResult, ListParams, ListResult, MutationResult, ResolveParams,
};
use crossterm::event::KeyEvent;
use ratatui::Frame;
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::HashSet, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Debug)]
pub(super) enum Reply {
    Loaded(Box<DecisionEntry>),
    Recorded(Box<MutationResult>, Option<String>),
}
#[derive(Debug)]
pub(super) struct Snapshot {
    tree: Box<OrgTree>,
    entries: Vec<DecisionEntry>,
}
#[derive(Debug)]
pub(super) enum Update {
    Popup {
        generation: u64,
        result: Result<Reply>,
    },
    Snapshot {
        generation: u64,
        result: Result<Snapshot>,
    },
}
#[derive(Debug)]
struct OpenRead {
    id: DecisionId,
    dirty: bool,
}
/// One connection Watch covers both snapshot consumers; a later cursor could lose a gap.
pub(super) fn watch_scope(
    decision: Option<u64>,
    work: Option<&(u64, Vec<String>)>,
) -> Option<(Vec<String>, u64)> {
    let mut cursor = decision;
    let mut topics: Vec<String> = decision.map_or_else(Vec::new, |_| {
        ["decision.", "agent.", "project."]
            .into_iter()
            .map(Into::into)
            .collect()
    });
    if let Some((seq, interests)) = work {
        cursor = Some(cursor.map_or(*seq, |old| old.min(*seq)));
        for topic in interests {
            if !topics.contains(topic) {
                topics.push(topic.clone());
            }
        }
    }
    cursor.map(|cursor| (topics, cursor))
}

pub(super) struct DecisionController {
    pub(super) popup: Option<DecisionPopup>,
    pub(super) waiting_panes: HashSet<String>,
    snapshot: Option<Snapshot>,
    reader: Option<JoinHandle<()>>,
    read_generation: u64,
    dirty: bool,
    online: bool,
    pending_open: Option<Option<String>>,
    decision_watch: Option<u64>,
    work_watch: Option<(u64, Vec<String>)>,
    pending_work_join: Option<u64>,
    control: ControlHandle,
    from_pane: Option<String>,
    generation: u64,
    task: Option<JoinHandle<()>>,
    get: Option<OpenRead>,
    mutating: bool,
    tx: mpsc::UnboundedSender<Update>,
    rx: mpsc::UnboundedReceiver<Update>,
}
impl DecisionController {
    pub(super) fn new(control: ControlHandle, from_pane: Option<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            popup: None,
            waiting_panes: HashSet::new(),
            snapshot: None,
            reader: None,
            read_generation: 0,
            dirty: false,
            online: true,
            pending_open: None,
            decision_watch: None,
            work_watch: None,
            pending_work_join: None,
            control,
            from_pane,
            generation: 0,
            task: None,
            get: None,
            mutating: false,
            tx,
            rx,
        }
    }
    fn install_watch(&self) -> Result<(), ControlFailure> {
        if self.pending_work_join.is_some() {
            return Ok(());
        }
        match watch_scope(self.decision_watch, self.work_watch.as_ref()) {
            Some((topics, cursor)) => self.control.watch(topics, cursor),
            None => self.control.unwatch(),
        }
    }
    pub(super) fn set_work_watch(
        &mut self,
        interest: Option<(u64, Vec<String>)>,
    ) -> Result<(), ControlFailure> {
        if self.work_watch == interest {
            return Ok(());
        }
        let joining = self.work_watch.is_none() && interest.is_some();
        self.work_watch = interest;
        if self.work_watch.is_none() {
            self.pending_work_join = None;
        } else if joining
            && (self.decision_watch.is_some() || self.reader.is_some() || !self.online)
        {
            // Even an in-flight read may have captured org.tree before this join.
            // Keep the old watch live until a post-join generation covers decisions.
            self.pending_work_join = Some(self.read_generation.wrapping_add(1));
            self.refresh();
        }
        let result = self.install_watch();
        if result.is_err() {
            self.waiting_panes.clear();
        }
        result
    }
    pub(super) fn advance_work_cursor(&mut self, seq: u64) {
        if let Some((cursor, _)) = &mut self.work_watch {
            *cursor = (*cursor).max(seq);
        }
    }
    pub(super) fn open_for(&mut self, session: Option<&SessionView>) {
        if self.mutating {
            return;
        }
        self.pending_open = Some(
            session
                .filter(|s| s.parent_session_id.is_none())
                .and_then(|s| s.tmux_pane.clone()),
        );
        self.popup = Some(DecisionPopup::default());
        self.refresh();
    }
    // Shared entry point for future organisation/Work rows, with no row/schema change.
    pub(super) fn open(&mut self, id: DecisionId) {
        if self.mutating {
            return;
        }
        self.popup.get_or_insert_with(DecisionPopup::default);
        if let Some(get) = &mut self.get {
            if get.id == id {
                get.dirty = true;
                return;
            }
        }
        self.get = Some(OpenRead { id, dirty: false });
        self.start(move |control, caller| async move {
            let result: GetResult = call(
                &control,
                &caller,
                "decision.get",
                GetParams {
                    id,
                    redo_after: None,
                    redo_limit: None,
                },
                27,
            )
            .await?;
            if result.decision.decision.id != id {
                bail!("decision.get returned a different ID");
            }
            if result.redo.is_some() {
                Ok(finish_redo(
                    &control,
                    &caller,
                    MutationResult {
                        decision: result.decision,
                        changed: false,
                        redo: result.redo,
                    },
                )
                .await)
            } else {
                Ok(Reply::Loaded(Box::new(result.decision)))
            }
        });
    }
    fn start<F, Fut>(&mut self, work: F)
    where
        F: FnOnce(ControlHandle, Option<String>) -> Fut,
        Fut: std::future::Future<Output = Result<Reply>> + Send + 'static,
    {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let control = self.control.clone();
        let caller = self.from_pane.clone();
        let tx = self.tx.clone();
        let work = work(control, caller);
        self.task = Some(tokio::spawn(async move {
            let _ = tx.send(Update::Popup {
                generation,
                result: work.await,
            });
        }));
    }
    pub(super) async fn recv(&mut self) -> Option<Update> {
        self.rx.recv().await
    }
    pub(super) fn apply(&mut self, update: Update) {
        let (generation, result) = match update {
            Update::Snapshot { generation, result } => {
                self.apply_snapshot(generation, result);
                return;
            }
            Update::Popup { generation, result } => (generation, result),
        };
        if generation != self.generation || self.popup.is_none() {
            return;
        }
        if let Some(get) = self.get.take() {
            if get.dirty {
                // This result predates an observed invalidation. Keep the popup
                // gated until one coalesced get issued after it completes.
                self.open(get.id);
                return;
            }
        }
        let was_mutating = self.mutating;
        self.mutating = false;
        match result {
            Ok(Reply::Loaded(entry)) => {
                if let Some(popup) = &mut self.popup {
                    popup.reduce(Input::Loaded(entry));
                }
            }
            Ok(Reply::Recorded(result, error)) => {
                if let Some(popup) = &mut self.popup {
                    popup.reduce(Input::Recorded(result, error));
                }
                self.refresh();
            }
            Err(error) => {
                if let Some(popup) = &mut self.popup {
                    popup.reduce(if was_mutating {
                        Input::OutcomeUnknown(error.to_string())
                    } else {
                        Input::Failed(error.to_string())
                    });
                }
            }
        }
    }
    /// One shared projection read in flight; bursts require at most one follow-up.
    pub(super) fn refresh(&mut self) {
        if self.reader.is_some() || !self.online {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        self.read_generation = self.read_generation.wrapping_add(1);
        let generation = self.read_generation;
        let control = self.control.clone();
        let caller = self.from_pane.clone();
        let tx = self.tx.clone();
        self.reader = Some(tokio::spawn(async move {
            let _ = tx.send(Update::Snapshot {
                generation,
                result: read_snapshot(&control, &caller).await,
            });
        }));
    }
    fn apply_snapshot(&mut self, generation: u64, result: Result<Snapshot>) {
        if generation != self.read_generation {
            return;
        }
        self.reader = None;
        let joined = self.pending_work_join == Some(generation);
        if joined {
            self.pending_work_join = None;
        }
        match result {
            Ok(snapshot) => {
                self.waiting_panes = snapshot
                    .tree
                    .projects
                    .iter()
                    .flat_map(|project| &project.seats)
                    .filter(|seat| {
                        snapshot.entries.iter().any(|entry| {
                            entry.decision.request.requester
                                == Address::Agent(seat.agent_id.clone())
                        })
                    })
                    .filter_map(|seat| seat.pane.clone())
                    .collect();
                if let Some(seq) = snapshot.tree.seq {
                    self.decision_watch = Some(seq);
                    if self.install_watch().is_err() {
                        self.waiting_panes.clear();
                    }
                }
                self.snapshot = Some(snapshot);
                if self.pending_open.is_none() {
                    if let Some(id) = self.open_id() {
                        self.changed(id);
                    }
                }
                if !self.dirty {
                    if let Some(pane) = self.pending_open.take() {
                        if let Some(snapshot) = &self.snapshot {
                            let agent = pane
                                .as_ref()
                                .and_then(|pane| {
                                    snapshot
                                        .tree
                                        .projects
                                        .iter()
                                        .flat_map(|p| &p.seats)
                                        .find(|seat| seat.pane.as_ref() == Some(pane))
                                })
                                .map(|seat| Address::Agent(seat.agent_id.clone()));
                            let matching: Vec<_> = snapshot
                                .entries
                                .iter()
                                .filter(|entry| {
                                    Some(&entry.decision.request.requester) == agent.as_ref()
                                })
                                .cloned()
                                .collect();
                            let direct = (matching.len() == 1)
                                .then(|| matching.first().map(|e| e.decision.id))
                                .flatten();
                            if let Some(popup) = &mut self.popup {
                                popup.reduce(Input::Queue(if matching.is_empty() {
                                    snapshot.entries.clone()
                                } else {
                                    matching
                                }));
                            }
                            if let Some(id) = direct {
                                if let Some(popup) = &mut self.popup {
                                    popup.screen = Screen::Loading;
                                }
                                self.open(id);
                            }
                        }
                    }
                }
            }
            Err(error) => {
                self.waiting_panes.clear();
                if joined {
                    // A failed refresh must not strand Work without its event stream.
                    let _ = self.install_watch();
                }
                if self.pending_open.take().is_some() {
                    if let Some(popup) = &mut self.popup {
                        popup.reduce(Input::Failed(error.to_string()));
                    }
                }
            }
        }
        if self.dirty {
            self.refresh();
        }
    }
    fn open_id(&self) -> Option<DecisionId> {
        self.get.as_ref().map(|get| get.id).or_else(|| {
            match self.popup.as_ref().map(|popup| &popup.screen) {
                Some(Screen::Inspect(entry)) => Some(entry.decision.id),
                _ => None,
            }
        })
    }
    fn changed(&mut self, id: DecisionId) {
        if self.mutating {
            return;
        }
        let effects = self
            .popup
            .as_mut()
            .map(|popup| popup.reduce(Input::Changed(id)))
            .unwrap_or_default();
        if let Some(get) = &mut self.get {
            if get.id == id {
                get.dirty = true;
            }
            return;
        }
        for effect in effects {
            if let Effect::Get(id) = effect {
                self.open(id);
            }
        }
    }
    pub(super) fn notice(&mut self, notice: &ControlNotice) {
        if let ControlNotice::Event(DaemonMessage::Event { seq, .. }) = notice {
            self.advance_work_cursor(*seq);
        }
        match notice {
            ControlNotice::Capabilities { .. } => {
                if !self.online || self.snapshot.is_none() {
                    self.online = true;
                    self.refresh();
                }
                if let Some(id) = self.open_id() {
                    self.changed(id);
                }
            }
            ControlNotice::Event(DaemonMessage::Event {
                seq, kind, body, ..
            }) if ["decision.", "agent.", "project."]
                .iter()
                .any(|prefix| kind.starts_with(prefix)) =>
            {
                // A lower Work cursor may replay events already covered by this snapshot.
                if self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.tree.seq)
                    .is_some_and(|old| *seq <= old)
                {
                    return;
                }
                if kind.starts_with("decision.") {
                    if let Some(id) = body
                        .get("decision_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| id.parse().ok())
                    {
                        self.changed(id);
                    }
                }
                self.refresh();
            }
            ControlNotice::WatchFailed(_) => {
                self.decision_watch = None;
                self.work_watch = None;
                self.pending_work_join = None;
                self.waiting_panes.clear();
                let _ = self.control.unwatch();
                if let Some(id) = self.open_id() {
                    self.changed(id);
                }
            }
            _ => {}
        }
    }
    pub(super) fn disconnected(&mut self) {
        self.decision_watch = None;
        self.work_watch = None;
        self.pending_work_join = None;
        self.online = false;
        self.dirty = true;
        self.waiting_panes.clear();
        self.read_generation = self.read_generation.wrapping_add(1);
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let _ = self.control.unwatch();
    }

    /// Returns true only for the application's explicit Ctrl-C quit.
    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let effects = self
            .popup
            .as_mut()
            .map(|popup| popup.reduce(Input::Key(key)))
            .unwrap_or_default();
        for effect in effects {
            match effect {
                Effect::Get(id) => self.open(id),
                Effect::Resolve(params) if !self.mutating => {
                    self.mutating = true;
                    self.get = None;
                    self.start(move |control, caller| async move {
                        let result: MutationResult =
                            call(&control, &caller, "decision.resolve", &params, 27).await?;
                        validate_resolution(&params, &result)?;
                        Ok(finish_redo(&control, &caller, result).await)
                    });
                }
                Effect::Resolve(_) => {}
                Effect::Close => {
                    if let Some(task) = self.task.take() {
                        task.abort();
                    }
                    self.generation = self.generation.wrapping_add(1);
                    self.pending_open = None;
                    self.get = None;
                    self.popup = None;
                }
                Effect::Quit => return true,
            }
        }
        false
    }
    pub(super) fn render(&mut self, frame: &mut Frame) {
        if let Some(popup) = &mut self.popup {
            popup.render(frame);
        }
    }
    pub(super) fn shutdown(&mut self) {
        self.decision_watch = None;
        self.work_watch = None;
        self.pending_work_join = None;
        self.get = None;
        self.online = false;
        self.dirty = false;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let _ = self.control.unwatch();
    }
}
impl Drop for DecisionController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn read_snapshot(control: &ControlHandle, caller: &Option<String>) -> Result<Snapshot> {
    let tree: OrgTree = call(control, caller, "org.tree", serde_json::json!({}), 17).await?;
    if tree.seq.is_none() {
        bail!("org.tree lacks a watch cursor; decision service unavailable");
    }
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let page: ListResult = call(
            control,
            caller,
            "decision.list",
            ListParams {
                statuses: None,
                after,
                limit: 100,
                count_only: false,
            },
            27,
        )
        .await?;
        for entry in &page.decisions {
            if after.is_some_and(|old| entry.decision.id <= old) {
                bail!("non-monotonic decision page");
            }
            after = Some(entry.decision.id);
        }
        if page.next_after.is_some() && (page.decisions.is_empty() || page.next_after != after) {
            bail!("invalid decision page cursor");
        }
        entries.extend(page.decisions);
        if page.next_after.is_none() {
            break;
        }
    }
    Ok(Snapshot {
        tree: Box::new(tree),
        entries,
    })
}

fn validate_resolution(params: &ResolveParams, result: &MutationResult) -> Result<()> {
    let decision = &result.decision.decision;
    let valid = decision.id == params.id
        && matches!(
            decision.status,
            DecisionStatus::Resolved | DecisionStatus::Overruled
        )
        && decision.resolution.as_ref().is_some_and(|answer| {
            answer.decision_id == params.id
                && answer.option == params.option
                && answer.note == params.note
                && answer.standing_rule == params.standing_rule
                && answer.by == Address::Owner
        });
    if !valid {
        bail!("invalid resolution result; inspect the decision before retrying");
    }
    Ok(())
}

async fn finish_redo(
    control: &ControlHandle,
    caller: &Option<String>,
    mut result: MutationResult,
) -> Reply {
    let mut error = None;
    if let Some(page) = result.redo.take() {
        let id = result.decision.decision.id;
        let complete = async {
            let raw = collect_pages(
                serde_json::to_value(page)?,
                PageKind::Redo,
                |after| async move {
                    let page: GetResult = call(
                        control,
                        caller,
                        "decision.get",
                        GetParams {
                            id,
                            redo_after: Some(serde_json::from_value(after)?),
                            redo_limit: Some(100),
                        },
                        27,
                    )
                    .await?;
                    if page.decision.decision.id != id {
                        bail!("redo page returned a different decision");
                    }
                    serde_json::to_value(page.redo.context("missing redo page")?)
                        .map_err(Into::into)
                },
            )
            .await?;
            serde_json::from_value(raw).context("invalid complete redo report")
        }
        .await;
        match complete {
            Ok(page) => result.redo = Some(page),
            Err(failure) => error = Some(failure.to_string()),
        }
    }
    Reply::Recorded(Box::new(result), error)
}

async fn call<P: Serialize, R: DeserializeOwned>(
    control: &ControlHandle,
    caller: &Option<String>,
    method: &str,
    params: P,
    seconds: u64,
) -> Result<R> {
    match control
        .request(
            method.into(),
            serde_json::to_value(params)?,
            caller.clone(),
            Duration::from_secs(seconds),
        )
        .await?
    {
        ResponsePayload::Result(raw) => {
            serde_json::from_value(raw).context("invalid decision result from daemon")
        }
        ResponsePayload::Error(error) => bail!("{}: {}", error.code, error.message),
    }
}
