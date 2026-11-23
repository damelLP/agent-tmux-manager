//! Pure: hand-written facts, decided and folded in memory; one "bridge pass"
//! submits what `questions` and `replies` return, then the wake rows, as the
//! daemon's reconcile and repair do. No daemon, clock or waits.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use atm_org::{
    channel_query, ChannelCommand, DecisionCommand, DecisionKind, DecisionRequest,
    DecisionResolution, DecisionWithdrawal, OrgCommand, OrgEvent, OwnerCommand, Post,
    ProvisionalChoice, TaskActor, TaskCommand, TaskCreated, TaskKind, TaskMeasure,
};
use chrono::{DateTime, Utc};

fn at(minute: u32) -> DateTime<Utc> {
    format!("2026-01-15T12:{minute:02}:00Z").parse().unwrap()
}
fn addr(text: &str) -> Address {
    text.parse().unwrap()
}
/// The committed seq at the start, before every record of these worlds:
/// questions wake the owner.
const LIVE: u64 = 0;
/// After every record: questions were asked before this daemon started.
const RESTARTED: u64 = u64::MAX;

struct World {
    state: OrgState,
    seq: u64,
}
impl World {
    /// `p` has a lead (its channel is listed); `solo` has none.
    fn new() -> Self {
        let mut w = Self {
            state: OrgState::default(),
            seq: 0,
        };
        for project in ["p", "solo"] {
            w.apply(OrgEvent::ProjectRegistered {
                project_id: project.parse().unwrap(),
                root: format!("/{project}"),
            });
        }
        for (pane, id) in ["p/lead", "p/worker", "p/other", "solo/worker"]
            .into_iter()
            .enumerate()
        {
            w.apply(OrgEvent::AgentAdopted {
                agent_id: id.parse().unwrap(),
                pane: format!("%{pane}"),
                session_id: None,
            });
        }
        w
    }
    fn apply(&mut self, event: OrgEvent) -> u64 {
        self.seq += 1;
        self.state.apply(self.seq, at(1), &event).unwrap();
        self.seq
    }
    fn run_at(&mut self, command: OrgCommand, now: DateTime<Utc>) -> Vec<u64> {
        let events = self.state.decide(&command, now).unwrap();
        events.into_iter().map(|e| self.apply(e)).collect()
    }
    fn run(&mut self, command: OrgCommand) -> Vec<u64> {
        self.run_at(command, at(1))
    }
    fn decide(&mut self, command: DecisionCommand) -> Vec<u64> {
        self.run(OrgCommand::Decision(command))
    }
    fn ask(&mut self, request: DecisionRequest) -> DecisionId {
        let seqs = self.decide(DecisionCommand::Request(request));
        DecisionId::new(seqs[0])
    }
    fn away(&mut self) {
        self.run(OrgCommand::Owner(OwnerCommand::Away {
            by: Address::Owner,
            stacking_caps: [("p".parse().unwrap(), 5)].into(),
        }));
    }
    /// One pass: the question posts, then the replies on the folded state,
    /// then every missing wake row in the order the daemon's repair writes
    /// them. Returns the records written.
    fn bridge(&mut self, started: u64) -> Vec<u64> {
        let mut written = vec![];
        for question in questions(&self.state, started) {
            let post = OrgCommand::Channel(ChannelCommand::Post(Box::new(question)));
            written.extend(self.run(post));
        }
        for said in replies(&self.state) {
            let command = match said {
                Said::Post(reply) => OrgCommand::Channel(ChannelCommand::Post(reply)),
                Said::Message(message) => OrgCommand::SendMessage(*message),
            };
            written.extend(self.run(command));
        }
        for row in crate::org::channel_post::wake_rows(&self.state, None) {
            written.extend(self.run(OrgCommand::SendMessage(row)));
        }
        written
    }
    fn post(&self, decision: DecisionId, step: &str) -> &Post {
        let id = decision_post(&self.state, decision, step).unwrap_or_else(|| panic!("{step}"));
        channel_query::post(&self.state, id).unwrap()
    }
    fn rows(&self, post: &Post) -> Vec<MessageContent> {
        self.state
            .messages()
            .filter(|m| m.content.post == Some(post.id))
            .map(|m| m.content.clone())
            .collect()
    }
}

