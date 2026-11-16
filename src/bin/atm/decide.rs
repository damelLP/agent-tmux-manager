//! One-shot decision commands; recording an answer is not receipt confirmation.
use super::msg::{escaped, output, MessageFormat};
use anyhow::{bail, Context, Result};
use atmd::org::decisions::{
    DecisionId, DecisionKind, DecisionStatus, ListParams, ListResult, MutationResult,
    RequestParams, RequestResult, ResolveParams, WithdrawParams,
};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    io::{self, Write},
    time::Duration,
};

const RPC_BUDGET: Duration = Duration::from_secs(27);
#[derive(Debug, Args)]
pub struct DecideArgs {
    id: DecisionId,
    option: String,
    note: Option<String>,
    #[arg(long = "rule")]
    standing_rule: bool,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DecisionsArgs {
    #[command(subcommand)]
    action: Option<DecisionsAction>,
    #[arg(long, conflicts_with = "statuses")]
    all: bool,
    #[arg(long = "status", value_parser = parse_status)]
    statuses: Vec<DecisionStatus>,
    #[arg(long)]
    after: Option<DecisionId>,
    #[arg(long, default_value_t = 100)]
    limit: u16,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}
#[derive(Debug, Subcommand)]
enum DecisionsAction {
    /// Request the owner's decision from a named agent pane; prints an ID, not approval
    Request {
        #[arg(long, value_parser = parse_request_kind)]
        kind: DecisionKind,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        context: String,
        /// Literal path, not read or uploaded; repeat for each attachment
        #[arg(long = "attach")]
        files: Vec<String>,
        /// Exact option string; repeat for 2–32 distinct options
        #[arg(long = "option", required = true)]
        options: Vec<String>,
        #[arg(long)]
        recommended: Option<String>,
        #[arg(long, action = clap::ArgAction::Set, required = true)]
        may_be_undone: bool,
        #[arg(long, action = clap::ArgAction::Set, required = true)]
        changes_intent: bool,
        /// Future RFC3339 timestamp; the daemon checks time, with no expiry timer
        #[arg(long, value_parser = parse_deadline)]
        deadline: Option<DateTime<Utc>>,
        /// Place after request; listing flags cannot be combined with a mutation
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
    /// Withdraw a pending decision as its requester or owner; provisional choices cannot be withdrawn
    Withdraw {
        id: DecisionId,
        #[arg(long)]
        reason: Option<String>,
        /// Place after withdraw; listing flags cannot be combined with a mutation
        #[arg(long, value_enum, default_value = "text")]
        format: MessageFormat,
    },
}
fn parse_request_kind(text: &str) -> std::result::Result<DecisionKind, String> {
    match serde_json::from_value(serde_json::Value::String(text.into())) {
        Ok(DecisionKind::Unknown) | Err(_) => {
            Err("expected action, hire, budget, policy, plan or dialog".into())
        }
        Ok(kind) => Ok(kind),
    }
}
fn parse_deadline(text: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| error.to_string())
}
fn parse_status(text: &str) -> std::result::Result<DecisionStatus, String> {
    serde_json::from_value(serde_json::Value::String(text.into())).map_err(|_| {
        "expected pending, taken_provisionally, resolved, overruled, expired or withdrawn".into()
    })
}
async fn request<P: Serialize, R: DeserializeOwned>(
    method: &str,
    params: P,
) -> Result<(R, serde_json::Value)> {
    let raw = super::rpc::call(method, serde_json::to_value(params)?, RPC_BUDGET)
        .await
        .map_err(|error| match error {
            super::rpc::RpcError::Timeout => {
                anyhow::Error::new(atmd::org::decisions::DecisionError::Timeout)
            }
            other => anyhow::Error::new(other),
        })?;
    let result =
        serde_json::from_value(raw.clone()).context("invalid decision result from daemon")?;
    Ok((result, raw))
}
fn word(value: impl Serialize) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .unwrap_or("unknown")
        .into())
}

