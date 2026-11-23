//! One channel post; the daemon owns mentions, permissions and wakes.

use super::{agent_target, msg, output::stdout, rpc, spawn::non_empty_env};
use anyhow::{ensure, Context, Result};
use atm_comms::channel_types::{
    ChannelEditParams, ChannelMutationResult, ChannelPostParams, ChannelPostResult,
    ChannelReactParams, ChannelRemoveParams, PostRouting,
};
use atm_org::{
    Address, AgentStatus, ChannelId, MessageId, MessageKind, ProjectId, ReactionEmoji, TaskId,
};
use atmd::{
    control::reservation::PaneId,
    org::{
        agents::{ListParams, ListResult},
        messages::MessageError,
    },
};
use clap::{ArgGroup, Args};
use std::{fmt::Write as _, time::Duration};

// 25 s router + 5 s startup/transport margin. Never retry a mutation.
const POST_TIMEOUT: Duration = Duration::from_secs(30);
const EXPLICIT_TARGET: &str =
    "atm post without a target requires a named agent; use --channel, --to, or --reply";

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").args(["channel", "to", "reply", "edit", "remove"])))]
pub(super) struct PostArgs {
    /// Post in this project channel (default: the named caller's project).
    #[arg(long)]
    channel: Option<ProjectId>,
    /// Post directly to this address and wake it.
    #[arg(long, value_parser = agent_target::parse_address)]
    to: Option<String>,
    /// Reply in a thread; accepts a post or wake ID.
    #[arg(long)]
    reply: Option<MessageId>,
    /// Replace the text of your post, without waking anyone.
    #[arg(long, conflicts_with_all = ["kind", "task", "tags", "attachments", "also_in_chat"])]
    edit: Option<MessageId>,
    /// Remove your post's text and leave a removed line.
    #[arg(long, conflicts_with_all = ["body", "kind", "task", "tags", "attachments", "also_in_chat"])]
    remove: Option<MessageId>,
    /// Also show this reply in the main chat.
    #[arg(long, requires = "reply", conflicts_with_all = ["channel", "to"])]
    also_in_chat: bool,
    /// Strict tag without the @; repeat to tag more than one address.
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long, default_value = "note", value_parser = parse_kind)]
    kind: MessageKind,
    #[arg(long)]
    task: Option<TaskId>,
    #[arg(long = "attach")]
    attachments: Vec<String>,
    #[arg(long, required_unless_present = "remove")]
    body: Option<String>,
    #[arg(long, value_enum, default_value = "text")]
    format: msg::MessageFormat,
}

#[derive(Debug, Args)]
pub(super) struct ReactArgs {
    /// A native post ID or the wake ID from your own inbox.
    id: MessageId,
    /// One of 👍 ✅ 👀 ❌.
    #[arg(value_parser = parse_emoji)]
    emoji: ReactionEmoji,
    #[arg(long, value_enum, default_value = "text")]
    format: msg::MessageFormat,
}

fn parse_emoji(text: &str) -> Result<ReactionEmoji, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
}

fn parse_kind(text: &str) -> Result<MessageKind, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
}

async fn default_project() -> Result<ProjectId> {
    let pane = PaneId::parse(non_empty_env("TMUX_PANE").context(EXPLICIT_TARGET)?)?;
    let value = rpc::call(
        "agent.list",
        serde_json::to_value(ListParams { project_root: None })?,
        agent_target::AGENT_LIST_TIMEOUT,
    )
    .await
    .context("could not resolve caller's project; no post attempted")?;
    let rows: ListResult =
        serde_json::from_value(value).context("invalid agent list; no post attempted")?;
    let mut bindings = rows
        .into_iter()
        .filter(|row| row.state != AgentStatus::Down && row.pane.as_deref() == Some(pane.as_str()));
    let caller = bindings.next().context(EXPLICIT_TARGET)?;
    ensure!(
        bindings.next().is_none(),
        "caller pane has ambiguous agent bindings"
    );
    Ok(caller.agent_id.project)
}