fn ask(requester: &str) -> DecisionRequest {
    DecisionRequest {
        requester: addr(requester),
        kind: DecisionKind::Plan,
        title: "Choose cache".into(),
        context: "Which size first".into(),
        files: vec!["plan.md".into()],
        options: vec!["small".into(), "large".into()],
        recommended: Some("small".into()),
        may_be_undone: true,
        changes_intent: true,
        deadline: Some(at(30)),
    }
}
fn answer(decision_id: DecisionId, option: &str, note: Option<&str>) -> DecisionCommand {
    DecisionCommand::Resolve(DecisionResolution {
        decision_id,
        option: option.into(),
        by: Address::Owner,
        note: note.map(Into::into),
        standing_rule: note.is_some(),
    })
}
fn take(decision_id: DecisionId) -> DecisionCommand {
    DecisionCommand::TakeProvisionally(ProvisionalChoice {
        relayed_by: None,
        decision_id,
        option: "small".into(),
        by: Address::System,
    })
}
/// Today's message from the bridge, as 445ddd24 builds it.
fn today(to: &str, kind: MessageKind, body: String, task: Option<TaskId>) -> MessageContent {
    MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: addr(to),
        kind,
        body,
        attachments: vec![],
        task,
        post: None,
    }
}

/// One walk per step, ending with the step's fact; the question was asked and
/// bridged first. Returns the world, the decision, and the step's kind and
/// text (the bodies are copied from 445ddd24's texts).
fn walk(step: &str) -> (World, DecisionId, MessageKind, String) {
    let mut w = World::new();
    let mut request = ask("p/worker");
    match step {
        "taken by relay" => {
            w.run(OrgCommand::Owner(OwnerCommand::SetRelay {
                by: Address::Owner,
                agent: Some("p/lead".parse().unwrap()),
            }));
        }
        "yours to decide" => {
            w.away();
            request.changes_intent = false;
        }
        "taken while away" => w.away(),
        "waits irreversible" => {
            w.away();
            request.may_be_undone = false;
        }
        "waits without recommendation" => {
            w.away();
            request.recommended = None;
        }
        _ => {}
    }
    let d = w.ask(request);
    w.bridge(LIVE);
    let (kind, body) = match step {
        "resolved" => {
            w.decide(answer(d, "small", Some("go")));
            (
                MessageKind::Answer,
                format!("Decision {d} resolved: small\nNote: go\nStanding rule: yes"),
            )
        }
        "overruled" => {
            w.decide(take(d));
            w.bridge(LIVE);
            w.decide(answer(d, "large", None));
            (
                MessageKind::Answer,
                format!("Decision {d} overruled: large\nStanding rule: no"),
            )
        }
        "taken by relay" => {
            w.decide(DecisionCommand::Relay {
                decision_id: d,
                option: "large".into(),
                by: "p/lead".parse().unwrap(),
            });
            (MessageKind::Answer, format!("Decision {d} taken provisionally: large\nRelayed by p/lead as the owner's spoken answer; he has not confirmed it himself. Finish your task in progress on this option. Start nothing further on it: a task built on it names {d} in rests_on and waits until the owner has confirmed {d}."))
        }
        "taken while away" => {
            w.run(OrgCommand::Owner(OwnerCommand::ApplyAway { decision_id: d }));
            (MessageKind::Answer, format!("Decision {d} taken provisionally: small\nThe owner is away. Finish your task in progress on this option. Start nothing further on it: a task built on it names {d} in rests_on and waits until the owner has seen {d}."))
        }
        "yours to decide" => {
            w.run(OrgCommand::Owner(OwnerCommand::ApplyAway { decision_id: d }));
            (MessageKind::Answer, format!("Decision {d} is yours to decide: recorded as small, your recommended option.\nThe owner is away and sees it when he is back."))
        }
        // The away switch came first: the first pass already said it.
        "waits irreversible" => (MessageKind::Note, format!("Decision {d} waits for the owner: he is away, and it cannot be undone or reaches outside.\nOnly work that depends on it waits. Carry on with other work.")),
        "waits without recommendation" => (MessageKind::Note, format!("Decision {d} waits for the owner: he is away, and it has no recommended option.\nTo have it handled while he is away, withdraw it and ask again with a recommended option.")),
        "expired" => {
            w.run_at(
                OrgCommand::Decision(DecisionCommand::Expire { decision_id: d }),
                at(30),
            );
            (MessageKind::Note, format!("Decision {d} expired: no answer by its deadline, 2026-01-15T12:30:00Z.\nIt is closed. Ask again if it still matters."))
        }
        "withdrawn" => {
            w.decide(DecisionCommand::Withdraw(DecisionWithdrawal {
                decision_id: d,
                by: addr("p/worker"),
                reason: Some("done".into()),
            }));
            (MessageKind::Note, format!("Decision {d} withdrawn by p/worker.\nReason: done"))
        }
        other => panic!("no walk {other}"),
    };
    (w, d, kind, body)
}

