//! One-shot decision commands; recording an answer is not receipt confirmation.
pub(super) use super::msg::status_text as word;
use super::msg::{escaped, output, MessageFormat};
use super::output::stdout;
use anyhow::{bail, Context, Result};
use atmd::org::decisions::{
    DecisionId, DecisionKind, DecisionStatus, GetParams, GetResult, ListParams, ListResult,
    MutationResult, RequestParams, RequestResult, ResolveParams, WithdrawParams,
};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{de::DeserializeOwned, Serialize};
use std::{io::Write, time::Duration};

const RPC_BUDGET: Duration = Duration::from_secs(27);
#[derive(Debug, Args)]
pub struct DecideArgs {
    id: DecisionId,
    option: String,
    note: Option<String>,
    #[arg(long = "rule")]
    standing_rule: bool,
    #[arg(long, conflicts_with = "standing_rule")]
    relay: bool,
    /// Confirm the same provisional option as the designated relay; NOTE is the owner's verbatim words
    #[arg(long, requires_all = ["relay", "note"], conflicts_with = "standing_rule")]
    confirm: bool,
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
        /// Future RFC3339 timestamp; a decision still pending then expires
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

fn relay_text(id: DecisionId, option: &str, result: &MutationResult) -> Result<String> {
    let decision = &result.decision.decision;
    let taken = decision
        .provisional
        .as_ref()
        .context("invalid relay result: missing provisional choice")?;
    if decision.id != id
        || taken.decision_id != id
        || taken.option != option
        || decision.status != DecisionStatus::TakenProvisionally
    {
        bail!("invalid relay result: expected the requested provisional answer");
    }
    Ok(format!(
        "{id} taken_provisionally: {}{}\nAwaiting the owner's confirmation.",
        escaped(&taken.option, false),
        if result.changed { "" } else { " (unchanged)" }
    ))
}

pub async fn cmd_decide(args: DecideArgs) -> Result<()> {
    if args.relay && !args.confirm && args.note.is_some() {
        use clap::error::ErrorKind::ArgumentConflict;
        clap::Error::raw(ArgumentConflict, "NOTE cannot be used with --relay alone").exit();
    }
    if args.relay && !args.confirm {
        let (result, raw): (MutationResult, _) = request(
            "decision.resolve",
            serde_json::json!({"id":args.id,"option":&args.option,"relay":true}),
        )
        .await?;
        let text = relay_text(args.id, &args.option, &result)?;
        return stdout(|out| output(&raw, args.format, &text, out));
    }
    let (mut result, mut raw): (MutationResult, _) = request(
        "decision.resolve",
        ResolveParams {
            id: args.id,
            option: args.option,
            note: args.note,
            standing_rule: args.standing_rule,
            relay: args.relay,
            confirm: args.confirm,
        },
    )
    .await?;
    if let Some(redo) = raw.get("redo").filter(|redo| !redo.is_null()).cloned() {
        if result.decision.decision.status != DecisionStatus::Overruled {
            bail!("redo metadata on a nonoverrule");
        }
        let id = args.id;
        let redo =
            super::away::collect_pages(redo, super::away::PageKind::Redo, |after| async move {
                let (page, raw): (GetResult, _) = request(
                    "decision.get",
                    GetParams {
                        id,
                        redo_after: Some(serde_json::from_value(after)?),
                        redo_limit: None,
                    },
                )
                .await?;
                if page.decision.decision.id != id
                    || page.decision.decision.status != DecisionStatus::Overruled
                {
                    bail!("redo page changed its decision");
                }
                raw.get("redo")
                    .cloned()
                    .context("missing redo continuation")
            })
            .await?;
        raw.as_object_mut()
            .context("invalid decision result")?
            .insert("redo".into(), redo);
        result = serde_json::from_value(raw.clone()).context("invalid assembled redo result")?;
    }
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
    let mut text = format!(
        "{} {}: {}{}",
        decision.id,
        word(decision.status)?,
        escaped(&answer.option, false),
        if result.changed { "" } else { " (unchanged)" }
    );
    if args.confirm {
        let note = answer.note.as_deref()
            .filter(|_| decision.id == args.id && decision.status == DecisionStatus::Resolved)
            .context("invalid relay confirmation result: expected the requested decision resolved with a note")?;
        let note = escaped(note, false);
        text.push_str(&format!(" by={}\nNote: {note}", answer.by));
    }
    if decision.status == DecisionStatus::Overruled {
        if let Some(redo) = result
            .redo
            .as_ref()
            .filter(|redo| redo.required_count.is_some())
        {
            let required = redo.required_count.context("missing redo requirement")?;
            let completed = redo
                .completed_count
                .context("missing redo completion count")?;
            let ids: Vec<_> = redo
                .task_ids
                .iter()
                .map(|id| escaped(&id.to_string(), false))
                .collect();
            let label = if result.changed {
                "Redo required"
            } else {
                "Redo already recorded"
            };
            match ids.as_slice() {
                [] if result.changed => {
                    text.push_str("\nRedo required: none. No files were reverted.")
                }
                [] => text.push_str("\nRedo already recorded: none."),
                [id] => {
                    text.push_str(&format!("\n{label}: {id}."));
                    if result.changed {
                        text.push_str(" It is open and unassigned; no files were reverted.");
                    }
                }
                _ => {
                    text.push_str(&format!("\n{label}:"));
                    for id in ids {
                        text.push_str(&format!("\n  {id}"));
                    }
                    if result.changed {
                        text.push_str("\nThey are open and unassigned; no files were reverted.");
                    }
                }
            }
            text.push_str(&format!(
                "\nTasks redone for {}: {completed}/{required} accepted again.",
                decision.id
            ));
        } else {
            text.push_str("\nRedo: not recorded by this daemon.");
        }
    }
    stdout(|out| output(&raw, args.format, &text, out))
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
    stdout(|out| output(&raw, format, &text, out))
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
    stdout(|out| match args.format {
        MessageFormat::Text => render_list(&result, out),
        MessageFormat::Json => output(&raw, args.format, "", out),
    })
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

    // Synthetic: provisional RPC results exercise status/identity validation,
    // including retries of a pre-existing system take with no relay mark.
    fn provisional_result() -> super::MutationResult {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/atm_decide/list.json"))
                .unwrap();
        let mut decision = value["decisions"].as_array_mut().unwrap().remove(0);
        decision["status"] = serde_json::json!("taken_provisionally");
        decision["provisional"] =
            serde_json::json!({"decision_id":"d41","option":"no","by":"system"});
        serde_json::from_value(serde_json::json!({"decision":decision,"changed":false})).unwrap()
    }

    // Break caught: treating a provisional relay as final, or falsely claiming
    // new provenance for an unchanged earlier nonrelay take.
    #[test]
    fn relay_text_is_provisional_and_refuses_malformed_results() {
        let id = super::DecisionId::new(41);
        let result = provisional_result();
        assert_eq!(
            super::relay_text(id, "no", &result).unwrap(),
            "d41 taken_provisionally: no (unchanged)\nAwaiting the owner's confirmation."
        );
        let mut changed = result.clone();
        changed.changed = true;
        assert_eq!(
            super::relay_text(id, "no", &changed).unwrap(),
            "d41 taken_provisionally: no\nAwaiting the owner's confirmation."
        );
        let mut bad = result.clone();
        bad.decision.decision.provisional = None;
        assert!(super::relay_text(id, "no", &bad).is_err());
        bad = result.clone();
        bad.decision.decision.status = super::DecisionStatus::Resolved;
        assert!(super::relay_text(id, "no", &bad).is_err());
        assert!(super::relay_text(super::DecisionId::new(42), "no", &result).is_err());
        assert!(super::relay_text(id, "yes", &result).is_err());
    }

    // Break caught: the standing-rule path or fabricated identity parses with relay.
    // A note without --confirm is refused pre-RPC by cmd_decide (CLI-tested).
    #[test]
    fn relay_arguments_exclude_rule_and_identity() {
        assert!(Args::try_parse_from(["atm", "decide", "d3", "no", "--relay"]).is_ok());
        for extra in [vec!["--rule"], vec!["--by", "owner"]] {
            let mut args = vec!["atm", "decide", "d3", "no", "--relay"];
            args.extend(extra);
            assert_eq!(Args::try_parse_from(args).unwrap_err().exit_code(), 2);
        }
    }

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
