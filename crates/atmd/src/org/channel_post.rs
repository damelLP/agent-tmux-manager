//! `channel.post` (design 6.1, 6.3 and 7): the post, then one wake row per
//! address it wakes, then the owner heal.
//!
//! The rules are atm-org's. A dry run of `decide` on the folded state tells
//! which seats the post wakes by rule, for msg.send's send-time check; every
//! write then goes through the one writer, whose `decide` writes no second row
//! for a (post, recipient) pair. The post and its rows are separate appends, so
//! a crash or a dropped request can leave rows missing; the inline path and the
//! repair pass both write only what `missing_wakes` lists.
//!
//! The owner heal (the lead's note N6): a wake row to the owner written after
//! his marker in that channel passed its post is read at once, by U3's `Seen`
//! at his current marker.
//!
//! Where a post can stop, and what heals it:
//!
//! | Stop | Left behind | Healed by |
//! |---|---|---|
//! | Crash or append error after the post | rows missing | the startup pass |
//! | After some rows | the rest missing | the startup pass |
//! | Timeout, or the request dropped | rows missing, writer live | the pass the post's own `channel.posted` triggers |
//! | Between his row and its heal | his row unread behind his marker | the startup pass, which heals every unread owner wake |
//! | Inline and repair at once | nothing | `decide`: the second write of a pair is no event |
//!
//! Cost per post, accepted at today's volume: the inline path and the pass its
//! event triggers each clone the whole state once or twice and may both submit
//! the post's rows (up to 2N `SendMessage` for N wakes, the second N writing
//! nothing); the owner's marker is read through every channel's summary.

use super::{
    channels::org_error,
    message_inbox::caller_address,
    message_methods::validate_content,
    messages::{MessageClock, MessageError, MessageHandle, Service},
    role_directory::{directory_projects, never_started, role_directory, RoleDirectory},
    tasks::TaskError,
    OrgHandle, OrgServiceError,
};
use crate::{
    eventlog::Envelope,
    server::methods::{CallContext, MethodError},
};
use atm_comms::channel_types::{
    ChannelPostParams, ChannelPostResult, PostRouting, UnavailableRow, UnresolvedReason,
    UnresolvedRow, WokeRow,
};
use atm_org::{
    channel_policy::{resolve_mentions, GroupDirectory, Unresolvable},
    channel_query::{self, channels_for, client_key_post, missing_wakes, wake_for, ListScope},
    Address, AgentStatus, ChannelCommand, ChannelId, ChannelRefusal, MessageContent, MessageId,
    MessageKind, OrgCommand, OrgEvent, OrgState, PostRequest, PostTarget, Refusal, Routing,
};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Post a validated automatic task notice, returning the legacy inbox row's ID.
/// Unlike RPC posts, these have a fixed audience and no send-time session check.
pub(super) async fn post_task_notice(
    org: &OrgHandle,
    mut content: MessageContent,
    now: DateTime<Utc>,
) -> Result<MessageId, TaskError> {
    let project = match (content.kind, &content.from, &content.to) {
        (MessageKind::Report, Address::Agent(submitter), _) => Some(&submitter.project),
        (_, _, Address::Agent(recipient)) => Some(&recipient.project),
        _ => None,
    };
    let post = |target| {
        OrgCommand::Channel(ChannelCommand::Post(Box::new(PostRequest {
            target,
            author: content.from.clone(),
            kind: content.kind,
            body: content.body.clone(),
            attachments: content.attachments.clone(),
            task: content.task.clone(),
            tags: vec![content.to.to_string()],
            groups: GroupDirectory::new(),
            decision: None,
            client_key: None,
            also_in_chat: false,
            no_route: true,
            internal: true,
        })))
    };
    let direct = || post(PostTarget::Direct(content.to.clone()));
    let committed = if let Some(project) = project {
        let preferred = post(PostTarget::Channel(ChannelId::Project(project.clone())));
        match org.execute(preferred, now).await {
            Err(OrgServiceError::Refused(Refusal::Channel(
                ChannelRefusal::UnknownChannel(_) | ChannelRefusal::NotMember(_),
            ))) => org.execute(direct(), now).await?,
            result => result?,
        }
    } else {
        org.execute(direct(), now).await?
    };
    let post = committed
        .first()
        .filter(|e| e.kind == "channel.posted")
        .map(|e| MessageId::new(e.seq))
        .ok_or_else(|| TaskError::Internal("task notice committed no post".into()))?;
    content.post = Some(post);
    let rows = org
        .execute(OrgCommand::SendMessage(content.clone()), now)
        .await?;
    let wake = match rows.as_slice() {
        [row] if row.kind == "msg.sent" => Some(MessageId::new(row.seq)),
        // The repair can write the row before the inline command decides.
        [] => wake_for(&org.snapshot().await?, post, &content.to),
        _ => None,
    }
    .ok_or_else(|| TaskError::Internal(format!("task post {post} has no confirmed wake")))?;
    if content.to == Address::Owner {
        let heal = async {
            for command in owner_heals(&org.snapshot().await?, &[content]) {
                org.execute(command, now).await?;
            }
            Ok::<(), OrgServiceError>(())
        };
        if let Err(error) = heal.await {
            tracing::warn!(%wake, %error, "Task wake committed but owner read heal not confirmed");
        }
    }
    Ok(wake)
}