/// (walk, step tag, wakes the requester)
const STEPS: [(&str, &str, bool); 9] = [
    ("resolved", "resolved", true),
    ("overruled", "overruled", true),
    ("taken by relay", "taken", true),
    ("taken while away", "taken", true),
    ("yours to decide", "taken", true),
    ("waits irreversible", "waits", true),
    ("waits without recommendation", "waits", true),
    ("expired", "expired", true),
    ("withdrawn", "withdrawn", false),
];

// Break caught: a step posted outside its thread, by another author, with
// another text or kind, waking someone else or twice, or posted again.
#[test]
fn each_step_posts_one_system_reply_with_todays_text_and_one_wake() {
    for (walk_name, step, wakes) in STEPS {
        let (mut w, d, kind, body) = walk(walk_name);
        let asked = w.post(d, ASKED).id;
        let before = w.seq;
        let written = w.bridge(LIVE);
        let (post, rows) = (w.post(d, step), w.rows(w.post(d, step)));
        if step != "waits" {
            assert_eq!(written.first(), Some(&(before + 1)), "{walk_name}");
            assert_eq!(post.id.seq(), before + 1, "{walk_name}: the reply first");
        }
        let c = &post.content;
        assert_eq!(c.author, Address::System, "{walk_name}");
        assert_eq!(c.thread, Some(asked), "{walk_name}");
        assert_eq!(c.channel, ChannelId::Project("p".parse().unwrap()));
        assert_eq!(
            (c.kind, c.body.as_str()),
            (kind, body.as_str()),
            "{walk_name}"
        );
        assert_eq!((c.task.clone(), c.attachments.len()), (None, 0));
        assert!(!c.route, "{walk_name}");
        let wake = if wakes {
            vec![addr("p/worker")]
        } else {
            vec![]
        };
        assert_eq!(c.wake, wake, "{walk_name}");
        assert_eq!(rows.len(), wake.len(), "{walk_name}");
        assert_eq!(w.bridge(LIVE), Vec::<u64>::new(), "{walk_name}: said twice");
        // Nothing left to submit either: a said step is not offered again
        // each pass for the policy's repeat key to drop (mutant D9).
        assert_eq!(questions(&w.state, LIVE), vec![], "{walk_name}");
        assert_eq!(replies(&w.state), vec![], "{walk_name}");
    }
}