pub async fn cmd_decide(args: DecideArgs) -> Result<()> {
    let (result, raw): (MutationResult, _) = request(
        "decision.resolve",
        ResolveParams {
            id: args.id,
            option: args.option,
            note: args.note,
            standing_rule: args.standing_rule,
        },
    )
    .await?;
    let decision = &result.decision.decision;
    if !matches!(
        decision.status,
        DecisionStatus::Resolved | DecisionStatus::Overruled
    ) {
        bail!("invalid decision result from daemon: answer is not final");
    }
    let answer = decision
        .resolution
        .as_ref()
        .context("invalid decision result from daemon: missing answer")?;
    let text = format!(
        "{} {}: {}{}",
        decision.id,
        word(decision.status)?,
        escaped(&answer.option, false),
        if result.changed { "" } else { " (unchanged)" }
    );
    output(&raw, args.format, &text, &mut io::stdout().lock())
}
fn render_list(result: &ListResult, out: &mut impl Write) -> Result<()> {
    if result.decisions.is_empty() {
        writeln!(out, "No matching decisions.")?;
    }
    for entry in &result.decisions {
        let d = &entry.decision;
        let r = &d.request;
        writeln!(
            out,
            "{} {} from={} kind={} class={}",
            d.id,
            word(d.status)?,
            r.requester,
            word(r.kind)?,
            word(entry.class)?
        )?;
        writeln!(out, "  title: {}", escaped(&r.title, false))?;
        if !r.context.is_empty() {
            for line in r.context.split('\n') {
                writeln!(out, "  context: {}", escaped(line, false))?;
            }
        }
        for path in &r.files {
            writeln!(out, "  attach: {}", escaped(path, false))?;
        }
        for option in &r.options {
            writeln!(
                out,
                "  option: {}{}",
                escaped(option, false),
                if r.recommended.as_ref() == Some(option) {
                    " (recommended)"
                } else {
                    ""
                }
            )?;
        }
        if let Some(deadline) = r.deadline {
            writeln!(out, "  deadline: {}", deadline.to_rfc3339())?;
        }
        if d.status == DecisionStatus::TakenProvisionally {
            if let Some(taken) = &d.provisional {
                writeln!(
                    out,
                    "  provisional: {} by={}",
                    escaped(&taken.option, false),
                    taken.by
                )?;
            }
        }
        if matches!(
            d.status,
            DecisionStatus::Resolved | DecisionStatus::Overruled
        ) {
            if let Some(answer) = &d.resolution {
                writeln!(
                    out,
                    "  answer: {} by={} rule={}",
                    escaped(&answer.option, false),
                    answer.by,
                    answer.standing_rule
                )?;
                if let Some(note) = &answer.note {
                    writeln!(out, "  note: {}", escaped(note, false))?;
                }
            }
        }
        if d.status == DecisionStatus::Withdrawn {
            if let Some(reason) = d.withdrawal.as_ref().and_then(|w| w.reason.as_deref()) {
                writeln!(out, "  reason: {}", escaped(reason, false))?;
            }
        }
    }
    writeln!(
        out,
        "{} pending, {} provisional",
        result.pending_count, result.provisional_count
    )?;
    if let Some(after) = result.next_after {
        writeln!(out, "next: --after {after}")?;
    }
    Ok(())
}
async fn cmd_decision_action(action: DecisionsAction) -> Result<()> {
    let (raw, format, text) = match action {
        DecisionsAction::Request {
            kind,
            title,
            context,
            files,
            options,
            recommended,
            may_be_undone,
            changes_intent,
            deadline,
            format,
        } => {
            let (result, raw): (RequestResult, _) = request(
                "decision.request",
                RequestParams {
                    kind,
                    title,
                    context,
                    files,
                    options,
                    recommended,
                    may_be_undone,
                    changes_intent,
                    deadline,
                },
            )
            .await?;
            (raw, format, format!("{} requested", result.decision_id))
        }
        DecisionsAction::Withdraw { id, reason, format } => {
            let (result, raw): (MutationResult, _) =
                request("decision.withdraw", WithdrawParams { id, reason }).await?;
            let decision = &result.decision.decision;
            if decision.status != DecisionStatus::Withdrawn {
                bail!("invalid decision result from daemon: decision is not withdrawn");
            }
            (
                raw,
                format,
                format!(
                    "{} withdrawn{}",
                    decision.id,
                    if result.changed { "" } else { " (unchanged)" }
                ),
            )
        }
    };
    output(&raw, format, &text, &mut io::stdout().lock())
}
pub async fn cmd_decisions(args: DecisionsArgs) -> Result<()> {
    if let Some(action) = args.action {
        return cmd_decision_action(action).await;
    }
    let statuses = if args.all {
        Some(vec![
            DecisionStatus::Pending,
            DecisionStatus::TakenProvisionally,
            DecisionStatus::Resolved,
            DecisionStatus::Overruled,
            DecisionStatus::Expired,
            DecisionStatus::Withdrawn,
        ])
    } else if args.statuses.is_empty() {
        None
    } else {
        Some(args.statuses)
    };
    let (result, raw): (ListResult, _) = request(
        "decision.list",
        ListParams {
            statuses,
            after: args.after,
            limit: args.limit,
            count_only: false,
        },
    )
    .await?;
    let mut out = io::stdout().lock();
    match args.format {
        MessageFormat::Text => render_list(&result, &mut out),
        MessageFormat::Json => output(&raw, args.format, "", &mut out),
    }
}