/// A post is one fold wait, a session read per seat it wakes by rule, the
/// post, one append per wake and the heal; the router allows 5 s more.
const POST_BUDGET: Duration = Duration::from_secs(20);

/// Strict tags per post. Each tag's length is bounded by atm-org's id grammar,
/// which the policy applies: a tag no seat or group could have is refused.
const MAX_TAGS: usize = 32;

impl MessageHandle {
    /// Write a post and its wakes; answers once they are folded. A request
    /// cannot mark its post internal: the params have no such field.
    pub(crate) async fn post(
        &self,
        p: ChannelPostParams,
        caller: CallContext,
    ) -> Result<ChannelPostResult, MethodError> {
        let target = validate_post(&p)?;
        let service = self.live()?;
        service
            .call(POST_BUDGET, posted(service, p, target, caller))
            .await
            .map_err(post_error)
    }
}

async fn posted(
    service: &Service,
    p: ChannelPostParams,
    target: PostTarget,
    caller: CallContext,
) -> Result<ChannelPostResult, MessageError> {
    let now = service.rt.clock.now()?;
    // Admission must include lifecycle changes committed since startup.
    service.rt.history.folded(service.rt.org.status()?).await?;
    let projects = service.read(|state, _| directory_projects(state, &target, &p.body, &p.tags))?;
    let directory = role_directory(service.notice_files.clone(), projects).await;
    let (request, rule_seats) = service.read(|state, _| {
        let author = caller_address(state, &caller)?;
        let request = request(p, target.clone(), author, directory.groups.clone());
        let command = OrgCommand::Channel(ChannelCommand::Post(Box::new(request.clone())));
        let seats = match state
            .decide(&command, now)
            .map_err(|refusal| tag_refusal(refusal, state, &target, &directory))?
            .as_slice()
        {
            [OrgEvent::ChannelPosted(post)] => post
                .wake
                .iter()
                .filter_map(|to| match to {
                    Address::Agent(id) => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(), // a repeat writes nothing
        };
        Ok::<_, MessageError>((request, seats))
    })??;
    for id in &rule_seats {
        if service.session_unconfirmed(id).await? {
            return Err(MessageError::Refused(format!(
                "agent {id} has a discovered process but its recorded session is not confirmed; post not written; inspect atm list and atm agent list before retrying"
            )));
        }
    }
    let committed = service
        .execute(OrgCommand::Channel(ChannelCommand::Post(Box::new(
            request.clone(),
        ))))
        .await?;
    let post = match committed.iter().find(|e| e.kind == "channel.posted") {
        Some(envelope) => MessageId::new(envelope.seq),
        None => {
            // A repeated client key: name the first post, and finish its wakes.
            service.rt.history.folded(service.rt.org.status()?).await?;
            service
                .read(|state, _| {
                    request
                        .client_key
                        .as_deref()
                        .and_then(|key| client_key_post(state, &request.author, key))
                })?
                .ok_or_else(|| MessageError::Internal("a repeated post names no post".into()))?
        }
    };
    let rows = service.read(|state, _| wake_rows(state, Some(post)))?;
    for row in &rows {
        service
            .execute(OrgCommand::SendMessage(row.clone()))
            .await?;
    }
    for heal in service.read(|state, _| owner_heals(state, &rows))? {
        service.execute(heal).await?;
    }
    service.rt.history.folded(service.rt.org.status()?).await?;
    service
        .read(|state, seq| post_result(state, seq, post, &request.groups))?
        .ok_or_else(|| MessageError::Internal(format!("post {post} is not in the fold")))
}

/// The limits `msg.send` applies, a tag count, and the one target.
fn validate_post(p: &ChannelPostParams) -> Result<PostTarget, MethodError> {
    if p.also_in_chat.is_some() && p.reply_to.is_none() {
        return Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "also_in_chat requires reply_to",
        ));
    }
    if p.tags.len() > MAX_TAGS {
        return Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "at most 32 tags are allowed",
        ));
    }
    validate_content(&p.body, &p.attachments, p.task.as_ref())?;
    match (&p.channel, &p.to, &p.reply_to) {
        (Some(channel), None, None) => Ok(PostTarget::Channel(channel.clone())),
        (None, Some(to), None) => Ok(PostTarget::Direct(to.clone())),
        (None, None, Some(id)) => Ok(PostTarget::Reply(*id)),
        _ => Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "exactly one of channel, to and reply_to is required",
        )),
    }
}

