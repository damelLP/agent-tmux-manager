//! Pure chat/controller boundary. Canonical messages are imported, never copied.

use std::time::Duration;

use atm_comms::message_types::{
    AckParams, AckResult, InboxEntry, InboxParams, InboxResult, SendParams, SendResult,
};
use atm_comms::{Address, AgentId, AgentStatus, HistoryParams, MessageId};
use chrono::{DateTime, Utc};
use crossterm::event::KeyEvent;
use ratatui::style::Color;
use thiserror::Error;

/// Unique within one ChatState run. Trace joins also require the controller's run ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpToken(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThreadKey {
    ForYou,
    All,
    Direct(Address),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Threads,
    Messages,
    Seats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Search,
    Help,
}

#[derive(Debug, Clone, Default)]
pub struct ChatConfig {
    pub initial_thread: Option<Address>,
    pub caller_pane: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatTime {
    pub utc: DateTime<Utc>,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct ThreadFacts {
    pub tree_seq: u64,
    pub seats: Vec<SeatCard>,
    pub waiting_decisions: usize,
    pub decisions_truncated: bool,
    pub caller_bound: bool,
}

/// Session IDs are opaque canonical values supplied by the binary, not reconstructed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatBinding {
    pub pane: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeatCard {
    pub agent: AgentId,
    pub state: AgentStatus,
    pub binding: SeatBinding,
    pub activity_label: String,
    pub activity_color: Color,
    pub working: bool,
    pub context_percentage: Option<f64>,
}

/// Screen projection, not a serialized copy of the daemon's private envelope.
#[derive(Debug, Clone)]
pub struct ChatPage {
    pub rows: Vec<InboxEntry>,
    pub head: u64,
    pub fence: u64,
    pub older: Option<String>,
}

/// IDs qualify only when their LAST wrapped body line was drawn successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameReport {
    pub generation: u64,
    pub thread: ThreadKey,
    pub visible_ids: Vec<MessageId>,
    pub read_ids: Vec<MessageId>,
    pub can_compose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChatFailure {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    TooOld(String),
    #[error("{0}")]
    OutcomeUnknown(String),
}

/// Distinct read scopes; completion tokens never alias summary/progress/recovery.
#[derive(Debug, Clone)]
pub enum ReadPurpose {
    Threads,
    Page {
        thread: ThreadKey,
        query: HistoryParams,
    },
    UnreadSummary,
    Progress(MessageId),
    Reconcile {
        send: OpToken,
        target: Address,
    },
}

#[derive(Debug, Clone)]
pub enum ChatInput {
    Start,
    ThreadsLoaded(OpToken, Result<ThreadFacts, ChatFailure>),
    PageLoaded(OpToken, Result<ChatPage, ChatFailure>),
    InboxLoaded(OpToken, Result<InboxResult, ChatFailure>),
    WatchEvent {
        seq: u64,
        kind: String,
    },
    Connection {
        online: bool,
        methods: Vec<String>,
        watch_available: bool,
    },
    Key(KeyEvent),
    Resize {
        width: u16,
        height: u16,
    },
    SendDispatched(OpToken),
    SendResult(OpToken, Result<SendResult, ChatFailure>),
    AckResult(OpToken, Result<AckResult, ChatFailure>),
    SessionUpdate(Vec<SeatCard>),
    Presented {
        generation: u64,
        visible_ids: Vec<MessageId>,
        visible: bool,
        hint: Option<String>,
    },
    CopyResult(Result<(), ChatFailure>),
    Tick,
    Close,
}

#[derive(Debug, Clone)]
pub enum ChatEffect {
    LoadThreads {
        token: OpToken,
    },
    LoadPage {
        token: OpToken,
        query: HistoryParams,
    },
    LoadInbox {
        token: OpToken,
        params: InboxParams,
    },
    Send {
        token: OpToken,
        params: SendParams,
    },
    Ack {
        token: OpToken,
        params: AckParams,
    },
    Watch {
        topics: Vec<String>,
        cursor: u64,
    },
    Unwatch,
    Copy {
        body: String,
    },
    RecordSend {
        token: OpToken,
        message_id: MessageId,
        word_count: usize,
    },
    Quit {
        error: Option<ChatFailure>,
    },
}