pub(super) async fn cmd_post(args: PostArgs) -> Result<()> {
    if let Some(post) = args.remove {
        return mutate(
            "channel.remove",
            ChannelRemoveParams { post },
            args.format,
            "removed",
        )
        .await;
    }
    let body = args.body.context("--body is required")?;
    if let Some(post) = args.edit {
        return mutate(
            "channel.edit",
            ChannelEditParams { post, body },
            args.format,
            "edited",
        )
        .await;
    }
    let mut params = ChannelPostParams {
        channel: args.channel.map(ChannelId::Project),
        to: None,
        reply_to: args.reply,
        also_in_chat: args.also_in_chat.then_some(true),
        body,
        kind: args.kind,
        attachments: args.attachments,
        task: args.task,
        tags: args.tags,
        client_key: None,
    };
    if let Some(to) = args.to {
        let to = agent_target::address(&to, None, agent_target::AGENT_LIST_TIMEOUT).await?;
        ensure!(to != Address::System, "system is not a post recipient");
        params.to = Some(to);
    } else if params.channel.is_none() && params.reply_to.is_none() {
        params.channel = Some(ChannelId::Project(default_project().await?));
    }
    let (result, raw) = msg::request("channel.post", params, POST_TIMEOUT)
        .await
        .map_err(|error| {
            if is_timeout(&error) {
                error.context("post may already exist; check `atm history <channel>` (add `--thread <id>` for a reply) before retrying")
            } else {
                error
            }
        })?;
    stdout(|out| {
        let text = match args.format {
            msg::MessageFormat::Text => render_post(&result)?,
            msg::MessageFormat::Json => String::new(),
        };
        msg::output(&raw, args.format, &text, out)
    })
}

pub(super) async fn cmd_react(args: ReactArgs) -> Result<()> {
    mutate(
        "channel.react",
        ChannelReactParams {
            post: args.id,
            emoji: args.emoji,
        },
        args.format,
        "reaction toggled",
    )
    .await
}

fn is_timeout(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<MessageError>(),
        Some(MessageError::Timeout)
    ) || matches!(error.downcast_ref::<rpc::RpcError>(), Some(rpc::RpcError::Daemon(reply)) if reply.code == atm_protocol::error_code::TIMEOUT)
}

async fn mutate<P: serde::Serialize>(
    method: &str,
    params: P,
    format: msg::MessageFormat,
    action: &str,
) -> Result<()> {
    let (result, raw): (ChannelMutationResult, _) = msg::request(method, params, POST_TIMEOUT)
        .await.map_err(|error| if is_timeout(&error) {
            error.context("post change may have completed; check `atm history <channel>` (add `--thread <id>` for a reply) before retrying")
        } else { error })?;
    stdout(|out| msg::output(&raw, format, &format!("{} {action}", result.post_id), out))
}

fn render_post(result: &ChannelPostResult) -> Result<String> {
    let woke = if result.woke.is_empty() {
        if result.route == PostRouting::Pending {
            "none yet"
        } else {
            "none"
        }
        .to_owned()
    } else {
        result
            .woke
            .iter()
            .map(|row| row.to.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "{} posted channel={} thread={}; woke: {woke}",
        result.post_id,
        msg::escaped(&result.channel.to_string(), false),
        result
            .thread
            .map_or_else(|| "-".into(), |id| id.to_string()),
    );
    for row in &result.unavailable {
        let name = match &result.channel {
            ChannelId::Project(project) if *project == row.to.project => row.to.name.clone(),
            _ => row.to.to_string(),
        };
        write!(
            text,
            "\n{name} is {} since {} UTC: wake {} is queued and its manager is told",
            msg::status_text(row.state)?,
            row.since.format("%H:%M"),
            row.message_id,
        )?;
    }
    for row in &result.unresolved {
        write!(
            text,
            "\nunresolved: {} ({})",
            msg::escaped(&row.text, false),
            msg::status_text(row.reason)?
        )?;
    }
    Ok(text)
}