/// The policy's request: never internal, no decision step, routed when
/// untagged, with the role groups read for its tags (U10).
fn request(
    p: ChannelPostParams,
    target: PostTarget,
    author: Address,
    groups: GroupDirectory,
) -> PostRequest {
    PostRequest {
        target,
        author,
        kind: p.kind,
        body: p.body,
        attachments: p.attachments,
        task: p.task,
        tags: p.tags,
        groups,
        decision: None,
        client_key: p.client_key,
        also_in_chat: p.also_in_chat.unwrap_or_default(),
        no_route: false,
        internal: false,
    }
}

fn refused(refusal: Refusal) -> MessageError {
    MessageError::Organisation(OrgServiceError::Refused(refusal))
}

/// A strict tag that resolved to nothing: a seat with a role file and no
/// record has never been started (design 9.4); otherwise the refusal names
/// the role files that could not be read, if any (9.3).
fn tag_refusal(
    refusal: Refusal,
    state: &OrgState,
    target: &PostTarget,
    directory: &RoleDirectory,
) -> MessageError {
    let Refusal::Channel(ChannelRefusal::UnknownTag {
        tag,
        reason: Unresolvable::Unknown,
    }) = &refusal
    else {
        return refused(refusal);
    };
    if let Some(seat) = never_started(state, target, tag, directory) {
        return MessageError::InvalidParams(format!(
            "tag {tag:?}: {seat} has a role file but has never been started, so nothing can be addressed to it until it is; post not written"
        ));
    }
    if directory.problems.is_empty() {
        return refused(refusal);
    }
    MessageError::InvalidParams(format!(
        "{refusal}; role files could not be read: {}",
        directory.problems.join("; ")
    ))
}

/// The rows still missing for `post` (for every post when `None`), each post's
/// in the order it wakes them.
pub(super) fn wake_rows(state: &OrgState, post: Option<MessageId>) -> Vec<MessageContent> {
    let mut rows: Vec<MessageContent> = missing_wakes(state)
        .into_iter()
        .filter(|row| post.is_none() || row.post == post)
        .collect();
    rows.sort_by_key(|row| {
        let place = row
            .post
            .and_then(|id| channel_query::post(state, id))
            .and_then(|p| p.woke().position(|to| *to == row.to));
        (row.post, place)
    });
    rows
}

/// Whether the owner has been shown `post` in its channel: his marker there is
/// at or past it.
pub(super) fn owner_has_seen(state: &OrgState, post: MessageId) -> bool {
    let Some(channel) = channel_query::post(state, post).map(|p| &p.content.channel) else {
        return false;
    };
    owner_marker(state, channel).is_some_and(|through| through >= post)
}

fn owner_marker(state: &OrgState, channel: &atm_org::ChannelId) -> Option<MessageId> {
    channels_for(state, &Address::Owner, ListScope::All)
        .into_iter()
        .find(|row| row.id == *channel)
        .and_then(|row| row.seen_through)
}

