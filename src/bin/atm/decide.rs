//! One-shot decision commands; recording an answer is not receipt confirmation.
use super::msg::{escaped, output, MessageFormat};
use anyhow::{bail, Context, Result};
use atmd::org::decisions::{
    DecisionId, DecisionStatus, ListParams, ListResult, MutationResult, ResolveParams,
};
use clap::Args;
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
pub struct DecisionsArgs {
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
pub async fn cmd_decisions(args: DecisionsArgs) -> Result<()> {
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

pub(super) async fn pending_count() -> Result<usize> {
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
    Ok(result.pending_count)
}
