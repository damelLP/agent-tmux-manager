//! One-page, read-only views of the daemon's channels and posts.

use super::msg::{escaped, output, render_content, request, status_text, MessageFormat};
use super::output::stdout;
use anyhow::Result;
use atm_comms::channel_types::{
    ChannelHistoryParams, ChannelHistoryResult, ChannelListParams, ChannelListResult,
};
use atm_org::{ChannelId, MessageId};
use clap::Args;
use serde_json::Value;
use std::{io::Write, time::Duration};

// Both channel reads have an 8 s server budget; leave 5 s for startup/transport.
const CHANNEL_READ_TIMEOUT: Duration = Duration::from_secs(13);

#[derive(Debug, Args)]
pub(super) struct ChannelsArgs {
    /// Include channels you cannot post in.
    #[arg(long)]
    all: bool,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

#[derive(Debug, Args)]
pub(super) struct HistoryArgs {
    /// A project ID or a direct channel ID from `atm channels`.
    #[arg(value_parser = parse_channel)]
    channel: ChannelId,
    /// Read this thread oldest first; takes a post, reply or wake ID.
    #[arg(long)]
    thread: Option<MessageId>,
    /// Include only posts before this ID (exclusive).
    #[arg(long)]
    before: Option<MessageId>,
    /// Include only posts after this ID (exclusive).
    #[arg(long)]
    after: Option<MessageId>,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200))]
    limit: u16,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

fn parse_channel(text: &str) -> Result<ChannelId, String> {
    let id = ChannelId::from(text.to_owned());
    id.is_known()
        .then_some(id)
        .ok_or_else(|| format!("invalid channel id {text:?}; use an ID from atm channels"))
}

pub(super) async fn list(all: bool) -> Result<(ChannelListResult, Value)> {
    request(
        "channel.list",
        ChannelListParams { all },
        CHANNEL_READ_TIMEOUT,
    )
    .await
}

pub(super) async fn cmd_channels(ChannelsArgs { all, format }: ChannelsArgs) -> Result<()> {
    let (result, raw) = list(all).await?;
    stdout(|out| match format {
        MessageFormat::Json => output(&raw, format, "", out),
        MessageFormat::Text => render_channels(&result, out),
    })
}

pub(super) async fn cmd_history(args: HistoryArgs) -> Result<()> {
    let params = ChannelHistoryParams {
        channel: args.channel,
        thread: args.thread,
        include_shared_replies: false,
        before: args.before,
        after: args.after,
        limit: args.limit,
    };
    let (result, raw) = request("channel.history", params, CHANNEL_READ_TIMEOUT).await?;
    stdout(|out| match args.format {
        MessageFormat::Json => output(&raw, args.format, "", out),
        MessageFormat::Text => render_history(&result, out),
    })
}

fn render_channels(result: &ChannelListResult, out: &mut impl Write) -> Result<()> {
    if result.channels.is_empty() {
        writeln!(out, "no channels")?;
    }
    for row in &result.channels {
        writeln!(
            out,
            "{} section={} writable={} unread={} last={}",
            escaped(&row.id.to_string(), false),
            status_text(row.section)?,
            row.writable,
            row.unread,
            row.last_post
                .map_or_else(|| "-".into(), |id| id.to_string()),
        )?;
    }
    Ok(())
}

fn render_history(result: &ChannelHistoryResult, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "channel: {}",
        escaped(&result.channel.to_string(), false)
    )?;
    if result.posts.is_empty() {
        writeln!(out, "no posts")?;
    }
    for row in &result.posts {
        writeln!(
            out,
            "{} from={} kind={} at={} thread={} replies={}{}",
            row.id,
            row.author,
            status_text(row.kind)?,
            row.posted_at.to_rfc3339(),
            row.thread.map_or_else(|| "-".into(), |id| id.to_string()),
            row.replies,
            if row.edited { " edited" } else { "" },
        )?;
        render_content(row.task.as_ref(), &row.attachments, &row.body, out)?;
        if !row.reactions.is_empty() {
            write!(out, "reactions:")?;
            for chip in &row.reactions {
                write!(
                    out,
                    " {}={}{}",
                    chip.emoji.as_str(),
                    chip.count,
                    if chip.mine { " (you)" } else { "" }
                )?;
            }
            writeln!(out)?;
        }
    }
    if result.more {
        writeln!(out, "more: true")?;
    }
    Ok(())
}