// The rollback property (design 12): each wake row without `post` is, field
// for field, the message 445ddd24's bridge builds, so its content dedupe finds
// it. Break caught: any drift in sender, recipient, kind, text, task or files.
#[test]
fn wake_rows_without_post_equal_todays_bridge_messages() {
    for (walk_name, step, wakes) in STEPS.into_iter().filter(|s| s.2) {
        let (mut w, d, kind, body) = walk(walk_name);
        w.bridge(LIVE);
        let rows = w.rows(w.post(d, step));
        let bare: Vec<_> = rows
            .into_iter()
            .map(|row| MessageContent { post: None, ..row })
            .collect();
        assert_eq!(
            bare,
            [today("p/worker", kind, body, None)],
            "{walk_name} {wakes}"
        );
    }
}

/// 445ddd24's list (content dedupe apart from `post`), for walks without redo
/// tasks; the redo test compares its rows with today's notes instead.
fn previous_bridge(state: &OrgState) -> Vec<MessageContent> {
    crate::org::decisions::missing_resolution_messages(state)
        .into_iter()
        .chain(away::missing_notices(state))
        .collect()
}

// Break caught: the previous binary, rolled back to after the replies, finds a
// step it would still send.
#[test]
fn previous_bridge_finds_nothing_to_send_after_the_replies() {
    for (walk_name, ..) in STEPS {
        let (mut w, ..) = walk(walk_name);
        w.bridge(LIVE);
        assert_eq!(previous_bridge(&w.state), vec![], "{walk_name}");
    }
}

fn redo_world() -> (World, DecisionId, [TaskId; 2]) {
    let mut w = World::new();
    let d = w.ask(ask("p/worker"));
    w.bridge(LIVE);
    let mut tasks = vec![];
    for (creator, agent) in [(Address::Owner, "p/other"), (addr("p/lead"), "p/worker")] {
        let seq = w.run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
            project_id: None,
            title: "Build cache".into(),
            kind: TaskKind::Work,
            creator: creator.clone(),
            parent: None,
            judges: None,
            measure: TaskMeasure::Owner,
            budget: None,
            files: vec![],
            rests_on: Some(d),
        })))[0];
        let task: TaskId = format!("t{seq}").parse().unwrap();
        w.run(OrgCommand::Task(TaskCommand::Assign {
            task_id: task.clone(),
            agent_id: agent.parse().unwrap(),
            by: TaskActor {
                who: Address::Owner,
                manager_of: None,
            },
        }));
        tasks.push(task);
    }
    w.decide(take(d));
    w.bridge(LIVE);
    w.decide(answer(d, "large", None));
    (w, d, tasks.try_into().unwrap())
}

// Break caught: redo notes sent as messages beside the thread, one reply per
// recipient instead of per task, or rows that are not today's two notes.
#[test]
fn overrule_replies_once_plus_once_per_redo_task_waking_creator_and_assignee() {
    let (mut w, d, tasks) = redo_world();
    w.bridge(LIVE);
    let asked = w.post(d, ASKED).id;
    assert_eq!(w.post(d, "overruled").content.wake, [addr("p/worker")]);
    for (task, creator, assignee) in [
        (&tasks[0], "owner", "p/other"),
        (&tasks[1], "p/lead", "p/worker"),
    ] {
        let reply = w.post(d, &format!("redo:{task}"));
        let body = format!("Task {task} needs redo after decision {d} was overruled: large\nIt was reopened and unassigned. Read the new answer before assigning or submitting it again. No files were reverted.");
        assert_eq!(reply.content.author, Address::System);
        assert_eq!(reply.content.thread, Some(asked));
        assert_eq!(reply.content.task.as_ref(), Some(task));
        assert_eq!(reply.content.wake, [addr(creator), addr(assignee)]);
        let bare: Vec<_> = w
            .rows(reply)
            .into_iter()
            .map(|row| MessageContent { post: None, ..row })
            .collect();
        assert_eq!(
            bare,
            [creator, assignee].map(|to| today(
                to,
                MessageKind::Note,
                body.clone(),
                Some(task.clone())
            ))
        );
    }
    assert!(w.state.redo_notices().is_empty());
    let posts = |w: &World| {
        w.state
            .messages()
            .filter(|m| m.content.post.is_none() && m.content.from == Address::System)
            .count()
    };
    assert_eq!(posts(&w), 0, "no bare message beside the thread");
    assert_eq!(w.bridge(LIVE), Vec::<u64>::new());
}

