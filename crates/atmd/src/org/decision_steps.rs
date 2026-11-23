//! What the decision bridge still has to say (design 12): a question post for
//! each open decision, and one reply in its thread per step. Pure: the bridge
//! submits what these return, and `decide` refuses a second post of a step.
//!
//! A step's reply keeps today's sender (`system`), kind, body and task, and
//! wakes exactly the recipients today's message went to, so each wake row is
//! that message plus `post`. The previous binary dedupes by content apart from
//! `post` and therefore sends nothing again after a rollback.
//!
//! | Step | Text | Wakes |
//! |---|---|---|
//! | `asked` | the title, then a blank line and the context when there is one | the owner, unless asked before this daemon started |
//! | `resolved`, `overruled` | the bridge's answer message | the requester |
//! | `taken`, `waits` | the away notices | the requester |
//! | `redo:<task>` | the redo note, one body for both | the task's creator (the owner for `system`) and assignee |
//! | `expired` | "Decision {id} expired: no answer by its deadline, {deadline}." and a line to ask again | the requester |
//! | `withdrawn` | "Decision {id} withdrawn by {by}." and the reason when given | nobody |
//!
//! A closed decision without a question post is a legacy one: it keeps
//! today's messages exactly, and gets no `expired` or `withdrawn` text. An
//! open one's steps wait for its question, which every pass writes first. A
//! step is due when no post shows it and some recipient has no message with
//! its content yet (a DM an open decision got before the switch counts as
//! said).
//!
//! Known limits, reasoned and unverified:
//! - An answer between `decision.requested` and the question post closes the
//!   decision before it has one: it gets no card, and its answer goes as a DM.
//! - A stop between a reply and its wake rows, then a rollback to the
//!   previous binary: its bridge may send the bare message before its repair
//!   pass writes the row, which repeats that one message.
//! - A question for a decision requested before a stop and posted after the
//!   restart wakes nobody (asked before this daemon started); its card shows
//!   through `decision.list`.

use super::{away, decisions::resolution_message};
use atm_org::{
    channel_policy::GroupDirectory,
    channel_query::{decision_post, listed},
    Address, ChannelId, Decision, DecisionId, DecisionStatus, DecisionStep, MessageContent,
    MessageKind, OrgState, PostRequest, PostTarget, TaskId,
};
use chrono::SecondsFormat;
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "decision_steps_tests.rs"]
mod tests;

/// The step of a decision's question post.
pub(super) const ASKED: &str = "asked";

/// One thing to submit: a post, or a legacy decision's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Said {
    Post(Box<PostRequest>),
    Message(Box<MessageContent>),
}

/// One step: what is said, and today's message to each recipient.
struct Step {
    decision: DecisionId,
    name: String,
    kind: MessageKind,
    body: String,
    attachments: Vec<String>,
    task: Option<TaskId>,
    notes: Vec<MessageContent>,
    /// Today's bridge says it, so a legacy decision still gets it.
    legacy: bool,
}
impl Step {
    fn of(decision: DecisionId, name: String, notes: Vec<MessageContent>) -> Option<Self> {
        let first = notes.first()?;
        Some(Self {
            decision,
            name,
            kind: first.kind,
            body: first.body.clone(),
            attachments: first.attachments.clone(),
            task: first.task.clone(),
            legacy: true,
            notes,
        })
    }
}

/// The question post of every open decision that has none. One asked before
/// this daemon started (its id, the seq of its request, at or before
/// `started`, the committed seq then) wakes nobody. Seqs, not clocks: a
/// clock stepped back between runs cannot make an old decision look new.
pub(super) fn questions(state: &OrgState, started: u64) -> Vec<PostRequest> {
    state
        .decisions()
        .filter(|d| is_open(d))
        .filter(|d| decision_post(state, d.id, ASKED).is_none())
        .filter_map(|d| question(state, d, d.id.seq() > started))
        .collect()
}

/// A decision's question post, in its requester's project channel when that
/// is listed, else in the requester's DM with the owner (`system`'s for a
/// decision `system` asked). Never routed.
pub(super) fn question(
    state: &OrgState,
    decision: &Decision,
    wake_owner: bool,
) -> Option<PostRequest> {
    let requester = &decision.request.requester;
    let channel = match requester {
        Address::Agent(id) => Some(ChannelId::Project(id.project.clone()))
            .filter(|project| listed(state, project))
            .or_else(|| ChannelId::direct(requester.clone(), Address::Owner))?,
        _ => ChannelId::direct(Address::System, Address::Owner)?,
    };
    let mut body = decision.request.title.clone();
    if !decision.request.context.is_empty() {
        body.push_str("\n\n");
        body.push_str(&decision.request.context);
    }
    Some(PostRequest {
        target: PostTarget::Channel(channel),
        author: requester.clone(),
        kind: MessageKind::Question,
        body,
        attachments: decision.request.files.clone(),
        task: None,
        tags: if wake_owner {
            vec!["owner".into()]
        } else {
            vec![]
        },
        groups: GroupDirectory::new(),
        decision: Some(step(decision.id, ASKED)),
        client_key: None,
        also_in_chat: false,
        no_route: true,
        internal: true,
    })
}

