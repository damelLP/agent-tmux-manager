use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use atm_comms::message_types::{AckParams, DeliveryPhase, InboxEntry, InboxParams, SendParams};
use atm_comms::{Address, AgentId, DeliveryStatus, HistoryParams, MessageId, MessageKind};
use atm_tui::keybinding::{InputHandler, UiAction};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct View {
    pub ids: BTreeSet<MessageId>,
    pub selected: Option<MessageId>,
    pub draft: String,
    pub search: String,
    pub older: Option<String>,
    pub scroll: usize,
    pub head: u64,
    pub fence: u64,
    pub epoch: u64,
    pub stale: bool,
}
fn same_row(a: &InboxEntry, b: &InboxEntry) -> bool {
    a.message_id == b.message_id
        && a.content == b.content
        && a.sent_at == b.sent_at
        && a.status == b.status
        && a.read_at == b.read_at
        && a.delivery.phase == b.delivery.phase
        && a.delivery.reason == b.delivery.reason
        && a.delivery.attempt == b.delivery.attempt
        && a.delivery.pane == b.delivery.pane
        && a.delivery.session_id == b.delivery.session_id
        && a.delivery.confirmation == b.delivery.confirmation
        && a.delivery.retry_at == b.delivery.retry_at
        && a.delivery.blocked_by == b.delivery.blocked_by
}

fn regresses(old: &InboxEntry, row: &InboxEntry) -> bool {
    old.delivery.attempt > row.delivery.attempt
        || (old.status == DeliveryStatus::Delivered && row.status != DeliveryStatus::Delivered)
        // Queued after an attempt is its non-final failure; retry must increment it.
        || (old.status == DeliveryStatus::Queued && old.delivery.attempt > 0
            && old.delivery.attempt == row.delivery.attempt && row.status == DeliveryStatus::Delivering)
        || (old.status == DeliveryStatus::Failed
            && matches!(
                row.status,
                DeliveryStatus::Queued | DeliveryStatus::Delivering
            ))
}

#[derive(Debug, Clone)]
pub(crate) struct PendingRead {
    pub token: OpToken,
    pub purpose: ReadPurpose,
    epoch: u64,
    head_chain: bool,
    followup: Option<(ReadPurpose, bool)>,
    older_followup: Option<(ReadPurpose, bool)>,
}
#[derive(Debug, Clone)]
struct Submission {
    thread: ThreadKey,
    target: Address,
    body: String,
    head: u64,
    dispatched: Option<Duration>,
    unknown: bool,
}

#[derive(Debug, Clone, Copy)]
struct AckRefusal {
    at: Duration,
    tree_requested: Duration,
}

/// Deterministic chat reducer. All time, binding and message facts are supplied.
#[derive(Debug, Clone)]
pub struct ChatState {
    pub(crate) generation: u64,
    pub(crate) thread: ThreadKey,
    pub(crate) column: Column,
    pub(crate) mode: Mode,
    pub(crate) status: String,
    pub(crate) current: View,
    rows: BTreeMap<MessageId, InboxEntry>,
    row_heads: BTreeMap<MessageId, u64>,
    cached: BTreeMap<ThreadKey, View>,
    pub(crate) reads: BTreeMap<OpToken, PendingRead>,
    pub(crate) next_token: u64,
    pub(crate) search_edit: String,
    pub(crate) unread_count: usize,
    pub(crate) summary_incomplete: bool,
    pub(crate) unread_by_sender: BTreeMap<Address, usize>,
    pub(crate) waiting_decisions: usize,
    pub(crate) decisions_truncated: bool,
    pub(crate) turn_notes: BTreeMap<AgentId, String>,
    turn_heads: BTreeMap<AgentId, u64>,
    turn_replied: BTreeSet<AgentId>,
    pub(crate) seats: Vec<SeatCard>,
    pub(crate) size: (u16, u16),
    pub(crate) online: bool,
    pub(crate) compatible: bool,
    pub(crate) read_hint: Option<String>,
    pub(crate) thread_index: usize,
    pub(crate) seat_index: usize,
    initial_choice: bool,
    started: bool,
    initialized: bool,
    tree_fresh: bool,
    bound: bool,
    closed: bool,
    watched: bool,
    watch_failed_at: Option<Duration>,
    watch_seq: u64,
    known_head: u64,
    submissions: BTreeMap<OpToken, Submission>,
    recorded_sends: BTreeSet<MessageId>,
    locked: Option<OpToken>,
    ack_refusal: Option<AckRefusal>,
    ack_pending: BTreeMap<OpToken, Vec<MessageId>>,
    ack_suppressed: BTreeSet<MessageId>,
    unknown_acks: BTreeMap<MessageId, OpToken>,
    presented: BTreeSet<MessageId>,
    last_progress: Duration,
    motion: InputHandler,
}

impl ChatState {
    #[must_use]
    pub fn new(config: ChatConfig) -> Self {
        let initial_choice = config.initial_thread.is_none();
        Self {
            generation: 0,
            thread: config
                .initial_thread
                .map(ThreadKey::Direct)
                .unwrap_or(ThreadKey::ForYou),
            column: Column::Messages,
            mode: Mode::Normal,
            status: "loading chat".into(),
            current: View::default(),
            rows: BTreeMap::new(),
            row_heads: BTreeMap::new(),
            cached: BTreeMap::new(),
            reads: BTreeMap::new(),
            next_token: 0,
            search_edit: String::new(),
            unread_count: 0,
            summary_incomplete: false,
            unread_by_sender: BTreeMap::new(),
            waiting_decisions: 0,
            decisions_truncated: false,
            turn_notes: BTreeMap::new(),
            turn_heads: BTreeMap::new(),
            turn_replied: BTreeSet::new(),
            seats: vec![],
            size: (120, 32),
            online: false,
            compatible: false,
            read_hint: Some("auto-ack unavailable: focus not checked".into()),
            thread_index: 0,
            seat_index: 0,
            initial_choice,
            started: false,
            initialized: false,
            tree_fresh: false,
            bound: false,
            closed: false,
            watched: false,
            watch_failed_at: None,
            watch_seq: 0,
            known_head: 0,
            submissions: BTreeMap::new(),
            recorded_sends: BTreeSet::new(),
            locked: None,
            ack_refusal: None,
            ack_pending: BTreeMap::new(),
            ack_suppressed: BTreeSet::new(),
            unknown_acks: BTreeMap::new(),
            presented: BTreeSet::new(),
            last_progress: Duration::ZERO,
            motion: InputHandler::new(),
        }
    }

