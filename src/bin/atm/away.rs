//! `atm away` and `atm back`: one request each; the daemon decides who may.
use super::msg::{output, MessageFormat};
use anyhow::{Context, Result};
use atmd::org::away::{AwayResult, BackResult, SwitchParams};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Args;
use serde::de::DeserializeOwned;
use std::{io, time::Duration};

const RPC_BUDGET: Duration = Duration::from_secs(27);

#[derive(Debug, Args)]
pub struct SwitchArgs {
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

async fn request<R: DeserializeOwned>(method: &str) -> Result<(R, serde_json::Value)> {
    let raw = super::rpc::call(method, serde_json::to_value(SwitchParams {})?, RPC_BUDGET).await?;
    let result = serde_json::from_value(raw.clone()).context("invalid away result from daemon")?;
    Ok((result, raw))
}

fn time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn away_text(result: &AwayResult) -> String {
    let since = time(result.away_since);
    if !result.changed {
        return format!("Already away since {since}.");
    }
    let mut text = format!(
        "Away since {since}.\nDetails: the agent that asked decides. Changes to the intent that can be undone: taken provisionally on the recommended option. Anything that cannot be undone or reaches outside, and any action, hire or budget: waits for you."
    );
    let caps: Vec<_> = result
        .stacking_caps
        .iter()
        .map(|(project, cap)| format!("{project} {cap}"))
        .collect();
    if !caps.is_empty() {
        text.push_str(&format!("\nStacking cap: {}", caps.join(", ")));
    }
    match result.pending_count {
        0 => {}
        1 => text.push_str("\n1 pending decision is now handled by these rules: atm decisions"),
        n => text.push_str(&format!(
            "\n{n} pending decisions are now handled by these rules: atm decisions"
        )),
    }
    text
}

fn back_text(result: &BackResult) -> String {
    if !result.changed {
        return "Not away.".into();
    }
    let mut text = match result.away_since {
        Some(since) => format!("Back. You were away since {}.", time(since)),
        None => "Back.".into(),
    };
    let (taken, verb) = match result.provisional_count {
        1 => ("1 decision".to_owned(), "waits"),
        n => (format!("{n} decisions"), "wait"),
    };
    let pending = match result.pending_count {
        1 => "1 is".to_owned(),
        n => format!("{n} are"),
    };
    text.push_str(&format!(
        "\n{taken} taken provisionally {verb} for your answer; {pending} pending: atm decisions\nDigest: not built yet (unit A2)."
    ));
    text
}

pub async fn cmd_away(args: SwitchArgs) -> Result<()> {
    let (result, raw): (AwayResult, _) = request("owner.away").await?;
    output(
        &raw,
        args.format,
        &away_text(&result),
        &mut io::stdout().lock(),
    )
}

pub async fn cmd_back(args: SwitchArgs) -> Result<()> {
    let (result, raw): (BackResult, _) = request("owner.back").await?;
    output(
        &raw,
        args.format,
        &back_text(&result),
        &mut io::stdout().lock(),
    )
}