// Break caught: the confirm is not said, or the provisional take is said again.
#[test]
fn relay_then_confirm_gives_two_replies_and_a_final_decision() {
    let (mut w, d, ..) = walk("taken by relay");
    w.bridge(LIVE);
    w.decide(answer(
        d,
        "large",
        Some("confirmed in his words, relayed by p/lead: 'large'"),
    ));
    w.bridge(LIVE);
    assert_eq!(
        w.state.decision(&d).unwrap().status,
        DecisionStatus::Resolved
    );
    let steps: Vec<_> = w
        .state
        .messages()
        .filter(|m| m.content.to == addr("p/worker"))
        .map(|m| channel_query::post(&w.state, m.content.post.unwrap()).unwrap())
        .map(|p| p.content.decision.clone().unwrap().step)
        .collect();
    assert_eq!(steps, ["taken", "resolved"]);
}

// Break caught: a down requester blocks its question (at the switch or after
// a restart), or its question wakes it.
#[test]
fn question_post_for_a_down_requester_is_written_at_the_switch() {
    let mut w = World::new();
    let d = w.ask(ask("p/worker"));
    w.apply(OrgEvent::AgentDown {
        agent_id: "p/worker".parse().unwrap(),
        reason: "gone".into(),
    });
    w.bridge(RESTARTED);
    let asked = w.post(d, ASKED);
    assert_eq!(asked.content.author, addr("p/worker"));
    assert_eq!(asked.content.wake, Vec::<Address>::new());
    // Its replies still reach it: a tag of a down seat queues.
    w.decide(answer(d, "small", None));
    w.bridge(RESTARTED);
    assert_eq!(w.post(d, "resolved").content.wake, [addr("p/worker")]);
}

// Break caught: a closed decision without a question post gets one, a thread
// reply, or a new kind of text; or its missing answer is not sent at all.
#[test]
fn closed_legacy_decisions_get_nothing_new_but_a_missing_answer_still_goes_as_a_dm() {
    let mut w = World::new();
    let resolved = w.ask(ask("p/worker"));
    w.decide(answer(resolved, "small", None));
    let withdrawn = w.ask(ask("p/worker"));
    w.decide(DecisionCommand::Withdraw(DecisionWithdrawal {
        decision_id: withdrawn,
        by: Address::Owner,
        reason: None,
    }));
    let expired = w.ask(ask("p/worker"));
    w.run_at(
        OrgCommand::Decision(DecisionCommand::Expire {
            decision_id: expired,
        }),
        at(30),
    );
    let message = today(
        "p/worker",
        MessageKind::Answer,
        format!("Decision {resolved} resolved: small\nStanding rule: no"),
        None,
    );
    assert_eq!(questions(&w.state, RESTARTED), vec![]);
    assert_eq!(
        replies(&w.state),
        [Said::Message(Box::new(message.clone()))]
    );
    w.bridge(RESTARTED);
    let rows: Vec<_> = w.state.messages().map(|m| m.content.clone()).collect();
    assert_eq!(rows, [message]);
    assert_eq!(w.bridge(RESTARTED), Vec::<u64>::new());
    for d in [resolved, withdrawn, expired] {
        assert_eq!(decision_post(&w.state, d, ASKED), None);
    }
}

// Break caught: an open legacy decision gets no card, its question wakes the
// owner or is routed, or its earlier DM is said again in the thread.
#[test]
fn open_legacy_decision_gets_a_question_post_at_start_and_its_earlier_dm_is_not_repeated() {
    let mut w = World::new();
    let d = w.ask(ask("p/worker"));
    w.decide(take(d));
    let notice = away::notice(w.state.decision(&d).unwrap(), false).unwrap();
    w.apply(OrgEvent::MessageSent(notice));
    w.bridge(RESTARTED);
    let asked = w.post(d, ASKED).clone();
    assert!(!asked.content.route);
    assert_eq!(asked.content.wake, Vec::<Address>::new());
    assert_eq!(decision_post(&w.state, d, "taken"), None);
    assert_eq!(w.state.messages().count(), 1, "no row for the question");
    w.decide(answer(d, "small", None));
    w.bridge(RESTARTED);
    assert_eq!(w.post(d, "resolved").content.thread, Some(asked.id));
}

