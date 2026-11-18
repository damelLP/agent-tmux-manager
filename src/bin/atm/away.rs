//! Away switch and complete return reports; the daemon owns dependency policy.
use super::msg::{escaped, output, MessageFormat};
use super::output::stdout;
use anyhow::{bail, Context, Result};
use atm_org::{Decision, DecisionStatus, DigestPage, DigestRow, DigestSection, ImpactCounts};
use atmd::org::away::{AwayResult, BackResult, SwitchParams};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Args;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{fmt::Write as _, future::Future, time::Duration};

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

#[derive(Clone, Copy)]
pub(super) enum PageKind {
    Digest,
    Redo,
}
fn task_sequence(value: &Value) -> Result<u64> {
    let text = value.as_str().context("invalid task cursor")?;
    let seq = text
        .strip_prefix('t')
        .context("invalid task cursor")?
        .parse::<u64>()?;
    if format!("t{seq}") != text {
        bail!("invalid task cursor");
    }
    Ok(seq)
}

/// Assemble raw pages before output. Only framing is checked here, never graph,
/// ranking or progress policy. Later progress/unknown fields remain intact.
pub(super) async fn collect_pages<F, Fut>(
    mut page: Value,
    kind: PageKind,
    mut fetch: F,
) -> Result<Value>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let (items, stable): (&str, &[&str]) = match kind {
        PageKind::Digest => ("rows", &["snapshot_seq", "absence", "counts", "total_rows"]),
        PageKind::Redo => ("task_ids", &["required_count"]),
    };
    let frozen: Vec<_> = stable
        .iter()
        .map(|key| {
            Ok((
                *key,
                page.get(*key).context("missing page metadata")?.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    let relay = page.get("relay").cloned().unwrap_or(Value::Null);
    let mut combined = page.clone();
    let mut rows = Vec::new();
    let mut previous_cursor = None;
    let mut previous_task = None;
    loop {
        if matches!(kind, PageKind::Digest) && page.get("relay").unwrap_or(&Value::Null) != &relay {
            bail!("inconsistent page metadata: relay");
        }
        for (key, value) in &frozen {
            if page.get(*key) != Some(value) {
                bail!("inconsistent page metadata: {key}");
            }
        }
        let batch = page
            .get(items)
            .and_then(Value::as_array)
            .context("invalid page rows")?;
        if previous_cursor.is_some() && batch.is_empty() {
            bail!("empty continuation page");
        }
        if matches!(kind, PageKind::Redo) {
            for task in batch {
                let seq = task_sequence(task)?;
                if previous_task.is_some_and(|old| seq <= old) {
                    bail!("non-monotonic redo IDs");
                }
                previous_task = Some(seq);
            }
        }
        rows.extend(batch.iter().cloned());
        let next = page
            .get("next_after")
            .context("missing page cursor")?
            .clone();
        let fields = page.as_object().context("invalid page object")?;
        for (key, value) in fields.iter().filter(|(key, _)| key.as_str() != items) {
            combined
                .as_object_mut()
                .context("invalid page object")?
                .insert(key.clone(), value.clone());
        }
        if next.is_null() {
            break;
        }
        let rank = match kind {
            PageKind::Digest => {
                let rank = next.as_u64().context("invalid digest cursor")?;
                if Some(rank)
                    != rows
                        .len()
                        .checked_sub(1)
                        .and_then(|v| u64::try_from(v).ok())
                {
                    bail!("digest cursor does not match its rows");
                }
                rank
            }
            PageKind::Redo => {
                if batch.last() != Some(&next) {
                    bail!("redo cursor does not match its IDs");
                }
                task_sequence(&next)?
            }
        };
        if previous_cursor.is_some_and(|old| rank <= old) {
            bail!("non-monotonic page cursor");
        }
        previous_cursor = Some(rank);
        page = fetch(next).await?;
    }
    let declared = match kind {
        PageKind::Digest => combined.get("total_rows"),
        PageKind::Redo => combined.get("required_count"),
    }
    .context("missing page size")?;
    if !declared.is_null() && declared.as_u64() != u64::try_from(rows.len()).ok() {
        bail!("incomplete paged result");
    }
    if declared.is_null() && !rows.is_empty() {
        bail!("unmeasured redo has task IDs");
    }
    combined
        .as_object_mut()
        .context("invalid page object")?
        .insert(items.into(), Value::Array(rows));
    Ok(combined)
}

fn finish_decision(out: &mut String, decision: &Decision, caller: Option<&str>) -> Result<()> {
    if decision.status == DecisionStatus::TakenProvisionally {
        writeln!(out, "  Answer: atm decide {} <option>", decision.id)?;
    } else if decision.resolution.is_some() {
        writeln!(
            out,
            "  Answer caller: {}",
            escaped(caller.unwrap_or("not recorded"), false)
        )?;
    }
    Ok(())
}
fn decision_header(out: &mut String, decision: &Decision, impact: &ImpactCounts) -> Result<()> {
    let choice = decision
        .provisional
        .as_ref()
        .context("digest decision has no recorded take")?;
    writeln!(
        out,
        "{} {} = {} [{}; by={}{}]",
        decision.id,
        escaped(&decision.request.title, false),
        escaped(&choice.option, false),
        super::decide::word(decision.status)?,
        escaped(&choice.by.to_string(), false),
        choice
            .relayed_by
            .as_ref()
            .map(|seat| format!("; relayed by {}", escaped(&seat.to_string(), false)))
            .unwrap_or_default()
    )?;
    write!(
        out,
        "  Recorded work: {} started ({} accepted), {} not started",
        impact.started, impact.accepted, impact.not_started
    )?;
    if impact.cancelled != 0 {
        write!(out, "; {} cancelled excluded", impact.cancelled)?;
    }
    writeln!(out, ".")?;
    if decision.status != DecisionStatus::TakenProvisionally {
        writeln!(out, "  Work using this decision is listed below.")?;
    } else if impact.unknown != 0 {
        writeln!(
            out,
            "  Overruling is refused: a recorded task has an unknown state."
        )?;
    } else {
        writeln!(
            out,
            "  Overruling would reopen {} task{}.",
            impact.started,
            if impact.started == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
}
fn decision_section(out: &mut String, page: &DigestPage, wanted: DigestSection) -> Result<()> {
    let mut active: Option<(&Decision, Option<&str>)> = None;
    for row in &page.rows {
        match row {
            DigestRow::Decision {
                section,
                decision,
                impact,
                answer_caller_pane,
            } => {
                if let Some((decision, caller)) = active.take() {
                    finish_decision(out, decision, caller)?;
                }
                if *section == wanted {
                    decision_header(out, decision, impact)?;
                    active = Some((decision, answer_caller_pane.as_deref()));
                }
            }
            DigestRow::Task {
                decision_id,
                task_id,
                title,
                state,
                started,
                direct,
            } => {
                if let Some((decision, _)) = active {
                    if *decision_id != decision.id {
                        bail!("digest task is outside its decision group");
                    }
                    let label = if *state == atm_org::TaskState::Unknown {
                        "unknown"
                    } else if !started {
                        "not started"
                    } else if decision.status == DecisionStatus::TakenProvisionally {
                        "redo"
                    } else {
                        "used"
                    };
                    writeln!(
                        out,
                        "  {label}: {} [{}; {}] {}",
                        escaped(&task_id.to_string(), false),
                        super::decide::word(state)?,
                        if *direct { "direct" } else { "inherited" },
                        escaped(title, false)
                    )?;
                }
            }
            _ => {
                if let Some((decision, caller)) = active.take() {
                    finish_decision(out, decision, caller)?;
                }
            }
        }
    }
    if let Some((decision, caller)) = active {
        finish_decision(out, decision, caller)?;
    }
    Ok(())
}
fn back_text(result: &BackResult) -> Result<String> {
    let Some(page) = &result.digest else {
        let fallback = "Digest unavailable from this daemon; run atm decisions.";
        if !result.changed {
            return Ok(format!("Not away.\n{fallback}"));
        }
        let mut text = result
            .away_since
            .map(|since| format!("Back. You were away since {}.", time(since)))
            .unwrap_or_else(|| "Back.".into());
        let n = result.provisional_count;
        let p = result.pending_count;
        write!(text, "\n{n} decision{} taken provisionally {} for your answer; {p} {} pending: atm decisions\n{fallback}", if n == 1 { "" } else { "s" }, if n == 1 { "waits" } else { "wait" }, if p == 1 { "is" } else { "are" })?;
        return Ok(text);
    };
    let mut text = String::new();
    match (&page.absence, result.changed) {
        (Some(window), true) => {
            writeln!(text, "Back. You were away since {}.", time(window.since))?
        }
        (Some(window), false) => writeln!(
            text,
            "Not away. Showing the last absence: {} to {}.",
            time(window.since),
            time(window.until)
        )?,
        (None, false) => writeln!(text, "Not away. No previous absence.")?,
        (None, true) => bail!("changed return has no closed absence"),
    }
    writeln!(
        text,
        "Relay seat: {}",
        page.relay
            .as_ref()
            .map(|seat| escaped(&seat.to_string(), false))
            .unwrap_or_else(|| "none".into())
    )?;
    let counts = &page.counts;
    if counts.during_absence != 0 {
        writeln!(
            text,
            "Decided for you while away ({}):",
            counts.during_absence
        )?;
    } else if page.absence.is_some() {
        writeln!(text, "No decisions were taken for you while away.")?;
    } else {
        writeln!(text, "No absence to report.")?;
    }
    decision_section(&mut text, page, DigestSection::Taken)?;
    if counts.relayed == 0 {
        writeln!(text, "Relayed spoken answers: none.")?;
    } else {
        writeln!(text, "Relayed spoken answers ({}):", counts.relayed)?;
    }
    decision_section(&mut text, page, DigestSection::Relayed)?;
    if counts.earlier_provisional == 0 {
        writeln!(text, "Earlier provisional decisions: none.")?;
    } else {
        writeln!(
            text,
            "Earlier provisional decisions ({}):",
            counts.earlier_provisional
        )?;
    }
    decision_section(&mut text, page, DigestSection::Earlier)?;
    if counts.waiting_total == 0 {
        writeln!(text, "Still waiting: none.")?;
    } else {
        writeln!(text, "Still waiting ({}):", counts.waiting_total)?;
    }
    for row in &page.rows {
        match row {
            DigestRow::Waiting { decision } => {
                writeln!(
                    text,
                    "{} {} [{}{}]",
                    decision.id,
                    escaped(&decision.request.title, false),
                    super::decide::word(decision.class())?,
                    if decision.request.recommended.is_none() {
                        "; no recommendation"
                    } else {
                        ""
                    }
                )?;
                writeln!(text, "  Answer: atm decide {} <option>", decision.id)?;
            }
            DigestRow::WaitingTask {
                task_id,
                title,
                state,
                reason,
                held_on,
            } => {
                let reason = held_on
                    .map(|id| format!("stacking cap on {id}"))
                    .unwrap_or_else(|| escaped(reason.as_deref().unwrap_or("not recorded"), false));
                writeln!(
                    text,
                    "{} {} [{}; {}]",
                    escaped(&task_id.to_string(), false),
                    escaped(title, false),
                    super::decide::word(state)?,
                    reason
                )?;
            }
            _ => {}
        }
    }
    writeln!(
        text,
        "Numbers (all recorded time): {} decision{} taken for you; {} overruled.",
        counts.taken_for_owner,
        if counts.taken_for_owner == 1 { "" } else { "s" },
        counts.overruled
    )?;
    if counts.overruled == 0 {
        writeln!(text, "Tasks redone per overrule: none.")?;
    } else {
        writeln!(text, "Tasks redone per overrule:")?;
        for row in &page.rows {
            if let DigestRow::OverruleNumber {
                decision_id,
                completed,
                required,
                cancelled,
                caller_pane,
            } = row
            {
                write!(text, "  {decision_id}: ")?;
                if let (Some(done), Some(total)) = (completed, required) {
                    write!(text, "{done}/{total} accepted again")?;
                    if cancelled.is_some_and(|n| n != 0) {
                        write!(text, " ({} cancelled)", cancelled.unwrap_or(0))?;
                    }
                } else {
                    write!(text, "not recorded")?;
                }
                writeln!(
                    text,
                    "; caller={}",
                    escaped(caller_pane.as_deref().unwrap_or("not recorded"), false)
                )?;
            }
        }
    }
    writeln!(
        text,
        "Recorded links only: unlinked work and an unlinked task in progress are not included."
    )?;
    Ok(text)
}

pub async fn cmd_away(args: SwitchArgs) -> Result<()> {
    let (result, raw): (AwayResult, _) = request("owner.away").await?;
    stdout(|out| output(&raw, args.format, &away_text(&result), out))
}

pub async fn cmd_back(args: SwitchArgs) -> Result<()> {
    let (mut result, mut raw): (BackResult, _) = request("owner.back").await?;
    if let Some(page) = raw.get("digest").filter(|page| !page.is_null()).cloned() {
        let snapshot_seq = page
            .get("snapshot_seq")
            .and_then(Value::as_u64)
            .context("invalid digest snapshot")?;
        let page = collect_pages(page, PageKind::Digest, |after| async move {
            super::rpc::call(
                "owner.digest",
                serde_json::json!({"snapshot_seq":snapshot_seq,"after":after,"limit":100}),
                RPC_BUDGET,
            )
            .await
            .map_err(Into::into)
        })
        .await?;
        raw.as_object_mut()
            .context("invalid back result")?
            .insert("digest".into(), page);
        result = serde_json::from_value(raw.clone()).context("invalid assembled digest")?;
    }
    let text = back_text(&result)?;
    stdout(|out| output(&raw, args.format, text.trim_end_matches('\n'), out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page() -> Value {
        serde_json::from_str(include_str!(
            "../../../crates/atm-org/tests/fixtures/return_digest_expected.json"
        ))
        .unwrap()
    }
    fn back(page: Value) -> BackResult {
        serde_json::from_value(json!({"changed":false,"away_since":null,"provisional_count":1,"pending_count":0,"digest":page})).unwrap()
    }

    // Break caught: relays vanish from text or final/earlier/absence rows lose recorder attribution.
    #[test]
    fn digest_renders_relay_seat_section_and_provenance() {
        let mut page = page();
        page["absence"] = Value::Null;
        page["relay"] = json!("gamma/chief");
        page["counts"]["during_absence"] = json!(0);
        page["counts"]["relayed"] = json!(1);
        page["rows"][0]["section"] = json!("relayed");
        page["rows"][0]["decision"]["provisional"]["by"] = json!("owner");
        page["rows"][0]["decision"]["provisional"]["relayed_by"] = json!("gamma/chief");
        let text = back_text(&back(page.clone())).unwrap();
        assert_eq!(text, "Not away. No previous absence.\nRelay seat: gamma/chief\nNo absence to report.\nRelayed spoken answers (1):\nd11 Choose cache = small [taken_provisionally; by=owner; relayed by gamma/chief]\n  Recorded work: 1 started (1 accepted), 0 not started.\n  Overruling would reopen 1 task.\n  redo: t14 [accepted; direct] Write cache\n  Answer: atm decide d11 <option>\nEarlier provisional decisions: none.\nStill waiting: none.\nNumbers (all recorded time): 1 decision taken for you; 0 overruled.\nTasks redone per overrule: none.\nRecorded links only: unlinked work and an unlinked task in progress are not included.\n");
        for status in ["resolved", "overruled"] {
            page["rows"][0]["decision"]["status"] = json!(status);
            page["rows"][0]["decision"]["resolution"] = json!({"decision_id":"d11","option":"small","by":"owner","note":null,"standing_rule":false});
            let text = back_text(&back(page.clone())).unwrap();
            assert!(text.contains(&format!("[{status}; by=owner; relayed by gamma/chief]")));
            assert!(text.contains("  used: t14"));
            assert!(text.contains("  Answer caller: not recorded"));
            assert!(!text.contains("  Answer: atm decide d11"));
        }
        for (section, label) in [
            ("taken", "Decided for you while away (1):"),
            ("earlier", "Earlier provisional decisions (1):"),
        ] {
            let mut page = page.clone();
            page["rows"][0]["decision"]["status"] = json!("taken_provisionally");
            page["rows"][0]["decision"]["resolution"] = Value::Null;
            page["rows"][0]["section"] = json!(section);
            page["counts"]["relayed"] = json!(0);
            page["counts"][if section == "taken" {
                "during_absence"
            } else {
                "earlier_provisional"
            }] = json!(1);
            let text = back_text(&back(page)).unwrap();
            assert!(text.contains(label));
            assert!(text.contains("Relayed spoken answers: none."));
            assert_eq!(text.matches("; relayed by gamma/chief]").count(), 1);
        }
    }

    // Break caught: defaulted reply fields become mandatory or old JSON is rewritten by the client.
    #[tokio::test]
    async fn legacy_digest_fields_default_without_changing_raw_json() {
        let mut page = page();
        page.as_object_mut().unwrap().remove("relay");
        page["counts"].as_object_mut().unwrap().remove("relayed");
        let original = page.clone();
        let combined = collect_pages(page, PageKind::Digest, |_| async {
            panic!("unexpected continuation")
        })
        .await
        .unwrap();
        assert_eq!(combined, original);
        let result = back(combined);
        assert_eq!(result.digest.as_ref().unwrap().counts.relayed, 0);
        assert_eq!(result.digest.as_ref().unwrap().relay, None);
        let text = back_text(&result).unwrap();
        assert!(text.contains("Relay seat: none\n"));
        assert!(text.contains("Relayed spoken answers: none.\n"));
        let fallback: BackResult = serde_json::from_value(
            json!({"changed":false,"away_since":null,"provisional_count":0,"pending_count":0}),
        )
        .unwrap();
        assert_eq!(
            back_text(&fallback).unwrap(),
            "Not away.\nDigest unavailable from this daemon; run atm decisions."
        );
    }

    // Break caught: assembling later pages silently substitutes a different frozen relay seat.
    #[tokio::test]
    async fn digest_pages_require_consistent_relay_metadata() {
        for (first_seat, next_seat, allowed) in [
            (Some("gamma/chief"), Some("beta/lead"), false),
            (Some("gamma/chief"), None, false),
            (None, None, true),
        ] {
            let original = page();
            let mut first = original.clone();
            let mut next = original.clone();
            first["rows"] = json!([original["rows"][0]]);
            first["next_after"] = json!(0);
            next["rows"] = json!([original["rows"][1]]);
            first["future_digest"] = json!({"kept":true});
            if let Some(seat) = first_seat {
                first["relay"] = json!(seat);
            } else {
                first.as_object_mut().unwrap().remove("relay");
            }
            next["relay"] = json!(next_seat);
            let combined = collect_pages(first, PageKind::Digest, |after| {
                assert_eq!(after, 0);
                std::future::ready(Ok(next.clone()))
            })
            .await;
            if allowed {
                let combined = combined.unwrap();
                assert_eq!(combined["rows"], original["rows"]);
                assert_eq!(combined["future_digest"], json!({"kept":true}));
            } else {
                assert_eq!(
                    combined.unwrap_err().to_string(),
                    "inconsistent page metadata: relay"
                );
            }
        }
        for key in ["snapshot_seq", "absence", "counts", "total_rows"] {
            let mut page = page();
            page.as_object_mut().unwrap().remove(key);
            assert!(collect_pages(page, PageKind::Digest, |_| async {
                panic!("unexpected continuation")
            })
            .await
            .is_err());
        }
    }
}
