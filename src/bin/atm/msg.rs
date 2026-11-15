//! One-shot confirmed-message commands. Bodies stay in the durable inbox.
use anyhow::{bail, Context, Result};
use atmd::org::messages::*;
use clap::{Args, Subcommand, ValueEnum};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

// 10 s router + 5 s startup/transport margin = 15 s (send/inbox/ack/status).
const MESSAGE_RPC_TIMEOUT: Duration = Duration::from_secs(15);
// 60 s router + 10 s startup/transport margin = 70 s (retry/give-up).
const MESSAGE_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(70);

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MessageFormat {
    Text,
    Json,
}
#[derive(Debug, Subcommand)]
pub enum MsgAction {
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
    Ack {
        #[arg(required = true)]
        ids: Vec<MessageId>,
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
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
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
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
    let value = super::rpc::call(method, serde_json::to_value(params)?, budget)
        .await
        .map_err(|error| match error {
            super::rpc::RpcError::Timeout => anyhow::Error::new(MessageError::Timeout),
            other => anyhow::Error::new(other),
        })?;
    let result =
        serde_json::from_value(value.clone()).context("invalid message result from daemon")?;
    Ok((result, value))
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
fn output<R: Serialize>(
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
fn status_text(status: impl Serialize) -> Result<String> {
    Ok(serde_json::to_value(status)?
        .as_str()
        .unwrap_or("unknown")
        .to_owned())
}

pub async fn cmd_msg(action: MsgAction) -> Result<()> {
    match action {
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
            let to = super::agent_target::address(&to, project_root).await?;
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
            output(&raw, format, &text, &mut io::stdout().lock())
        }
        MsgAction::Ack { ids, format } => {
            let (result, raw): (AckResult, _) =
                request("msg.ack", AckParams { ids }, MESSAGE_RPC_TIMEOUT).await?;
            output(
                &raw,
                format,
                &format!("{} marked read", result.count),
                &mut io::stdout().lock(),
            )
        }
        MsgAction::Retry {
            id,
            accept_duplicate_risk,
            format,
        } => {
            if !accept_duplicate_risk {
                bail!("Inspect the pane first; retry can duplicate delivery. Repeat with --accept-duplicate-risk.");
            }
            let (result, raw): (RetryResult, _) = request(
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
            output(&raw, format, &text, &mut io::stdout().lock())
        }
        MsgAction::GiveUp { id, format } => {
            let (result, raw): (GiveUpResult, _) = request(
                "msg.give_up",
                ResolveParams { id },
                MESSAGE_RESOLUTION_TIMEOUT,
            )
            .await?;
            let text = format!("{} {}", result.message_id, status_text(result.status)?);
            output(&raw, format, &text, &mut io::stdout().lock())
        }
    }
}

fn render_inbox(
    result: &InboxResult,
    recipient: Option<&Address>,
    unread: bool,
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
        if let Some(reason) = &delivery.reason {
            write!(out, " reason={}", escaped(reason, false))?;
        }
        writeln!(out)?;
        if let Some(task) = &c.task {
            writeln!(out, "task: {}", escaped(&task.to_string(), false))?;
        }
        for path in &c.attachments {
            writeln!(out, "attach: {}", escaped(path, false))?;
        }
        for line in escaped(&c.body, true).split('\n') {
            writeln!(out, "  {line}")?;
        }
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
    let recipient = match args.recipient {
        Some(to) => Some(super::agent_target::address(&to, args.project_root).await?),
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
    let mut out = io::stdout().lock();
    match args.format {
        MessageFormat::Json => output(&raw, args.format, "", &mut out),
        MessageFormat::Text => render_inbox(&result, recipient.as_ref(), args.unread, &mut out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
            &mut Broken
        )
        .is_err());
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
    #[test]
    fn cli_budgets_exceed_method_router_budgets() {
        assert!(MESSAGE_RPC_TIMEOUT > Duration::from_secs(10));
        assert!(MESSAGE_RESOLUTION_TIMEOUT > Duration::from_secs(60));
        assert!(super::super::agent_target::AGENT_LIST_TIMEOUT > Duration::from_secs(10));
    }
}
