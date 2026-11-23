//! Builders for the channel tests: one project, three seats, hand-written facts.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test builders only
use atm_core::SessionId;
use atm_org::channel_query::*;
use atm_org::*;
use chrono::{DateTime, Duration, Utc};

pub fn at(seq: u64) -> DateTime<Utc> {
    "2026-11-20T08:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::seconds(seq as i64)
}
pub fn addr(text: &str) -> Address {
    text.parse().unwrap()
}
pub fn channel(text: &str) -> ChannelId {
    ChannelId::from(text.to_owned())
}

/// Project `atm` with `lead`, `build-2` and `build-4` adopted at sequences 2-4.
pub struct Org {
    pub state: OrgState,
    pub seq: u64,
}
impl Org {
    pub fn new() -> Self {
        let mut org = Self {
            state: OrgState::default(),
            seq: 0,
        };
        org.apply(OrgEvent::ProjectRegistered {
            project_id: "atm".parse().unwrap(),
            root: "/atm".into(),
        });
        for (index, name) in ["lead", "build-2", "build-4"].into_iter().enumerate() {
            org.apply(OrgEvent::AgentAdopted {
                agent_id: format!("atm/{name}").parse().unwrap(),
                pane: format!("%{index}"),
                session_id: Some(SessionId::new(format!("s-{name}"))),
            });
        }
        org
    }
    pub fn apply(&mut self, event: OrgEvent) -> MessageId {
        self.seq += 1;
        self.state.apply(self.seq, at(self.seq), &event).unwrap();
        MessageId::new(self.seq)
    }
    /// Applies at the next sequence; on refusal the state must be unchanged.
    pub fn try_apply(&mut self, event: OrgEvent) -> Result<(), ApplyError> {
        let before = self.state.clone();
        let seq = self.seq + 1;
        let result = self.state.apply(seq, at(seq), &event);
        match result {
            Ok(()) => self.seq = seq,
            Err(_) => assert_eq!(self.state, before, "refused {event:?} changed state"),
        }
        result
    }
    pub fn post(&mut self, content: PostContent) -> MessageId {
        self.apply(OrgEvent::ChannelPosted(content))
    }
    pub fn send(&mut self, from: &str, to: &str, post: Option<MessageId>) -> MessageId {
        self.apply(OrgEvent::MessageSent(MessageContent {
            from: addr(from),
            to: addr(to),
            kind: MessageKind::Note,
            body: "hi".into(),
            attachments: vec![],
            task: None,
            delivery_notice: None,
            post,
        }))
    }
    /// Write every due wake, as the daemon's repair pass will.
    pub fn wake_all(&mut self) -> Vec<MessageId> {
        missing_wakes(&self.state)
            .into_iter()
            .map(|row| self.apply(OrgEvent::MessageSent(row)))
            .collect()
    }
    pub fn decision(&mut self) -> DecisionId {
        let id = self.apply(OrgEvent::DecisionRequested(DecisionRequest {
            requester: addr("atm/lead"),
            kind: DecisionKind::Plan,
            title: "W2 order".into(),
            context: "which first".into(),
            files: vec![],
            options: vec!["a".into(), "b".into()],
            recommended: Some("a".into()),
            may_be_undone: true,
            changes_intent: false,
            deadline: None,
        }));
        DecisionId::new(id.seq())
    }
}

pub fn note(channel_text: &str, author: &str) -> PostContent {
    PostContent {
        channel: channel(channel_text),
        author: addr(author),
        session_id: None,
        kind: MessageKind::Note,
        body: "hold W2".into(),
        attachments: vec!["plan.md".into()],
        task: Some("t9".parse().unwrap()),
        thread: None,
        mentions: vec![],
        wake: vec![],
        route: false,
        decision: None,
        client_key: None,
        also_in_chat: false,
    }
}
pub fn waking(mut post: PostContent, to: &[&str]) -> PostContent {
    post.wake = to.iter().map(|t| addr(t)).collect();
    post
}
pub fn routed(post: MessageId, to: &[&str], cause: &str) -> OrgEvent {
    OrgEvent::ChannelRouted(PostRouted {
        post,
        wake: to.iter().map(|t| addr(t)).collect(),
        by: RouteBy::Stopgap {
            cause: cause.into(),
        },
        reason: None,
    })
}
pub fn due(state: &OrgState) -> Vec<(MessageId, String)> {
    missing_wakes(state)
        .into_iter()
        .map(|row| (row.post.unwrap(), row.to.to_string()))
        .collect()
}