// Break caught: the listed project channel is passed over, an unlisted one is
// used, or the owner's DM fallback wakes him when it must not.
#[test]
fn question_goes_to_the_listed_project_channel_else_the_owner_dm() {
    let mut w = World::new();
    let listed = w.ask(ask("p/worker"));
    let solo = w.ask(ask("solo/worker"));
    let late = w.ask(ask("solo/worker"));
    w.bridge(LIVE);
    let dm = ChannelId::direct(addr("solo/worker"), Address::Owner).unwrap();
    assert_eq!(
        w.post(listed, ASKED).content.channel,
        ChannelId::Project("p".parse().unwrap())
    );
    let solo_post = w.post(solo, ASKED).content.clone();
    assert_eq!(solo_post.channel, dm);
    assert_eq!(solo_post.wake, [Address::Owner]);
    assert_eq!(solo_post.body, "Choose cache\n\nWhich size first");
    assert_eq!(solo_post.kind, MessageKind::Question);
    assert_eq!(solo_post.attachments, ["plan.md"]);
    // The thread of a DM question takes system's replies too.
    w.decide(answer(late, "small", None));
    w.bridge(LIVE);
    let reply = w.post(late, "resolved");
    assert_eq!(reply.content.channel, dm);
    assert_eq!(reply.content.wake, [addr("solo/worker")]);
}

// Break caught: a system decision (journal only: the RPC refuses it) is asked
// elsewhere, wakes nobody, or gets replies today's bridge never sent.
#[test]
fn system_requested_decision_asks_in_the_owner_system_channel_and_gets_no_replies() {
    let mut w = World::new();
    let mut request = ask("p/worker");
    request.requester = Address::System;
    request.context = String::new();
    let d = DecisionId::new(w.apply(OrgEvent::DecisionRequested(request)));
    w.bridge(LIVE);
    let asked = w.post(d, ASKED).content.clone();
    assert_eq!(
        asked.channel,
        ChannelId::direct(Address::Owner, Address::System).unwrap()
    );
    assert_eq!(
        (asked.author, asked.wake),
        (Address::System, vec![Address::Owner])
    );
    assert_eq!(asked.body, "Choose cache");
    w.decide(answer(d, "small", None));
    assert_eq!(replies(&w.state), vec![]);
    // Nor the steps today's bridge never said: a withdrawal, an expiry.
    let mut request = ask("p/worker");
    request.requester = Address::System;
    let withdrawn = DecisionId::new(w.apply(OrgEvent::DecisionRequested(request.clone())));
    let expired = DecisionId::new(w.apply(OrgEvent::DecisionRequested(request)));
    w.bridge(LIVE);
    w.decide(DecisionCommand::Withdraw(DecisionWithdrawal {
        decision_id: withdrawn,
        by: Address::Owner,
        reason: None,
    }));
    w.run_at(
        OrgCommand::Decision(DecisionCommand::Expire {
            decision_id: expired,
        }),
        at(30),
    );
    assert_eq!(replies(&w.state), vec![]);
}