    /// Current presentation generation, without rendering or mutating state.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The only generation writer. Rendering and supplied-time no-ops cannot advance it.
    #[must_use]
    pub fn reduce(&mut self, input: ChatInput, now: ChatTime) -> Vec<ChatEffect> {
        if self.closed {
            return vec![];
        }
        let before = self.clone();
        let mut effects = vec![];
        self.apply(input, now, &mut effects);
        if !self.same_presentation(&before) {
            if let Some(next) = self.generation.checked_add(1) {
                self.generation = next;
            } else {
                self.status = "presentation generation exhausted; restart chat".into();
                self.compatible = false;
            }
        }
        effects
    }

    fn same_presentation(&self, b: &Self) -> bool {
        self.thread == b.thread
            && self.column == b.column
            && self.mode == b.mode
            && self.status == b.status
            && self.current == b.current
            && self
                .current
                .ids
                .iter()
                .all(|id| match (self.rows.get(id), b.rows.get(id)) {
                    (Some(a), Some(b)) => same_row(a, b),
                    (None, None) => true,
                    _ => false,
                })
            && self.search_edit == b.search_edit
            && self.unread_count == b.unread_count
            && self.summary_incomplete == b.summary_incomplete
            && self.unread_by_sender == b.unread_by_sender
            && self.waiting_decisions == b.waiting_decisions
            && self.decisions_truncated == b.decisions_truncated
            && self.turn_notes == b.turn_notes
            && self.seats == b.seats
            && self.size == b.size
            && self.online == b.online
            && self.compatible == b.compatible
            && self.read_hint == b.read_hint
            && self.thread_index == b.thread_index
            && self.seat_index == b.seat_index
            && self.tree_fresh == b.tree_fresh
            && self.bound == b.bound
            && self.closed == b.closed
            && self.locked == b.locked
            && self.ack_suppressed == b.ack_suppressed
            && self.ack_refusal.is_some() == b.ack_refusal.is_some()
    }