/// One `Seen` at his current marker per channel where a row to him, now
/// written, delivers a post he has already been shown (N6).
pub(super) fn owner_heals(state: &OrgState, rows: &[MessageContent]) -> Vec<OrgCommand> {
    let mut heals = Vec::new();
    for post in rows
        .iter()
        .filter(|row| row.to == Address::Owner)
        .filter_map(|row| row.post)
        .filter(|post| owner_has_seen(state, *post))
    {
        let Some(channel) = channel_query::post(state, post).map(|p| p.content.channel.clone())
        else {
            continue;
        };
        let Some(through) = owner_marker(state, &channel) else {
            continue;
        };
        let heal = OrgCommand::Channel(ChannelCommand::Seen {
            channel,
            reader: Address::Owner,
            through,
        });
        if !heals.contains(&heal) {
            heals.push(heal);
        }
    }
    heals
}

/// The answer for a post, read from state alone: a new post and a repeat get
/// the same shape.
fn post_result(
    state: &OrgState,
    seq: u64,
    id: MessageId,
    groups: &GroupDirectory,
) -> Option<ChannelPostResult> {
    let post = channel_query::post(state, id)?;
    let mut woke = Vec::new();
    let mut unavailable = Vec::new();
    for to in post.woke() {
        let Some(row) = wake_for(state, id, to) else {
            continue;
        };
        woke.push(WokeRow {
            to: to.clone(),
            message_id: row,
        });
        let Address::Agent(seat) = to else {
            continue;
        };
        if let Some(agent) = state
            .agent(seat)
            .filter(|a| matches!(a.status, AgentStatus::Down | AgentStatus::Stuck))
        {
            unavailable.push(UnavailableRow {
                to: seat.clone(),
                state: agent.status,
                // The record's last change. Every status change sets it; so does a
                // pane created with no status change (state.rs, AgentPaneCreated),
                // which a down or stuck seat is not expected to see afterwards.
                since: agent.updated_at,
                message_id: row,
            });
        }
    }
    // Inline tags only: a strict tag that did not resolve refused the post.
    let unresolved = resolve_mentions(
        state,
        &post.content.channel,
        &post.content.body,
        &[],
        groups,
    )
    .map(|resolution| resolution.unresolved)
    .unwrap_or_default()
    .into_iter()
    .map(|token| UnresolvedRow {
        text: token.text,
        reason: match token.reason {
            Unresolvable::Malformed => UnresolvedReason::Malformed,
            Unresolvable::Unknown => UnresolvedReason::Unknown,
            Unresolvable::Ambiguous => UnresolvedReason::Ambiguous,
            Unresolvable::EmptyGroup => UnresolvedReason::EmptyGroup,
        },
    })
    .collect();
    Some(ChannelPostResult {
        seq,
        post_id: id,
        channel: post.content.channel.clone(),
        thread: post.content.thread,
        mentions: post.content.mentions.clone(),
        woke,
        unavailable,
        route: match post.routing() {
            Routing::None | Routing::Unknown => PostRouting::None,
            Routing::Pending => PostRouting::Pending,
            Routing::Router => PostRouting::Router,
            Routing::Stopgap => PostRouting::Stopgap,
        },
        unresolved,
    })
}

/// U4's channel codes and the existing org mapping, plus two of a post's own:
/// a strict tag that does not resolve and a recipient that cannot be woken
/// are the caller's parameters.
fn post_error(error: MessageError) -> MethodError {
    match error {
        MessageError::Organisation(OrgServiceError::Refused(
            refusal @ (Refusal::Channel(ChannelRefusal::UnknownTag { .. })
            | Refusal::InvalidRecipient(_)),
        )) => MethodError::new(error_code::INVALID_PARAMS, refusal.to_string()),
        MessageError::Organisation(error) => org_error(error),
        error => error.into(),
    }
}