// The lead's Q3 ruling: a decision asked before this daemon started already
// pinged the owner and shows through decision.list. Break caught: its question
// writes an owner row, is routed (stopgap or router), or a live one stops
// waking him.
#[test]
fn pre_start_question_wakes_nobody_and_is_not_routed() {
    for requester in ["p/worker", "solo/worker"] {
        let mut w = World::new();
        let d = w.ask(ask(requester));
        let written = w.bridge(RESTARTED);
        let asked = w.post(d, ASKED).content.clone();
        assert_eq!(written.len(), 1, "{requester}: the post alone");
        assert_eq!(asked.wake, Vec::<Address>::new(), "{requester}");
        assert_eq!(asked.mentions, vec![], "{requester}");
        assert!(!asked.route, "{requester}");
        assert_eq!(channel_query::pending_routes(&w.state).count(), 0);
        assert_eq!(w.state.messages().count(), 0, "{requester}");

        let mut live = World::new();
        let d = live.ask(ask(requester));
        live.bridge(LIVE);
        assert_eq!(live.post(d, ASKED).content.wake, [Address::Owner]);
        let rows = live.rows(live.post(d, ASKED));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].from.clone(), rows[0].to.clone()),
            (addr(requester), Address::Owner)
        );
    }
}

// A redo note the old binary already sent to one recipient (a rollback in
// between): the reply wakes only the other, so nobody gets it twice. Break
// caught: tags from every note instead of the unsent ones.
#[test]
fn a_redo_note_one_recipient_already_has_wakes_only_the_other() {
    let (mut w, d, tasks) = redo_world();
    let sent = w
        .state
        .redo_notices()
        .into_iter()
        .find(|n| n.task.as_ref() == Some(&tasks[0]) && n.to == Address::Owner)
        .unwrap();
    w.apply(OrgEvent::MessageSent(sent));
    w.bridge(LIVE);
    let reply = w.post(d, &format!("redo:{}", tasks[0]));
    assert_eq!(reply.content.wake, [addr("p/other")]);
    assert_eq!(w.rows(reply).len(), 1);
    assert_eq!(
        w.post(d, &format!("redo:{}", tasks[1])).content.wake,
        [addr("p/lead"), addr("p/worker")]
    );
}

// Review N1, the race it names: the owner is away and a seat asks an
// irreversible decision between a pass's question snapshot and its reply
// snapshot. Break caught: its "waits" goes out as a bare DM, so its thread
// never gets it; only a closed legacy decision gets today's messages.
#[test]
fn an_open_decision_without_its_question_gets_no_dm() {
    let mut w = World::new();
    w.away();
    let mut request = ask("p/worker");
    request.may_be_undone = false;
    let d = w.ask(request);
    assert_eq!(replies(&w.state), vec![], "no DM before its question");
    w.bridge(LIVE);
    let waits = w.post(d, "waits");
    assert_eq!(waits.content.thread, decision_post(&w.state, d, ASKED));
    assert_eq!(waits.content.wake, [addr("p/worker")]);
    let bare = w
        .state
        .messages()
        .filter(|m| m.content.post.is_none())
        .count();
    assert_eq!(bare, 0);
}

// Review N6. No source note has files today, but a reply copies its note,
// and its rows copy the reply. Break caught: files dropped from the reply, so
// its rows would no longer equal the note (the rollback property).
#[test]
fn a_reply_carries_its_notes_attachments() {
    let mut w = World::new();
    let d = w.ask(ask("p/worker"));
    w.bridge(LIVE);
    w.decide(answer(d, "small", None));
    let note = MessageContent {
        attachments: vec!["proof.md".into()],
        ..today(
            "p/worker",
            MessageKind::Answer,
            format!("Decision {d} resolved: small\nStanding rule: no"),
            None,
        )
    };
    let step = Step::of(d, "resolved".into(), vec![note.clone()]).unwrap();
    let said = unsaid(&w.state, step).unwrap();
    let [Said::Post(reply)] = said.as_slice() else {
        panic!("one reply: {said:?}");
    };
    assert_eq!(reply.attachments, ["proof.md"]);
    w.run(OrgCommand::Channel(ChannelCommand::Post(reply.clone())));
    let rows = crate::org::channel_post::wake_rows(&w.state, None);
    assert_eq!(
        rows.into_iter()
            .map(|row| MessageContent { post: None, ..row })
            .collect::<Vec<_>>(),
        [note]
    );
}