/// Every step still to say: a reply for a decision with a question post,
/// today's message for one without.
pub(super) fn replies(state: &OrgState) -> Vec<Said> {
    let away = state.owner().away_since.is_some();
    let mut steps = Vec::new();
    for decision in state.decisions() {
        if let Some(note) = resolution_message(decision) {
            let name = match decision.status {
                DecisionStatus::Overruled => "overruled",
                _ => "resolved",
            };
            steps.extend(Step::of(decision.id, name.into(), vec![note]));
        }
        if let Some(note) = away::notice(decision, away) {
            let name = match decision.status {
                DecisionStatus::Pending => "waits",
                _ => "taken",
            };
            steps.extend(Step::of(decision.id, name.into(), vec![note]));
        }
        steps.extend(closing(decision));
    }
    let mut redo: BTreeMap<(DecisionId, Option<TaskId>), Vec<MessageContent>> = BTreeMap::new();
    for (decision, note) in state.redo_notices_by_decision() {
        redo.entry((decision, note.task.clone()))
            .or_default()
            .push(note);
    }
    for ((decision, task), notes) in redo {
        let name = format!("redo:{}", task.map(|t| t.to_string()).unwrap_or_default());
        steps.extend(Step::of(decision, name, notes));
    }
    steps
        .into_iter()
        .filter_map(|step| unsaid(state, step))
        .flatten()
        .collect()
}

/// The new steps of a native decision that today's bridge does not say.
fn closing(decision: &Decision) -> Option<Step> {
    let Address::Agent(_) = &decision.request.requester else {
        return None;
    };
    let id = decision.id;
    let (name, body, notes) = match decision.status {
        DecisionStatus::Expired => {
            let deadline = decision.request.deadline?;
            let body = format!(
                "Decision {id} expired: no answer by its deadline, {}.\nIt is closed. Ask again if it still matters.",
                deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
            );
            let note = MessageContent {
                delivery_notice: None,
                from: Address::System,
                to: decision.request.requester.clone(),
                kind: MessageKind::Note,
                body: body.clone(),
                attachments: vec![],
                task: None,
                post: None,
            };
            ("expired", body, vec![note])
        }
        DecisionStatus::Withdrawn => {
            let withdrawal = decision.withdrawal.as_ref()?;
            let mut body = format!("Decision {id} withdrawn by {}.", withdrawal.by);
            if let Some(reason) = &withdrawal.reason {
                body.push_str(&format!("\nReason: {reason}"));
            }
            ("withdrawn", body, vec![])
        }
        _ => return None,
    };
    Some(Step {
        decision: id,
        name: name.into(),
        kind: MessageKind::Note,
        body,
        attachments: vec![],
        task: None,
        notes,
        legacy: false,
    })
}

/// What a step still needs: a reply in the question's thread waking those
/// without today's message, or a legacy decision's missing messages.
fn unsaid(state: &OrgState, step: Step) -> Option<Vec<Said>> {
    let unsent: Vec<MessageContent> = step
        .notes
        .iter()
        .filter(|note| !state.messages().any(|m| m.content.same_except_post(note)))
        .cloned()
        .collect();
    let Some(root) = decision_post(state, step.decision, ASKED) else {
        // An open decision waits for its question, which every pass writes
        // first; only a closed legacy decision gets today's messages.
        let open = state.decision(&step.decision).is_some_and(is_open);
        return (step.legacy && !open).then(|| {
            unsent
                .into_iter()
                .map(|m| Said::Message(Box::new(m)))
                .collect()
        });
    };
    if decision_post(state, step.decision, &step.name).is_some()
        || (unsent.is_empty() && !step.notes.is_empty())
    {
        return None;
    }
    // `owner`, or a seat's long form, which resolves from any channel.
    let tags = unsent.iter().map(|note| note.to.to_string()).collect();
    Some(vec![Said::Post(Box::new(PostRequest {
        target: PostTarget::Reply(root),
        author: Address::System,
        kind: step.kind,
        body: step.body,
        attachments: step.attachments,
        task: step.task,
        tags,
        groups: GroupDirectory::new(),
        decision: Some(DecisionStep {
            decision_id: step.decision,
            step: step.name,
        }),
        client_key: None,
        also_in_chat: false,
        no_route: true,
        internal: true,
    }))])
}

/// Pending or taken provisionally: it has, or is about to get, a question.
pub(super) fn is_open(decision: &Decision) -> bool {
    matches!(
        decision.status,
        DecisionStatus::Pending | DecisionStatus::TakenProvisionally
    )
}

fn step(decision_id: DecisionId, step: &str) -> DecisionStep {
    DecisionStep {
        decision_id,
        step: step.into(),
    }
}