/// One repair pass: every missing wake row, each post's in its wake order,
/// then the owner heals for the rows it wrote, or at `startup` for every
/// unread owner wake (a crash or a dropped request can fall between his row
/// and its heal). Idempotent: `decide` writes nothing for a pair that already
/// has its row, and a heal reads only his unread rows. A refused row or heal
/// (none is expected: rows are copies of their post, heals stand at his
/// marker) is logged and left for the next pass, never retried in a loop.
/// Returns the records written.
pub(crate) async fn repair(
    org: &OrgHandle,
    now: DateTime<Utc>,
    startup: bool,
) -> Result<usize, OrgServiceError> {
    let rows = wake_rows(&org.snapshot().await?, None);
    let mut written = 0;
    for row in &rows {
        let result = org.execute(OrgCommand::SendMessage(row.clone()), now).await;
        written += unless_refused(result, || {
            format!("wake row of {:?} to {}", row.post, row.to)
        })?;
    }
    if !rows.is_empty() || startup {
        let state = org.snapshot().await?;
        let candidates = if startup {
            unread_owner_wakes(&state)
        } else {
            rows
        };
        for heal in owner_heals(&state, &candidates) {
            let result = org.execute(heal, now).await;
            written += unless_refused(result, || "owner heal".to_owned())?;
        }
    }
    Ok(written)
}

/// Every wake row to the owner that he has not read.
fn unread_owner_wakes(state: &OrgState) -> Vec<MessageContent> {
    state
        .messages()
        .filter(|m| m.content.to == Address::Owner && m.content.post.is_some())
        .filter(|m| m.read_at.is_none())
        .map(|m| m.content.clone())
        .collect()
}

/// The records a write committed; a refusal is logged as zero.
fn unless_refused(
    result: Result<Vec<Envelope>, OrgServiceError>,
    what: impl FnOnce() -> String,
) -> Result<usize, OrgServiceError> {
    match result {
        Ok(events) => Ok(events.len()),
        Err(OrgServiceError::Refused(refusal)) => {
            tracing::error!(%refusal, "Refused {}; left for the next repair pass", what());
            Ok(0)
        }
        Err(error) => Err(error),
    }
}

/// One pass, retried while the writer is healthy. A timeout, or a full request
/// queue (`Unavailable` while the writer's status is fine), backs off and tries
/// again: every write is decide-checked, so a retry writes nothing twice. Only
/// a writer that has stopped ends it.
pub(super) async fn pass(
    org: &OrgHandle,
    now: DateTime<Utc>,
    startup: bool,
) -> Result<(), OrgServiceError> {
    loop {
        match repair(org, now, startup).await {
            Err(error @ (OrgServiceError::Timeout | OrgServiceError::Unavailable(_)))
                if org.status().is_ok() =>
            {
                tracing::warn!(%error, "Wake repair pass incomplete; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            result => return result.map(|_| ()),
        }
    }
}

/// The repair task: watch new posts and routing answers from the committed
/// cursor, then repair once (after startup replay) and after each such event.
/// The receiver holds the cursor the last finished pass covered, or why the
/// task stopped. Watching before the first snapshot closes the gap between
/// them, as the decision bridge does.
pub(crate) fn spawn_wake_repair(
    org: OrgHandle,
    clock: MessageClock,
    cancel: CancellationToken,
) -> (watch::Receiver<Result<u64, String>>, JoinHandle<()>) {
    let (health, status) = watch::channel(Err("wake repair has not run".to_owned()));
    let watched = org.status().and_then(|cursor| {
        org.watch(
            vec!["channel.posted".into(), "channel.routed".into()],
            cursor,
        )
        .map(|events| (cursor, events))
    });
    let (mut cursor, mut events) = match watched {
        Ok(watched) => watched,
        Err(error) => {
            let _ = health.send_replace(Err(error.to_string()));
            return (status, tokio::spawn(async {}));
        }
    };
    let task = tokio::spawn(async move {
        let work = async {
            let mut startup = true;
            loop {
                let now = clock.now().map_err(|error| error.to_string())?;
                pass(&org, now, std::mem::take(&mut startup))
                    .await
                    .map_err(|error| error.to_string())?;
                let _ = health.send_replace(Ok(cursor));
                let Some(event) = events.next().await.map_err(|error| error.to_string())? else {
                    return Err("channel watch closed".to_owned());
                };
                cursor = event.seq;
            }
        };
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(()),
            result = work => result,
        };
        let reason = match result {
            Ok(()) => "wake repair stopped".to_owned(),
            Err(error) => {
                tracing::warn!(%error, "Wake repair stopped; restart to recover");
                error
            }
        };
        let _ = health.send_replace(Err(reason));
    });
    (status, task)
}