/// The global pending count, and whether the owner is marked away.
pub(super) async fn pending_count() -> Result<(usize, bool)> {
    let (result, _): (ListResult, _) = request(
        "decision.list",
        ListParams {
            statuses: None,
            after: None,
            limit: 100,
            count_only: true,
        },
    )
    .await?;
    if !result.decisions.is_empty() || result.next_after.is_some() {
        bail!("invalid decision aggregate result from daemon");
    }
    Ok((result.pending_count, result.away))
}

#[cfg(test)]
mod tests {
    use super::super::Args;
    use clap::Parser;

    fn request_args<'a>(kind: &'a str, undone: &'a str, intent: &'a str) -> Vec<&'a str> {
        vec![
            "atm",
            "decisions",
            "request",
            "--kind",
            kind,
            "--title",
            "Approve this plan",
            "--option",
            "yes",
            "--option",
            "no",
            "--may-be-undone",
            undone,
            "--changes-intent",
            intent,
        ]
    }

    // Break caught: commands disappear from discovery or valid policy facts cannot be expressed.
    #[test]
    fn decisions_request_and_withdraw_have_help_and_parse() {
        for kind in ["action", "hire", "budget", "policy", "plan", "dialog"] {
            for undone in ["true", "false"] {
                for intent in ["true", "false"] {
                    let args = request_args(kind, undone, intent);
                    assert!(Args::try_parse_from(&args).is_ok(), "{args:?}");
                }
            }
        }
        for args in [
            vec!["atm", "decisions"],
            vec!["atm", "decisions", "--all", "--format", "json"],
            vec![
                "atm",
                "decisions",
                "--status",
                "resolved",
                "--after",
                "d4",
                "--limit",
                "2",
            ],
            vec!["atm", "decisions", "withdraw", "d41"],
            vec![
                "atm",
                "decisions",
                "withdraw",
                "d41",
                "--reason",
                "superseded",
                "--format",
                "json",
            ],
        ] {
            assert!(Args::try_parse_from(&args).is_ok(), "{args:?}");
        }
        for action in ["request", "withdraw"] {
            assert_eq!(
                Args::try_parse_from(["atm", "decisions", action, "--help"])
                    .unwrap_err()
                    .kind(),
                clap::error::ErrorKind::DisplayHelp,
            );
        }
    }

    // Break caught: a missing fact defaults into delegation, or a listing flag is silently ignored.
    #[test]
    fn decision_mutations_require_explicit_facts_and_reject_mixed_list_flags() {
        let valid = request_args("plan", "true", "true");
        let mut bad = Vec::new();
        for flag in [
            "--kind",
            "--title",
            "--option",
            "--may-be-undone",
            "--changes-intent",
        ] {
            let mut args = valid.clone();
            while let Some(index) = args.iter().position(|value| *value == flag) {
                args.drain(index..index + 2);
            }
            bad.push(args);
        }
        for (flag, value) in [
            ("--kind", "unknown"),
            ("--kind", "future"),
            ("--kind", "Plan"),
            ("--may-be-undone", "yes"),
            ("--changes-intent", "0"),
        ] {
            let mut args = valid.clone();
            let index = args.iter().position(|value| *value == flag).unwrap();
            args[index + 1] = value;
            bad.push(args);
        }
        for extra in [
            vec!["--deadline", "tomorrow"],
            vec!["--deadline", "2099-01-01"],
            vec!["--format", "xml"],
            vec!["--requester", "owner"],
            vec!["--by", "owner"],
            vec!["--from", "owner"],
            vec!["--may-be-undone"],
            vec!["--changes-intent"],
        ] {
            let mut args = valid.clone();
            args.extend(extra);
            bad.push(args);
        }
        for id in ["d01", "m1", "d-1", "d18446744073709551616"] {
            bad.push(vec!["atm", "decisions", "withdraw", id]);
        }
        bad.push(vec!["atm", "decisions", "withdraw"]);
        for mutation in [valid, vec!["atm", "decisions", "withdraw", "d41"]] {
            for flags in [
                vec!["--all"],
                vec!["--status", "pending"],
                vec!["--after", "d4"],
                vec!["--limit", "2"],
            ] {
                let mut before = vec!["atm", "decisions"];
                before.extend(&flags);
                before.extend(&mutation[2..]);
                bad.push(before);
                let mut after = mutation.clone();
                after.extend(flags);
                bad.push(after);
            }
            let mut before = vec!["atm", "decisions", "--format", "json"];
            before.extend(&mutation[2..]);
            bad.push(before);
        }
        for args in bad {
            let error = Args::try_parse_from(&args).expect_err(&format!("accepted {args:?}"));
            assert_eq!(error.exit_code(), 2, "{args:?}: {error}");
        }
    }
}
