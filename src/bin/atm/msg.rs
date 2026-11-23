//! One-shot confirmed-message commands. Bodies stay in the durable inbox.
use super::{
    output::stdout,
    rpc::{self, RpcError},
};
use anyhow::{bail, Context, Result};
use atm_comms::channel_types::{ChannelKind, ChannelListResult};
use atmd::org::messages::*;
use clap::{Args, Subcommand, ValueEnum};
use serde::{de::DeserializeOwned, Serialize};
use std::{io::Write, path::PathBuf, time::Duration};

// 10 s router + 5 s startup/transport margin = 15 s (send/inbox/ack/status).
const MESSAGE_RPC_TIMEOUT: Duration = Duration::from_secs(15);
// 60 s router + 10 s startup/transport margin = 70 s (retry/give-up).
const MESSAGE_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(70);
// 12 s history router + 5 s startup/transport margin, one page only.
const MESSAGE_HISTORY_TIMEOUT: Duration = Duration::from_secs(17);

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MessageFormat {
    Text,
    Json,
}
#[derive(Debug, Subcommand)]
pub enum MsgAction {
    /// Read one JSON page of committed history (full agent IDs, no name lookup)
    History {
        #[arg(long)]
        agent: Option<atm_org::AgentId>,
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long)]
        correspondent: Option<Address>,
        #[arg(long)]
        since: Option<chrono::DateTime<chrono::Utc>>,
        #[arg(long)]
        until: Option<chrono::DateTime<chrono::Utc>>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u16,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, value_parser = ["json"], default_value = "json")]
        format: String,
    },
    /// Queue a durable message; delivery accepts a reference, not the body
    Send {
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "note")]
        kind: String,
        #[arg(long)]
        body: String,
        #[arg(long = "attach")]
        attachments: Vec<String>,
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
    /// Explicitly mark delivered messages in your inbox read
    Ack(AckArgs),
    #[command(flatten)]
    Resolve(WakeAction),
}