    fn apply(&mut self, input: ChatInput, now: ChatTime, effects: &mut Vec<ChatEffect>) {
        match input {
            ChatInput::Start if !self.started => {
                self.started = true;
                self.request(ReadPurpose::Threads, effects);
            }
            ChatInput::ThreadsLoaded(token, result) => {
                let Some(pending) = self.take_read(token, |p| matches!(p, ReadPurpose::Threads))
                else {
                    return;
                };
                match result {
                    Ok(facts) => {
                        self.known_head = self.known_head.max(facts.tree_seq);
                        self.bound = facts.caller_bound;
                        self.tree_fresh = pending.followup.is_none();
                        if self.tree_fresh && self.bound && !self.initialized {
                            self.closed = true;
                            effects.push(ChatEffect::Quit {
                                error: Some(ChatFailure::Refused(
                                    "chat runs from an unbound pane".into(),
                                )),
                            });
                            return;
                        }
                        self.update_seats(facts.seats);
                        if !self.initialized {
                            self.thread_index = self
                                .threads()
                                .iter()
                                .position(|t| t == &self.thread)
                                .unwrap_or(0);
                        }
                        self.waiting_decisions = facts.waiting_decisions;
                        self.decisions_truncated = facts.decisions_truncated;
                        if !self.initialized && self.tree_fresh {
                            self.initialized = true;
                            if self.compatible && self.watch_failed_at.is_none() {
                                self.status.clear();
                            }
                            self.watch_seq = facts.tree_seq;
                            self.watch(now, effects);
                            self.request(ReadPurpose::UnreadSummary, effects);
                            if !self.initial_choice {
                                self.head(effects);
                            }
                        }
                        if self.bound {
                            self.status = "not sent: caller pane is bound".into();
                        }
                    }
                    Err(error) => {
                        self.tree_fresh = false;
                        self.status = if self.ack_refusal.is_some() {
                            format!("checking who you are; try again: {error}")
                        } else {
                            format!("tree stale: {error}")
                        };
                    }
                }
                self.followup(pending, effects);
            }
            ChatInput::PageLoaded(token, result) => {
                let Some(pending) = self.take_read(token, |p| {
                    matches!(p, ReadPurpose::Page { .. } | ReadPurpose::Reconcile { .. })
                }) else {
                    return;
                };
                if let Ok(page) = &result {
                    self.known_head = self.known_head.max(page.head).max(page.fence);
                    self.note_message_ids(&page.rows);
                    self.observe_replies(&page.rows);
                }
                match &pending.purpose {
                    ReadPurpose::Reconcile { send, .. } => {
                        if let Ok(page) = &result {
                            for row in &page.rows {
                                self.merge_row(row.clone(), Some(page.head), token);
                            }
                            self.reconcile(*send, &page.rows, effects);
                        } else if let Err(error) = result {
                            self.status = format!("send may still arrive; recovery stale: {error}");
                        }
                    }
                    ReadPurpose::Page { thread, query }
                        if thread == &self.thread && pending.epoch == self.current.epoch =>
                    {
                        match result {
                            Ok(page) => {
                                let older = page.older.clone();
                                let unrelated = !page.rows.is_empty()
                                    && !page.rows.iter().any(|r| {
                                        in_thread(&self.thread, r)
                                            && (self.current.search.is_empty()
                                                || r.content.body.contains(&self.current.search))
                                    });
                                self.install_page(page, pending.head_chain, token);
                                if unrelated {
                                    if let Some(cursor) = older {
                                        let mut query = query.clone();
                                        query.cursor = Some(cursor);
                                        self.request_with_head(
                                            ReadPurpose::Page {
                                                thread: thread.clone(),
                                                query,
                                            },
                                            pending.head_chain,
                                            effects,
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                self.current.stale = true;
                                self.status = format!("history stale: {error}");
                            }
                        }
                    }
                    _ => {}
                }
                self.followup(pending, effects);
            }
            ChatInput::InboxLoaded(token, result) => {
                let Some(pending) = self.take_read(token, |p| {
                    matches!(p, ReadPurpose::UnreadSummary | ReadPurpose::Progress(_))
                }) else {
                    return;
                };
                if let Ok(inbox) = &result {
                    self.note_message_ids(&inbox.messages);
                    self.observe_replies(&inbox.messages);
                }
                match result {
                    Ok(inbox) => match pending.purpose {
                        ReadPurpose::UnreadSummary => {
                            self.unread_count = inbox.unread_count;
                            self.summary_incomplete = inbox.next_after.is_some();
                            self.unread_by_sender.clear();
                            let mut ids = BTreeSet::new();
                            for row in inbox.messages {
                                let id = row.message_id;
                                self.merge_row(row, None, token);
                                if let Some(row) = self.rows.get(&id).filter(|row| {
                                    row.content.to == Address::Owner && row.read_at.is_none()
                                }) {
                                    *self
                                        .unread_by_sender
                                        .entry(row.content.from.clone())
                                        .or_default() += 1;
                                    ids.insert(id);
                                }
                            }
                            if self.thread == ThreadKey::ForYou {
                                self.current.ids = ids;
                                self.keep_selection();
                            }
                            if self.initial_choice {
                                self.initial_choice = false;
                                if self.unread_count == 0 {
                                    let thread = self
                                        .seats
                                        .first()
                                        .map(|s| ThreadKey::Direct(Address::Agent(s.agent.clone())))
                                        .unwrap_or(ThreadKey::All);
                                    self.open_thread(thread, effects);
                                } else {
                                    self.head(effects);
                                }
                            }
                        }
                        ReadPurpose::Progress(id) => {
                            if let Some(row) =
                                inbox.messages.into_iter().find(|r| r.message_id == id)
                            {
                                self.merge_row(row, None, token);
                            }
                        }
                        _ => {}
                    },
                    Err(error) => self.status = format!("inbox progress/summary stale: {error}"),
                }
                self.followup(pending, effects);
            }
            ChatInput::WatchEvent { seq, kind } if seq > self.watch_seq => {
                if self.watched && self.watch_failed_at.take().is_some() {
                    self.status.clear();
                }
                self.watch_seq = seq;
                self.known_head = self.known_head.max(seq);
                if kind.starts_with("msg.") && self.initialized {
                    self.head(effects);
                    self.request(ReadPurpose::UnreadSummary, effects);
                    self.recover_unknown(effects);
                } else if ["agent.", "project.", "decision."]
                    .iter()
                    .any(|prefix| kind.starts_with(prefix))
                {
                    self.tree_fresh = false;
                    self.request(ReadPurpose::Threads, effects);
                }
            }
            ChatInput::Connection {
                online,
                methods,
                watch_available,
            } => {
                let was_online = self.online;
                let was_compatible = self.compatible;
                let watch_recovered =
                    watch_available && (self.watch_failed_at.is_some() || !self.watched);
                let supports_watch = methods.iter().any(|method| method == WATCH_CAPABILITY);
                self.online = online;
                if online && !watch_available {
                    self.watched = false;
                    self.tree_fresh = false;
                    self.watch_failed_at = supports_watch.then_some(now.elapsed);
                }
                self.compatible = (watch_available || supports_watch)
                    && [
                        "org.tree",
                        "msg.history",
                        "msg.inbox",
                        "msg.send",
                        "msg.ack",
                    ]
                    .iter()
                    .all(|name| methods.iter().any(|method| method == name));
                if !online {
                    self.status = "disconnected · history stale".into();
                    self.current.stale = true;
                    self.tree_fresh = false;
                    self.turn_notes.clear();
                    self.turn_heads.clear();
                    self.turn_replied.clear();
                    self.seats.iter_mut().for_each(|s| s.working = false);
                } else if !watch_available || !self.compatible {
                    self.status = if !watch_available && supports_watch {
                        "live updates stopped; retrying"
                    } else {
                        "daemon too old: chat methods/watch unavailable · read only"
                    }
                    .into();
                    self.current.stale = true;
                } else if (!was_online || !was_compatible || watch_recovered) && self.initialized {
                    self.refresh(effects);
                }
                if online && watch_available {
                    self.watch_failed_at = None;
                    if self.compatible && self.initialized && (!was_compatible || watch_recovered) {
                        self.status.clear();
                    }
                    self.watch(now, effects);
                }
            }
            ChatInput::Key(key) if key.kind != KeyEventKind::Release => {
                self.initial_choice = false;
                self.key(key, now, effects);
            }
            ChatInput::Resize { width, height } => {
                self.size = (width, height);
                self.current.scroll = self.current.scroll.min(crate::screen::scroll_limit(self));
            }
            ChatInput::SendDispatched(token) => {
                if let Some(send) = self.submissions.get_mut(&token) {
                    if send.dispatched.is_none() {
                        send.dispatched = Some(now.elapsed);
                    }
                }
            }
            ChatInput::SendResult(token, result) if self.submissions.contains_key(&token) => {
                match result {
                    Ok(sent) => self.committed(token, sent.message_id, effects),
                    Err(ChatFailure::Refused(reason)) => {
                        self.submissions.remove(&token);
                        if self.locked == Some(token) {
                            self.locked = None;
                        }
                        self.status = reason;
                    }
                    Err(error) => {
                        if let Some(send) = self.submissions.get_mut(&token) {
                            send.unknown = true;
                        }
                        self.status =
                            format!("send outcome unknown: {error}; send may still arrive");
                        self.unlock_hint(now);
                        self.recover_unknown(effects);
                    }
                }
            }
            ChatInput::AckResult(token, result) => {
                let Some(ids) = self.ack_pending.remove(&token) else {
                    return;
                };
                let retry = result.is_err();
                if retry {
                    let floor = OpToken(self.next_token);
                    for id in &ids {
                        self.unknown_acks.insert(*id, floor);
                    }
                }
                self.status = match result {
                    Ok(_) => "read acknowledgment committed; refreshing canonical rows".into(),
                    Err(ChatFailure::Refused(reason)) => {
                        self.ack_refusal = Some(AckRefusal {
                            at: now.elapsed,
                            tree_requested: now.elapsed,
                        });
                        self.tree_fresh = false;
                        self.request(ReadPurpose::Threads, effects);
                        format!("checking who you are; try again: {reason}")
                    }
                    Err(error) => format!("read acknowledgment outcome unknown: {error}"),
                };
                self.head(effects);
                self.request(ReadPurpose::UnreadSummary, effects);
                if retry {
                    self.recover_acks(effects);
                }
            }
            ChatInput::SessionUpdate(seats) => self.update_seats(seats),
            ChatInput::Presented {
                generation,
                visible_ids,
                visible,
                hint,
            } if generation == self.generation => {
                self.retry_refused_ack(now.elapsed, effects);
                self.read_hint = hint;
                self.presented = visible_ids
                    .iter()
                    .copied()
                    .filter(|id| self.current.ids.contains(id))
                    .collect();
                if visible && self.mutations_allowed() && self.ack_refusal.is_none() {
                    let ids: Vec<_> = self
                        .presented
                        .iter()
                        .filter(|id| !self.ack_suppressed.contains(id))
                        .filter(|id| {
                            self.row(id).is_some_and(|r| {
                                r.content.to == Address::Owner
                                    && r.content.from != Address::Owner
                                    && r.status == DeliveryStatus::Delivered
                                    && r.read_at.is_none()
                            })
                        })
                        .take(1000)
                        .copied()
                        .collect();
                    if !ids.is_empty() {
                        if let Some(token) = self.token() {
                            self.ack_suppressed.extend(ids.iter().copied());
                            self.ack_pending.insert(token, ids.clone());
                            effects.push(ChatEffect::Ack {
                                token,
                                params: AckParams { ids },
                            });
                        }
                    }
                }
            }
            ChatInput::CopyResult(result) => {
                self.status = match result {
                    Ok(()) => "copied body to tmux buffer".into(),
                    Err(error) => format!("copy unavailable: {error}"),
                }
            }
            ChatInput::Tick => {
                self.retry_refused_ack(now.elapsed, effects);
                if self.online && self.watch_failed_at.is_some() {
                    let was_watched = self.watched;
                    self.watch(now, effects);
                    if !was_watched && self.watched {
                        self.refresh(effects);
                    }
                }
                if self.started
                    && !self.initialized
                    && self.online
                    && self.compatible
                    && !self
                        .reads
                        .values()
                        .any(|read| matches!(read.purpose, ReadPurpose::Threads))
                {
                    self.request(ReadPurpose::Threads, effects);
                }
                self.unlock_hint(now);
                if self.online
                    && self.compatible
                    && now.elapsed.saturating_sub(self.last_progress) >= Duration::from_secs(1)
                {
                    self.last_progress = now.elapsed;
                    let ids: Vec<_> = self
                        .presented
                        .iter()
                        .filter(|id| {
                            self.row(id).is_some_and(|r| {
                                r.content.from == Address::Owner
                                    && matches!(
                                        r.status,
                                        DeliveryStatus::Queued | DeliveryStatus::Delivering
                                    )
                            })
                        })
                        .copied()
                        .collect();
                    for id in ids {
                        self.request(ReadPurpose::Progress(id), effects);
                    }
                }
            }
            ChatInput::Close => {
                if self.watched {
                    effects.push(ChatEffect::Unwatch);
                }
                self.closed = true;
                effects.push(ChatEffect::Quit { error: None });
            }
            _ => {}
        }
    }

    fn retry_refused_ack(&mut self, elapsed: Duration, effects: &mut Vec<ChatEffect>) {
        let Some(mut refusal) = self.ack_refusal else {
            return;
        };
        if elapsed.saturating_sub(refusal.at) < Duration::from_secs(10) {
            return;
        }
        if self.tree_fresh && !self.bound {
            self.ack_refusal = None;
        } else if !self.tree_fresh
            && self.online
            && self.compatible
            && elapsed.saturating_sub(refusal.tree_requested) >= Duration::from_secs(10)
            && !self
                .reads
                .values()
                .any(|read| matches!(read.purpose, ReadPurpose::Threads))
        {
            refusal.tree_requested = elapsed;
            self.ack_refusal = Some(refusal);
            self.request(ReadPurpose::Threads, effects);
        }
    }
    fn refresh(&mut self, effects: &mut Vec<ChatEffect>) {
        self.tree_fresh = false;
        self.request(ReadPurpose::Threads, effects);
        self.head(effects);
        self.request(ReadPurpose::UnreadSummary, effects);
        self.recover_unknown(effects);
    }
    fn watch(&mut self, now: ChatTime, effects: &mut Vec<ChatEffect>) {
        if self.online
            && self.compatible
            && self.initialized
            && !self.watched
            && self
                .watch_failed_at
                .is_none_or(|failed| now.elapsed.saturating_sub(failed) >= Duration::from_secs(10))
        {
            self.watched = true;
            effects.push(ChatEffect::Watch {
                topics: ["msg.", "agent.", "project.", "decision."]
                    .map(str::to_owned)
                    .to_vec(),
                cursor: self.watch_seq,
            });
        }
    }
    fn token(&mut self) -> Option<OpToken> {
        match self.next_token.checked_add(1) {
            Some(next) => {
                self.next_token = next;
                Some(OpToken(next))
            }
            None => {
                self.status = "operation tokens exhausted; restart chat".into();
                None
            }
        }
    }
    fn take_read(
        &mut self,
        token: OpToken,
        matches: impl FnOnce(&ReadPurpose) -> bool,
    ) -> Option<PendingRead> {
        if self
            .reads
            .get(&token)
            .is_some_and(|r| r.token == token && matches(&r.purpose))
        {
            self.reads.remove(&token)
        } else {
            None
        }
    }
    fn read_epoch(&self, purpose: &ReadPurpose) -> u64 {
        match purpose {
            ReadPurpose::Page { thread, .. } if thread != &self.thread => {
                self.cached.get(thread).map_or(0, |view| view.epoch)
            }
            _ => self.current.epoch,
        }
    }
    fn request(&mut self, purpose: ReadPurpose, effects: &mut Vec<ChatEffect>) {
        let head_chain = matches!(&purpose,ReadPurpose::Page{query,..} if query.cursor.is_none());
        self.request_with_head(purpose, head_chain, effects);
    }
    fn request_with_head(
        &mut self,
        purpose: ReadPurpose,
        head_chain: bool,
        effects: &mut Vec<ChatEffect>,
    ) {
        if let Some(pending) = self
            .reads
            .values_mut()
            .find(|p| same_purpose(&p.purpose, &purpose))
        {
            if pending
                .followup
                .as_ref()
                .is_some_and(|(_, head)| *head != head_chain)
            {
                if head_chain {
                    pending.older_followup = pending.followup.take();
                } else {
                    pending.older_followup = Some((purpose, head_chain));
                    return;
                }
            }
            pending.followup = Some((purpose, head_chain));
            return;
        }
        let Some(token) = self.token() else {
            return;
        };
        let effect = match &purpose {
            ReadPurpose::Threads => ChatEffect::LoadThreads { token },
            ReadPurpose::Page { query, .. } => ChatEffect::LoadPage {
                token,
                query: query.clone(),
            },
            ReadPurpose::Reconcile { target, .. } => ChatEffect::LoadPage {
                token,
                query: history_query(&ThreadKey::Direct(target.clone()), None, None),
            },
            ReadPurpose::UnreadSummary => ChatEffect::LoadInbox {
                token,
                params: InboxParams {
                    recipient: Some(Address::Owner),
                    unread_only: true,
                    count_only: false,
                    id: None,
                    after: None,
                    limit: 100,
                },
            },
            ReadPurpose::Progress(id) => {
                let Some(row) = self.rows.get(id) else {
                    return;
                };
                ChatEffect::LoadInbox {
                    token,
                    params: InboxParams {
                        recipient: Some(row.content.to.clone()),
                        unread_only: false,
                        count_only: false,
                        id: Some(*id),
                        after: None,
                        limit: 100,
                    },
                }
            }
        };
        self.reads.insert(
            token,
            PendingRead {
                token,
                epoch: self.read_epoch(&purpose),
                purpose,
                head_chain,
                followup: None,
                older_followup: None,
            },
        );
        effects.push(effect);
    }
    fn followup(&mut self, pending: PendingRead, effects: &mut Vec<ChatEffect>) {
        for (next, head_chain) in pending.followup.into_iter().chain(pending.older_followup) {
            if matches!(&next, ReadPurpose::Page { query, .. } if query.cursor.is_some())
                && pending.epoch != self.read_epoch(&next)
            {
                continue;
            }
            self.request_with_head(next, head_chain, effects);
        }
    }
    fn head(&mut self, effects: &mut Vec<ChatEffect>) {
        let query = history_query(
            &self.thread,
            (!self.current.search.is_empty()).then(|| self.current.search.clone()),
            None,
        );
        self.request(
            ReadPurpose::Page {
                thread: self.thread.clone(),
                query,
            },
            effects,
        );
    }
    pub(crate) fn view(&self) -> &View {
        &self.current
    }
    pub(crate) fn row(&self, id: &MessageId) -> Option<&InboxEntry> {
        self.current
            .ids
            .contains(id)
            .then(|| self.rows.get(id))
            .flatten()
    }
    pub(crate) fn view_rows(&self) -> impl Iterator<Item = &InboxEntry> {
        self.current.ids.iter().filter_map(|id| self.rows.get(id))
    }
    #[cfg(test)]
    pub(crate) fn view_mut(&mut self) -> &mut View {
        &mut self.current
    }
    pub(crate) fn draft_locked(&self) -> bool {
        self.locked.is_some()
    }
    pub(crate) fn has_unknown_send(&self) -> bool {
        self.submissions.values().any(|send| send.unknown)
    }
    fn mutations_allowed(&self) -> bool {
        self.online && self.compatible && self.initialized && self.tree_fresh && !self.bound
    }
    pub(crate) fn can_compose(&self) -> bool {
        self.mutations_allowed()
            && !self.draft_locked()
            && match &self.thread {
                ThreadKey::Direct(Address::Agent(agent)) => {
                    self.seats.iter().any(|s| &s.agent == agent)
                }
                _ => false,
            }
    }
    pub(crate) fn threads(&self) -> Vec<ThreadKey> {
        let mut list = vec![ThreadKey::ForYou];
        let mut addresses: BTreeSet<_> = self
            .seats
            .iter()
            .map(|s| Address::Agent(s.agent.clone()))
            .chain(self.unread_by_sender.keys().cloned())
            .collect();
        if let ThreadKey::Direct(address) = &self.thread {
            addresses.insert(address.clone());
        }
        list.extend(addresses.into_iter().map(ThreadKey::Direct));
        list.push(ThreadKey::All);
        list
    }
    pub(crate) fn open_thread(&mut self, thread: ThreadKey, effects: &mut Vec<ChatEffect>) {
        if thread == self.thread {
            return;
        }
        let old = std::mem::take(&mut self.current);
        self.cached.insert(self.thread.clone(), old);
        self.current = self.cached.remove(&thread).unwrap_or_default();
        self.thread = thread;
        self.presented.clear();
        self.mode = Mode::Normal;
        self.motion.reset();
        self.thread_index = self
            .threads()
            .iter()
            .position(|t| t == &self.thread)
            .unwrap_or(0);
        self.head(effects);
        if self.thread == ThreadKey::ForYou {
            self.request(ReadPurpose::UnreadSummary, effects);
        }
    }
    fn keep_selection(&mut self) {
        if !self
            .current
            .selected
            .is_some_and(|id| self.current.ids.contains(&id))
        {
            self.current.selected = self.current.ids.iter().next_back().copied();
            self.current.scroll = 0;
        }
    }
    fn install_page(&mut self, page: ChatPage, head: bool, read_token: OpToken) {
        let raw_low = page.rows.iter().map(|row| row.message_id).min();
        let gap = head
            && page.older.is_some()
            && raw_low
                .zip(self.current.ids.iter().next_back().copied())
                .is_some_and(|(low, high)| low > high);
        if gap {
            self.current.ids.clear();
            self.current.epoch = self.current.epoch.saturating_add(1);
            self.status = "history reloaded".into();
        }
        for row in page.rows {
            let id = row.message_id;
            self.merge_row(row, Some(page.head), read_token);
            if self.rows.get(&id).is_some_and(|row| {
                in_thread(&self.thread, row)
                    && (self.current.search.is_empty()
                        || row.content.body.contains(&self.current.search))
            }) {
                self.current.ids.insert(id);
            }
        }
        self.current.head = self.current.head.max(page.head);
        self.current.fence = page.fence;
        if head || page.older.is_some() || self.current.older.is_some() {
            self.current.older = page.older;
        }
        self.current.stale = false;
        self.keep_selection();
    }
    fn merge_row(&mut self, mut row: InboxEntry, history_head: Option<u64>, read_token: OpToken) {
        let id = row.message_id;
        if history_head.is_some_and(|head| self.row_heads.get(&id).is_some_and(|old| head < *old)) {
            return;
        }
        if let Some(old) = self.rows.get(&id) {
            if regresses(old, &row) {
                return;
            }
            row.read_at = old.read_at.or(row.read_at);
            // History has no live holds; retain the inbox overlay for this attempt.
            if history_head.is_some()
                && row.status == old.status
                && row.delivery.attempt == old.delivery.attempt
                && matches!(
                    row.status,
                    DeliveryStatus::Queued | DeliveryStatus::Delivering
                )
                && matches!(
                    old.delivery.phase,
                    DeliveryPhase::Waiting | DeliveryPhase::Blocked
                )
            {
                row.delivery = old.delivery.clone();
            }
        }
        if row.read_at.is_some() {
            self.ack_suppressed.remove(&id);
            self.unknown_acks.remove(&id);
        } else if self
            .unknown_acks
            .get(&id)
            .is_some_and(|floor| read_token > *floor)
            && !self.ack_pending.values().any(|ids| ids.contains(&id))
        {
            // Only a read started after the uncertain outcome can release its hold.
            self.unknown_acks.remove(&id);
            self.ack_suppressed.remove(&id);
        }
        if let Some(head) = history_head {
            self.row_heads
                .entry(id)
                .and_modify(|old| *old = (*old).max(head))
                .or_insert(head);
        }
        self.rows.insert(id, row);
    }
    fn recover_unknown(&mut self, effects: &mut Vec<ChatEffect>) {
        let targets: Vec<_> = self
            .submissions
            .iter()
            .filter(|(_, s)| s.unknown)
            .map(|(token, s)| (*token, s.target.clone()))
            .collect();
        for (send, target) in targets {
            self.request(ReadPurpose::Reconcile { send, target }, effects);
        }
        self.recover_acks(effects);
    }
    fn recover_acks(&mut self, effects: &mut Vec<ChatEffect>) {
        let ids: Vec<_> = self.unknown_acks.keys().copied().collect();
        for id in ids {
            self.request(ReadPurpose::Progress(id), effects);
        }
    }
    fn reconcile(&mut self, token: OpToken, rows: &[InboxEntry], effects: &mut Vec<ChatEffect>) {
        let found = self.submissions.get(&token).and_then(|send| {
            rows.iter()
                .find(|r| {
                    r.message_id.seq() > send.head
                        && !self.recorded_sends.contains(&r.message_id)
                        && r.content.from == Address::Owner
                        && r.content.to == send.target
                        && r.content.body == send.body
                })
                .map(|r| r.message_id)
        });
        if let Some(id) = found {
            self.committed(token, id, effects);
        }
    }
    fn committed(&mut self, token: OpToken, message_id: MessageId, effects: &mut Vec<ChatEffect>) {
        let Some(send) = self.submissions.remove(&token) else {
            return;
        };
        self.known_head = self.known_head.max(message_id.seq());
        let was_locked = self.locked == Some(token);
        if was_locked {
            self.locked = None;
        }
        let newer_draft = self
            .locked
            .and_then(|token| self.submissions.get(&token))
            .is_some_and(|newer| newer.thread == send.thread);
        let view = if self.thread == send.thread {
            Some(&mut self.current)
        } else {
            self.cached.get_mut(&send.thread)
        };
        if let Some(view) = view {
            if !newer_draft && view.draft == send.body {
                view.draft.clear();
            }
        }
        self.status = format!("sent {message_id} · delivery pending");
        if was_locked && self.thread == send.thread {
            self.mode = Mode::Normal;
        }
        if self.recorded_sends.insert(message_id) {
            effects.push(ChatEffect::RecordSend {
                token,
                message_id,
                word_count: send.body.split_whitespace().count(),
            });
        }
        self.head(effects);
        self.request(ReadPurpose::UnreadSummary, effects);
    }
    fn unlock_hint(&mut self, now: ChatTime) {
        if let Some(send) = self
            .locked
            .and_then(|t| self.submissions.get(&t))
            .filter(|s| s.unknown)
        {
            let left = send.dispatched.map(|at| {
                Duration::from_secs(10)
                    .saturating_sub(now.elapsed.saturating_sub(at))
                    .as_secs()
            });
            self.status = match left {
                Some(0) => "send may still arrive · Ctrl-r unlocks (duplicate risk)".into(),
                Some(n) => format!("send may still arrive · Ctrl-r unlock in {n}s"),
                None => "send may still arrive · awaiting dispatch attribution".into(),
            };
        }
    }
    fn note_message_ids(&mut self, rows: &[InboxEntry]) {
        self.known_head = self.known_head.max(
            rows.iter()
                .map(|row| row.message_id.seq())
                .max()
                .unwrap_or(0),
        );
    }
    fn observe_replies(&mut self, rows: &[InboxEntry]) {
        for row in rows {
            if let Address::Agent(agent) = &row.content.from {
                if row.content.to == Address::Owner
                    && self
                        .turn_heads
                        .get(agent)
                        .is_some_and(|head| row.message_id.seq() > *head)
                {
                    self.turn_replied.insert(agent.clone());
                    self.turn_notes.remove(agent);
                }
            }
        }
    }
    fn update_seats(&mut self, seats: Vec<SeatCard>) {
        for card in &seats {
            let previous = self.seats.iter().find(|s| s.agent == card.agent);
            let rebound = card.binding.session_id.is_none()
                || previous.is_none_or(|s| s.binding != card.binding);
            if rebound {
                self.turn_notes.remove(&card.agent);
                self.turn_heads.remove(&card.agent);
                self.turn_replied.remove(&card.agent);
            } else if self.online
                && previous.is_some_and(|s| s.working)
                && !card.working
                && !self.turn_replied.contains(&card.agent)
            {
                self.turn_notes.insert(
                    card.agent.clone(),
                    "turn ended; no message reply observed".into(),
                );
            }
            if card.working {
                self.turn_notes.remove(&card.agent);
                if rebound || previous.is_none_or(|s| !s.working) {
                    self.turn_heads
                        .insert(card.agent.clone(), self.watch_seq.max(self.current.head));
                    self.turn_replied.remove(&card.agent);
                }
            }
        }
        self.turn_notes
            .retain(|agent, _| seats.iter().any(|s| &s.agent == agent));
        self.turn_heads
            .retain(|agent, _| seats.iter().any(|s| &s.agent == agent));
        self.turn_replied
            .retain(|agent| seats.iter().any(|s| &s.agent == agent));
        self.seats = seats;
    }
    fn key(&mut self, key: KeyEvent, now: ChatTime, effects: &mut Vec<ChatEffect>) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('r') {
            let unlock = self
                .locked
                .and_then(|token| self.submissions.get(&token))
                .is_some_and(|s| {
                    s.unknown
                        && s.dispatched.is_some_and(|at| {
                            now.elapsed.saturating_sub(at) >= Duration::from_secs(10)
                        })
                });
            if unlock {
                self.locked = None;
                self.status = "send may still arrive · editing unlocked; duplicate risk".into();
            }
            return;
        }
        match self.mode {
            Mode::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Insert => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    return;
                }
                if self.draft_locked() {
                    return;
                }
                if key.code == KeyCode::Enter && !control {
                    self.send(effects);
                    return;
                }
                let text = if control && key.code == KeyCode::Char('j') {
                    Some('\n')
                } else if !control && !key.modifiers.contains(KeyModifiers::ALT) {
                    if let KeyCode::Char(c) = key.code {
                        Some(c)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(c) = text {
                    if self.current.draft.len().saturating_add(c.len_utf8()) <= 16384 {
                        self.current.draft.push(c);
                    } else {
                        self.status = "body limit: 16384 UTF-8 bytes".into();
                    }
                } else if key.code == KeyCode::Backspace {
                    self.current.draft.pop();
                }
                return;
            }
            Mode::Search => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        self.mode = Mode::Normal;
                        if self.current.search != self.search_edit {
                            self.current.search = self.search_edit.clone();
                            self.current.ids.clear();
                            self.current.selected = None;
                            self.current.scroll = 0;
                            self.current.older = None;
                            self.current.epoch = self.current.epoch.saturating_add(1);
                            self.head(effects);
                        }
                    }
                    KeyCode::Backspace => {
                        self.search_edit.pop();
                    }
                    KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                        if self.search_edit.len().saturating_add(c.len_utf8()) <= 256 {
                            self.search_edit.push(c);
                        } else {
                            self.status = "search limit: 256 UTF-8 bytes".into();
                        }
                    }
                    _ => {}
                }
                return;
            }
            Mode::Normal => {}
        }
        if !control {
            match key.code {
                KeyCode::Char('i') => {
                    if self.can_compose() {
                        self.mode = Mode::Insert;
                    } else {
                        self.status =
                            "read only: choose a known Direct seat from an unbound pane".into();
                    }
                    return;
                }
                KeyCode::Char('r') => {
                    let target = self
                        .current
                        .selected
                        .and_then(|id| self.row(&id))
                        .map(counterpart);
                    if let Some(target) = target {
                        self.open_thread(ThreadKey::Direct(target), effects);
                        if self.can_compose() {
                            self.mode = Mode::Insert;
                        }
                    }
                    return;
                }
                KeyCode::Char('/') => {
                    if self.thread == ThreadKey::ForYou {
                        let target = self
                            .current
                            .selected
                            .and_then(|id| self.row(&id))
                            .map(counterpart);
                        if let Some(target) = target {
                            self.open_thread(ThreadKey::Direct(target), effects);
                        }
                    }
                    self.search_edit = self.current.search.clone();
                    self.mode = Mode::Search;
                    return;
                }
                KeyCode::Char('y') => {
                    if let Some(row) = self.current.selected.and_then(|id| self.row(&id)) {
                        effects.push(ChatEffect::Copy {
                            body: row.content.body.clone(),
                        });
                    }
                    return;
                }
                KeyCode::Char('h') | KeyCode::Char('l') | KeyCode::Tab => {
                    self.column = match (self.column, key.code) {
                        (Column::Threads, KeyCode::Char('h')) => Column::Threads,
                        (Column::Messages, KeyCode::Char('h')) => Column::Threads,
                        (Column::Seats, KeyCode::Char('h')) => Column::Messages,
                        (Column::Threads, _) => Column::Messages,
                        (Column::Messages, _) => Column::Seats,
                        (Column::Seats, _) => Column::Threads,
                    };
                    return;
                }
                KeyCode::Char('J') | KeyCode::Char('K') => {
                    let threads: Vec<_> = self
                        .unread_by_sender
                        .keys()
                        .cloned()
                        .map(ThreadKey::Direct)
                        .collect();
                    let old = threads.iter().position(|t| t == &self.thread).unwrap_or(0);
                    let index = if key.code == KeyCode::Char('J') {
                        old.saturating_add(1)
                            .checked_rem(threads.len())
                            .unwrap_or(0)
                    } else {
                        old.checked_sub(1)
                            .unwrap_or(threads.len().saturating_sub(1))
                    };
                    if let Some(t) = threads.get(index) {
                        self.open_thread(t.clone(), effects);
                    }
                    return;
                }
                KeyCode::Enter => {
                    if self.column == Column::Threads {
                        if let Some(thread) = self.threads().get(self.thread_index) {
                            self.open_thread(thread.clone(), effects);
                            self.column = Column::Messages;
                        }
                    } else {
                        self.status = "pane jump is slice 2".into();
                    }
                    return;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.column = Column::Messages;
                    self.move_selection(
                        if key.code == KeyCode::Char('n') {
                            1
                        } else {
                            -1
                        },
                        effects,
                    );
                    return;
                }
                _ => {}
            }
        }
        match self.motion.handle(key) {
            Some(UiAction::Quit)
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers == KeyModifiers::CONTROL) =>
            {
                self.apply(ChatInput::Close, now, effects)
            }
            Some(UiAction::ToggleHelp) => self.mode = Mode::Help,
            Some(UiAction::MoveDown(n)) => {
                self.move_selection(n.min(isize::MAX as usize) as isize, effects)
            }
            Some(UiAction::MoveUp(n)) => {
                self.move_selection(-(n.min(isize::MAX as usize) as isize), effects)
            }
            Some(UiAction::GoToFirst) => self.move_selection(isize::MIN, effects),
            Some(UiAction::GoToLast) => self.move_selection(isize::MAX, effects),
            Some(UiAction::HalfPageDown(n)) => {
                self.current.scroll = self
                    .current
                    .scroll
                    .saturating_add(usize::from(self.size.1 / 2).saturating_mul(n))
                    .min(crate::screen::scroll_limit(self))
            }
            Some(UiAction::HalfPageUp(n)) => {
                self.current.scroll = self
                    .current
                    .scroll
                    .saturating_sub(usize::from(self.size.1 / 2).saturating_mul(n))
            }
            _ => {}
        }
    }
    fn send(&mut self, effects: &mut Vec<ChatEffect>) {
        if !self.tree_fresh
            && (self.ack_refusal.is_some()
                || self
                    .reads
                    .values()
                    .any(|read| matches!(read.purpose, ReadPurpose::Threads)))
        {
            self.status = "checking who you are; try again".into();
            return;
        }
        if !self.can_compose() || self.current.draft.is_empty() || self.current.draft.len() > 16384
        {
            return;
        }
        let ThreadKey::Direct(target) = self.thread.clone() else {
            return;
        };
        let Some(token) = self.token() else {
            return;
        };
        let body = self.current.draft.clone();
        self.submissions.insert(
            token,
            Submission {
                thread: self.thread.clone(),
                target: target.clone(),
                body: body.clone(),
                head: self.known_head.max(self.current.head),
                dispatched: None,
                unknown: false,
            },
        );
        self.locked = Some(token);
        self.status = "sending · draft locked".into();
        effects.push(ChatEffect::Send {
            token,
            params: SendParams {
                to: target,
                kind: MessageKind::Note,
                body,
                attachments: vec![],
                task: None,
            },
        });
    }
    fn move_selection(&mut self, delta: isize, effects: &mut Vec<ChatEffect>) {
        let len = match self.column {
            Column::Threads => self.threads().len(),
            Column::Messages => self.current.ids.len(),
            Column::Seats => self.seats.len(),
        };
        let old = match self.column {
            Column::Threads => self.thread_index,
            Column::Messages => self
                .current
                .ids
                .iter()
                .position(|id| Some(*id) == self.current.selected)
                .unwrap_or(0),
            Column::Seats => self.seat_index,
        };
        let index = old.saturating_add_signed(delta).min(len.saturating_sub(1));
        match self.column {
            Column::Threads => self.thread_index = index,
            Column::Seats => self.seat_index = index,
            Column::Messages => {
                self.current.selected = self.current.ids.iter().nth(index).copied();
                self.current.scroll = 0;
                if index == 0 && delta < 0 {
                    if let Some(cursor) = self.current.older.clone() {
                        let query = history_query(
                            &self.thread,
                            (!self.current.search.is_empty()).then(|| self.current.search.clone()),
                            Some(cursor),
                        );
                        self.request(
                            ReadPurpose::Page {
                                thread: self.thread.clone(),
                                query,
                            },
                            effects,
                        );
                    }
                }
            }
        }
    }
}
fn same_purpose(a: &ReadPurpose, b: &ReadPurpose) -> bool {
    match (a, b) {
        (ReadPurpose::Threads, ReadPurpose::Threads)
        | (ReadPurpose::UnreadSummary, ReadPurpose::UnreadSummary) => true,
        (ReadPurpose::Page { thread: a, .. }, ReadPurpose::Page { thread: b, .. }) => a == b,
        (ReadPurpose::Progress(a), ReadPurpose::Progress(b)) => a == b,
        (ReadPurpose::Reconcile { send: a, .. }, ReadPurpose::Reconcile { send: b, .. }) => a == b,
        _ => false,
    }
}
fn history_query(
    thread: &ThreadKey,
    text: Option<String>,
    cursor: Option<String>,
) -> HistoryParams {
    let (agent, correspondent) = match thread {
        ThreadKey::Direct(Address::Agent(agent)) => (Some(agent.clone()), Some(Address::Owner)),
        ThreadKey::Direct(other) => (None, Some(other.clone())),
        ThreadKey::ForYou => (None, Some(Address::Owner)),
        ThreadKey::All => (None, None),
    };
    HistoryParams {
        agent,
        correspondent,
        task: None,
        since: None,
        until: None,
        text,
        limit: 100,
        cursor,
    }
}
fn in_thread(thread: &ThreadKey, row: &InboxEntry) -> bool {
    match thread {
        ThreadKey::All => true,
        ThreadKey::ForYou => row.content.to == Address::Owner && row.read_at.is_none(),
        ThreadKey::Direct(other) => {
            (row.content.from == Address::Owner && &row.content.to == other)
                || (&row.content.from == other && row.content.to == Address::Owner)
        }
    }
}
fn counterpart(row: &InboxEntry) -> Address {
    if row.content.from == Address::Owner {
        row.content.to.clone()
    } else {
        row.content.from.clone()
    }
}
pub(crate) fn delivery_label(row: &InboxEntry) -> String {
    match row.status {
        DeliveryStatus::Delivered if row.read_at.is_some() => "seen (read)".into(),
        DeliveryStatus::Delivered => "delivered".into(),
        DeliveryStatus::Failed => format!(
            "failed: {}",
            row.delivery.reason.as_deref().unwrap_or("delivery failed")
        ),
        _ => match row.delivery.phase {
            DeliveryPhase::Waiting | DeliveryPhase::Blocked => {
                let mut label = format!(
                    "held: {}",
                    row.delivery.reason.as_deref().unwrap_or("waiting")
                );
                if let Some(at) = row.delivery.retry_at {
                    label.push_str(&format!(" · retry {}", at.format("%H:%M:%S")));
                }
                if let Some(id) = row.delivery.blocked_by {
                    label.push_str(&format!(" · behind {id}"));
                }
                label
            }
            DeliveryPhase::Delivering => "delivering".into(),
            _ => "queued".into(),
        },
    }
}
#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
