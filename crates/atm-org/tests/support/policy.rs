//! Builders for the channel policy tests: a wider org than `channel.rs`'s, and
//! helpers that decide a command and fold what it emits. Hand-written facts;
//! no daemon, clock or harness.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test builders only
use crate::channel_support::*;
use atm_core::SessionId;
use atm_org::channel_policy::*;
use atm_org::*;

pub fn agent_id(text: &str) -> AgentId {
    text.parse().unwrap()
}
pub fn addrs(list: &[&str]) -> Vec<Address> {
    list.iter().map(|a| addr(a)).collect()
}
pub fn seat(text: &str) -> Mention {
    Mention::Seat(agent_id(text))
}
pub fn group(project: &str, name: &str) -> Mention {
    Mention::Group {
        project: project.parse().unwrap(),
        name: name.into(),
    }
}
pub fn groups(entries: &[(&str, &str, &[&str])]) -> GroupDirectory {
    entries
        .iter()
        .map(|(project, name, seats)| {
            (
                (project.parse().unwrap(), (*name).to_owned()),
                seats.iter().map(|s| agent_id(s)).collect(),
            )
        })
        .collect()
}
pub fn refused(refusal: ChannelRefusal) -> Refusal {
    Refusal::Channel(refusal)
}

pub fn in_channel(text: &str) -> PostTarget {
    PostTarget::Channel(channel(text))
}
pub fn direct(to: &str) -> PostTarget {
    PostTarget::Direct(addr(to))
}
pub fn request(target: PostTarget, author: &str, body: &str) -> PostRequest {
    PostRequest {
        target,
        author: addr(author),
        kind: MessageKind::Note,
        body: body.into(),
        attachments: vec![],
        task: None,
        tags: vec![],
        groups: GroupDirectory::new(),
        decision: None,
        client_key: None,
        also_in_chat: false,
        no_route: false,
        internal: false,
    }
}
pub fn posting(request: PostRequest) -> OrgCommand {
    OrgCommand::Channel(ChannelCommand::Post(Box::new(request)))
}
/// The one post a decision emitted.
pub fn the_post(events: &[OrgEvent]) -> &PostContent {
    match events {
        [OrgEvent::ChannelPosted(content)] => content,
        other => panic!("expected one post, got {other:?}"),
    }
}

pub fn stopgap(cause: &str) -> RouteBy {
    RouteBy::Stopgap {
        cause: cause.into(),
    }
}
pub fn router() -> RouteBy {
    RouteBy::Router {
        name: "fake".into(),
        model: None,
    }
}
pub fn routing(post: MessageId, to: &[&str], by: RouteBy) -> OrgCommand {
    OrgCommand::Channel(ChannelCommand::Route(PostRouted {
        post,
        wake: addrs(to),
        by,
        reason: None,
    }))
}

impl Org {
    /// Route a pending post by the stopgap's rule, as the daemon will; whom
    /// it woke.
    pub fn stopgap_route(&mut self, post: MessageId) -> Vec<Address> {
        let wake = stopgap_wake(&self.state, post);
        let command = OrgCommand::Channel(ChannelCommand::Route(PostRouted {
            post,
            wake: wake.clone(),
            by: stopgap("disabled"),
            reason: None,
        }));
        self.run(command).unwrap();
        wake
    }
    pub fn register(&mut self, project: &str) {
        self.apply(OrgEvent::ProjectRegistered {
            project_id: project.parse().unwrap(),
            root: format!("/{project}"),
        });
    }
    /// Ready with a session, starting without one.
    pub fn adopt(&mut self, id: &str, session: Option<&str>) {
        self.apply(OrgEvent::AgentAdopted {
            agent_id: agent_id(id),
            pane: format!("%{}", id.replace('/', "-")),
            session_id: session.map(SessionId::new),
        });
    }
    pub fn down(&mut self, id: &str) {
        self.apply(OrgEvent::AgentDown {
            agent_id: agent_id(id),
            reason: "gone".into(),
        });
    }
    pub fn stuck(&mut self, id: &str) {
        self.apply(OrgEvent::AgentStuck {
            agent_id: agent_id(id),
            reason: "dialog".into(),
        });
    }
    pub fn clearing(&mut self, id: &str) {
        self.apply(OrgEvent::AgentClearRequested {
            agent_id: agent_id(id),
        });
    }
    pub fn decide(&self, command: &OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
        self.state.decide(command, at(self.seq))
    }
    /// Decide, then fold each emitted fact; each with the id it took.
    pub fn run(&mut self, command: OrgCommand) -> Result<Vec<(MessageId, OrgEvent)>, Refusal> {
        let events = self.decide(&command)?;
        Ok(events
            .into_iter()
            .map(|event| (self.apply(event.clone()), event))
            .collect())
    }
    /// Decide a post that must be accepted and fold it; its id.
    pub fn posted(&mut self, request: PostRequest) -> MessageId {
        let events = self.run(posting(request)).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        events[0].0
    }
}

/// `atm` with `lead`, `build-2`, `build-4` ready and `build-6` down; `web` with
/// `lead` and `build-1` ready; `solo` registered with no seat.
pub fn org() -> Org {
    let mut org = Org::new();
    org.adopt("atm/build-6", Some("s-build-6"));
    org.down("atm/build-6");
    org.register("web");
    org.adopt("web/lead", Some("s-web-lead"));
    org.adopt("web/build-1", Some("s-web-build-1"));
    org.register("solo");
    org
}