#[derive(Debug, Subcommand)]
pub enum WakeAction {
    /// Retry a latched delivery with the same ID (duplicates are possible)
    Retry {
        id: MessageId,
        #[arg(long)]
        accept_duplicate_risk: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
    /// Cancel a held delivery; a late receipt may still confirm it
    GiveUp {
        id: MessageId,
        /// The owner's verbatim words; only with --relay --confirm
        #[arg(requires_all = ["relay", "confirm"])]
        words: Option<String>,
        /// Give up as the owner's designated relay; needs --confirm and WORDS
        #[arg(long, requires_all = ["confirm", "words"])]
        relay: bool,
        /// Confirm the owner asked for this give-up in WORDS
        #[arg(long, requires_all = ["relay", "words"])]
        confirm: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
}

#[derive(Clone, Copy)]
pub(super) enum WakeNames {
    Legacy,
    Final,
}

#[derive(Debug, Args)]
pub struct AckArgs {
    #[arg(required = true)]
    ids: Vec<MessageId>,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

#[derive(Debug, Args)]
pub struct InboxArgs {
    #[arg(long = "for", value_parser = super::agent_target::parse_address)]
    recipient: Option<String>,
    #[arg(long)]
    project_root: Option<PathBuf>,
    #[arg(long)]
    unread: bool,
    #[arg(long, conflicts_with = "after")]
    id: Option<MessageId>,
    #[arg(long)]
    after: Option<MessageId>,
    #[arg(long, default_value_t = 100)]
    limit: u16,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

pub(super) async fn request<P: Serialize, R: DeserializeOwned>(
    method: &str,
    params: P,
    budget: Duration,
) -> Result<(R, serde_json::Value)> {
    let value = rpc::call(method, serde_json::to_value(params)?, budget)
        .await
        .map_err(|error| match error {
            RpcError::Timeout => anyhow::Error::new(MessageError::Timeout),
            other => anyhow::Error::new(other),
        })?;
    let result =
        serde_json::from_value(value.clone()).context("invalid message result from daemon")?;
    Ok((result, value))
}

async fn request_wake<P: Serialize, R: DeserializeOwned>(
    names: WakeNames,
    final_name: &str,
    legacy: &str,
    params: P,
    budget: Duration,
) -> Result<(R, serde_json::Value)> {
    if matches!(names, WakeNames::Legacy) {
        return request(legacy, params, budget).await;
    }
    tokio::time::timeout(budget, async {
        match request(final_name, &params, budget).await {
            Err(error) if matches!(error.downcast_ref::<RpcError>(), Some(RpcError::TooOld)) => {
                // Connected.methods omitted the final name; no request was sent.
                request(legacy, params, budget).await
            }
            result => result,
        }
    })
    .await
    .map_err(|_| MessageError::Timeout)?
}

pub(super) async fn inbox_request(params: InboxParams) -> Result<InboxResult> {
    request("msg.inbox", params, MESSAGE_RPC_TIMEOUT)
        .await
        .map(|(result, _)| result)
}

pub(super) fn escaped(text: &str, multiline: bool) -> String {
    text.chars()
        .flat_map(|c| {
            if c == '\\' && !multiline {
                "\\\\".chars().collect::<Vec<_>>()
            } else if c.is_control() && !(multiline && matches!(c, '\n' | '\t')) {
                c.escape_debug().collect()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub(super) fn output<R: Serialize>(
    result: &R,
    format: MessageFormat,
    text: &str,
    out: &mut impl Write,
) -> Result<()> {
    match format {
        MessageFormat::Json => {
            // serde_json escapes C0; also neutralize DEL/C1 for terminal readers.
            for c in serde_json::to_string(result)?.chars() {
                if ('\u{7f}'..='\u{9f}').contains(&c) {
                    write!(out, "\\u{:04x}", u32::from(c))?;
                } else {
                    write!(out, "{c}")?;
                }
            }
            writeln!(out)?;
        }
        MessageFormat::Text => writeln!(out, "{text}")?,
    }
    Ok(())
}
pub(super) fn status_text(status: impl Serialize) -> Result<String> {
    Ok(serde_json::to_value(status)?
        .as_str()
        .unwrap_or("unknown")
        .to_owned())
}

pub async fn cmd_msg(action: MsgAction) -> Result<()> {
    cmd_msg_with_lookup_budget(action, super::agent_target::AGENT_LIST_TIMEOUT).await
}

pub(super) async fn cmd_msg_with_lookup_budget(
    action: MsgAction,
    lookup_budget: Duration,
) -> Result<()> {
    match action {
        MsgAction::History {
            agent,
            task,
            correspondent,
            since,
            until,
            text,
            limit,
            cursor,
            format: _,
        } => {
            let params = atm_org::message_query::HistoryParams {
                agent,
                task,
                correspondent,
                since,
                until,
                text,
                limit,
                cursor,
            };
            params.validate()?;
            let (_, raw): (serde_json::Value, _) =
                request("msg.history", params, MESSAGE_HISTORY_TIMEOUT).await?;
            stdout(|out| output(&raw, MessageFormat::Json, "", out))
        }
        MsgAction::Send {
            to,
            kind,
            body,
            attachments,
            task,
            project_root,
            format,
        } => {
            let kind = serde_json::from_value(serde_json::Value::String(kind))
                .context("invalid message kind")?;
            let to = super::agent_target::address(&to, project_root, lookup_budget).await?;
            if to == Address::System {
                bail!("system is not a message recipient");
            }
            let owner = to == Address::Owner;
            let (result, raw): (SendResult, _) = request(
                "msg.send",
                SendParams {
                    to,
                    kind,
                    body,
                    attachments,
                    task,
                },
                MESSAGE_RPC_TIMEOUT,
            )
            .await?;
            let text = format!(
                "{} {}",
                result.message_id,
                if owner {
                    "delivered to owner"
                } else {
                    "queued"
                }
            );
            stdout(|out| output(&raw, format, &text, out))
        }
        MsgAction::Ack(args) => cmd_ack(args, WakeNames::Legacy).await,
        MsgAction::Resolve(action) => cmd_wake(action, WakeNames::Legacy).await,
    }
}

pub(super) async fn cmd_wake(action: WakeAction, names: WakeNames) -> Result<()> {
    match action {
        WakeAction::Retry {
            id,
            accept_duplicate_risk,
            format,
        } => {
            if !accept_duplicate_risk {
                bail!("Inspect the pane first; retry can duplicate delivery. Repeat with --accept-duplicate-risk.");
            }
            let (result, raw): (RetryResult, _) = request_wake(
                names,
                "wake.retry",
                "msg.retry",
                ResolveParams { id },
                MESSAGE_RESOLUTION_TIMEOUT,
            )
            .await?;
            let text = format!(
                "{} {} (duplicate delivery possible)",
                result.message_id,
                status_text(result.status)?
            );
            stdout(|out| output(&raw, format, &text, out))
        }
        WakeAction::GiveUp {
            id, words, format, ..
        } => {
            let (result, raw): (GiveUpResult, _) = request_wake(
                names,
                "wake.give_up",
                "msg.give_up",
                GiveUpParams {
                    id,
                    relay_words: words,
                },
                MESSAGE_RESOLUTION_TIMEOUT,
            )
            .await?;
            let mut text = format!("{} {}", result.message_id, status_text(result.status)?);
            if let Some(warning) = &result.editor_warning {
                text.push_str(&format!("\nwarning: {}", escaped(warning, false)));
            }
            stdout(|out| output(&raw, format, &text, out))
        }
    }
}

pub(super) async fn cmd_ack(AckArgs { ids, format }: AckArgs, names: WakeNames) -> Result<()> {
    let (result, raw): (AckResult, _) = request_wake(
        names,
        "wake.ack",
        "msg.ack",
        AckParams { ids },
        MESSAGE_RPC_TIMEOUT,
    )
    .await?;
    stdout(|out| output(&raw, format, &format!("{} marked read", result.count), out))
}

pub(super) fn render_content(
    task: Option<&TaskId>,
    attachments: &[String],
    body: &str,
    out: &mut impl Write,
) -> Result<()> {
    if let Some(task) = task {
        writeln!(out, "task: {}", escaped(&task.to_string(), false))?;
    }
    for path in attachments {
        writeln!(out, "attach: {}", escaped(path, false))?;
    }
    for line in escaped(body, true).split('\n') {
        writeln!(out, "  {line}")?;
    }
    Ok(())
}

fn wants_reply_hint(entry: &InboxEntry) -> bool {
    entry.content.post.is_some()
        && entry.content.from != Address::System
        && entry
            .post_context
            .as_ref()
            .is_some_and(|context| context.channel.is_known())
}

fn render_post_context(
    entry: &InboxEntry,
    channels: Option<&ChannelListResult>,
    out: &mut impl Write,
) -> Result<()> {
    let Some((post, context)) = entry.content.post.zip(entry.post_context.as_ref()) else {
        return Ok(());
    };
    writeln!(
        out,
        "post: {post} channel={} thread={}",
        escaped(&context.channel.to_string(), false),
        context
            .thread
            .map_or_else(|| "-".into(), |id| id.to_string()),
    )?;
    if let Some(channels) = channels.filter(|_| wants_reply_hint(entry)) {
        if channels.channels.iter().any(|row| {
            row.id == context.channel && row.writable && row.kind == ChannelKind::Project
        }) {
            writeln!(out, "reply: atm post --reply {post} --body '...'")?;
        } else {
            writeln!(
                out,
                "reply: atm post --to {} --body '...'",
                entry.content.from
            )?;
        }
    }
    Ok(())
}

fn render_inbox(
    result: &InboxResult,
    recipient: Option<&Address>,
    unread: bool,
    channels: Option<&ChannelListResult>,
    out: &mut impl Write,
) -> Result<()> {
    for entry in &result.messages {
        let c = &entry.content;
        writeln!(
            out,
            "{} from={} to={} kind={} status={} sent={} read={}",
            entry.message_id,
            c.from,
            c.to,
            serde_json::to_value(c.kind)?.as_str().unwrap_or("unknown"),
            status_text(entry.status)?,
            entry.sent_at.to_rfc3339(),
            entry
                .read_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "-".into())
        )?;
        let delivery = &entry.delivery;
        write!(out, "delivery: {}", status_text(delivery.phase)?)?;
        if let Some(pane) = &delivery.pane {
            write!(out, " pane={}", escaped(pane, false))?;
        }
        if delivery.attempt > 0 {
            write!(out, " attempt={}", delivery.attempt)?;
        }
        if let Some(blocked_by) = &delivery.blocked_by {
            write!(
                out,
                " blocked_by={}",
                escaped(&blocked_by.to_string(), false)
            )?;
        }
        if let Some(retry_at) = &delivery.retry_at {
            write!(out, " retry_at={}", escaped(&retry_at.to_rfc3339(), false))?;
        }
        if let Some(reason) = &delivery.reason {
            write!(out, " reason={}", escaped(reason, false))?;
        }
        writeln!(out)?;
        render_post_context(entry, channels, out)?;
        render_content(c.task.as_ref(), &c.attachments, &c.body, out)?;
    }
    writeln!(out, "{} unread", result.unread_count)?;
    if let Some(id) = result.next_after {
        write!(out, "next:")?;
        if let Some(recipient) = recipient {
            write!(out, " --for {recipient}")?;
        }
        if unread {
            write!(out, " --unread")?;
        }
        writeln!(out, " --after {id}")?;
    }
    Ok(())
}
pub async fn cmd_inbox(args: InboxArgs) -> Result<()> {
    cmd_inbox_with_lookup_budget(args, super::agent_target::AGENT_LIST_TIMEOUT).await
}

pub(super) async fn cmd_inbox_with_lookup_budget(
    args: InboxArgs,
    lookup_budget: Duration,
) -> Result<()> {
    let recipient = match args.recipient {
        Some(to) => {
            Some(super::agent_target::address(&to, args.project_root, lookup_budget).await?)
        }
        None => None,
    };
    let (result, raw): (InboxResult, _) = request(
        "msg.inbox",
        InboxParams {
            recipient: recipient.clone(),
            unread_only: args.unread,
            count_only: false,
            id: args.id,
            after: args.after,
            limit: args.limit,
        },
        MESSAGE_RPC_TIMEOUT,
    )
    .await?;
    match args.format {
        MessageFormat::Json => stdout(|out| output(&raw, args.format, "", out)),
        MessageFormat::Text => {
            // Ask as the caller, not the --for recipient. A failed lookup must
            // not hide the inbox or turn into a guess at how to reply.
            let channels = if result.messages.iter().any(wants_reply_hint) {
                super::channels::list(false)
                    .await
                    .ok()
                    .map(|(list, _)| list)
            } else {
                None
            };
            stdout(|out| {
                render_inbox(
                    &result,
                    recipient.as_ref(),
                    args.unread,
                    channels.as_ref(),
                    out,
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io;

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn output_write_failure_is_error() {
        let result = AckResult { count: 1 };
        for format in [MessageFormat::Text, MessageFormat::Json] {
            assert!(output(&result, format, "read", &mut Broken).is_err());
        }
        assert!(render_inbox(
            &InboxResult {
                messages: vec![],
                unread_count: 0,
                next_after: None
            },
            None,
            false,
            None,
            &mut Broken
        )
        .is_err());
    }
    // Break: history chooses the ordinary 15 s or resolution 70 s budget.
    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)]
    async fn history_has_one_seventeen_second_total_budget() {
        use std::{future::Future, task::Poll};
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Only in-memory permit/clock work: no startup worker or socket can run.
        let _held = super::super::rpc::startup_slot_for_test()
            .acquire_owned()
            .await
            .unwrap();
        let super::super::Command::Msg { action } =
            super::super::Args::try_parse_from(["atm", "msg", "history"])
                .unwrap()
                .command
                .unwrap()
        else {
            panic!("wrong command")
        };
        let mut call = Box::pin(cmd_msg(action));
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(16)).await;
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let Poll::Ready(result) =
            std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await
        else {
            panic!("missing history deadline")
        };
        assert!(result.unwrap_err().to_string().contains("timed out"));
        let history = serde_json::json!({"messages":[],"next_cursor":null});
        assert!(output(&history, MessageFormat::Json, "", &mut Broken).is_err());
    }

    #[test]
    fn message_commands_parse_exact_shape() {
        for args in [
            vec!["atm", "msg", "send", "--to", "owner", "--body", "hi"],
            vec!["atm", "msg", "ack", "m1"],
            vec!["atm", "msg", "retry", "m1"],
            vec!["atm", "msg", "give-up", "m1"],
            vec!["atm", "inbox"],
        ] {
            assert!(super::super::Args::try_parse_from(&args).is_ok());
            let mut help = args;
            help.push("--help");
            assert_eq!(
                super::super::Args::try_parse_from(help).unwrap_err().kind(),
                clap::error::ErrorKind::DisplayHelp
            );
        }
    }
    // s3-giverelay. Break: a partial relay form reaches the daemon (as the
    // owner's give-up or without his words), or the full form stops parsing.
    #[test]
    fn give_up_relay_needs_relay_confirm_and_words_together() {
        for group in ["wake", "msg"] {
            let parse = |tail: &[&str]| {
                let mut args = vec!["atm", group, "give-up"];
                args.extend(tail);
                super::super::Args::try_parse_from(args)
            };
            assert!(parse(&["--relay", "--confirm", "m1", "Cancel them"]).is_ok());
            assert!(parse(&["m1", "--relay", "--confirm", "Cancel them"]).is_ok());
            for partial in [
                &["--relay", "m1", "Cancel them"][..],
                &["--confirm", "m1", "Cancel them"],
                &["--relay", "--confirm", "m1"],
                &["--relay", "m1"],
                &["--confirm", "m1"],
                &["m1", "Cancel them"],
            ] {
                let error = parse(partial).unwrap_err();
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "{group} {partial:?}"
                );
            }
        }
    }
    #[test]
    fn cli_budgets_exceed_method_router_budgets() {
        assert!(MESSAGE_RPC_TIMEOUT > Duration::from_secs(10));
        assert!(MESSAGE_RESOLUTION_TIMEOUT > Duration::from_secs(60));
        assert!(super::super::agent_target::AGENT_LIST_TIMEOUT > Duration::from_secs(10));
    }
}
