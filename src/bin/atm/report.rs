//! One org.report request; text renders only the daemon's supplied projection.
use super::msg::{escaped, output, MessageFormat};
use anyhow::{bail, Context, Result};
use atm_core::Money;
use atm_org::{
    DepartmentCostReport, Direction, OutcomeCounts, OverBudget, ProjectId, ProjectReport, Report,
    ReportParams, TaskReport, Trend,
};
use chrono::{DateTime, TimeDelta, Utc};
use clap::Args;
use serde::Deserialize;
use std::{collections::BTreeSet, fmt::Display, io::Write, time::Duration};

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[arg(long)]
    project: Option<ProjectId>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long,default_value_t=5,value_parser=clap::value_parser!(u16).range(1..=1000))]
    bucket: u16,
    #[arg(short = 'f', long, value_enum, default_value = "text")]
    format: MessageFormat,
}
// Reuse the pure projection; seq is a daemon-envelope requirement, not a join.
#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "seq")]
    _seq: u64,
    #[serde(flatten)]
    report: Report,
    #[serde(default)]
    departments: Vec<DepartmentCostReport>,
    warning: Option<String>,
}
fn time(value: Option<&str>, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|value| {
            let relative = value
                .strip_suffix('h')
                .map(|n| (n, 3600))
                .or_else(|| value.strip_suffix('d').map(|n| (n, 86400)));
            if let Some((n, unit)) = relative {
                if n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()) {
                    bail!("relative report time must be Nh or Nd");
                }
                let seconds = n
                    .parse::<i64>()
                    .ok()
                    .and_then(|n| n.checked_mul(unit))
                    .and_then(TimeDelta::try_seconds)
                    .context("relative report time is out of range")?;
                now.checked_sub_signed(seconds)
                    .context("relative report time is out of range")
            } else {
                DateTime::parse_from_rfc3339(value)
                    .map(|t| t.with_timezone(&Utc))
                    .context("report time must be RFC 3339, Nh or Nd")
            }
        })
        .transpose()
}
fn parameters(args: &ReportArgs, now: DateTime<Utc>) -> Result<ReportParams> {
    let since = time(args.since.as_deref(), now)?;
    let until = time(args.until.as_deref(), now)?;
    if since.is_some_and(|since| since > until.unwrap_or(now)) {
        bail!("since must not be after until");
    }
    Ok(ReportParams {
        project_id: args.project.clone(),
        since,
        until,
        bucket: args.bucket,
        after: None,
        limit: 100,
    })
}
fn money(value: Option<Money>) -> String {
    let Some(value) = value else {
        return "unknown".into();
    };
    let micros = i128::from(value.as_microdollars());
    let cents = (micros.unsigned_abs() + 5000) / 10000;
    format!(
        "{}${}.{:02}",
        if micros < 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}
fn elapsed(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    let mut out = if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        String::new()
    };
    if seconds > 0 || out.is_empty() {
        out.push_str(&format!("{seconds}s"));
    }
    out
}
fn number(value: Option<impl Display>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}
fn percent(value: Option<u8>) -> String {
    value
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "unknown".into())
}
fn over_budget(value: &OverBudget) -> String {
    let mut fields = Vec::new();
    if let Some(turns) = value.turns {
        fields.push(format!("turns {turns}"));
    }
    if value.cost.is_some() {
        fields.push(format!("cost {}", money(value.cost)));
    }
    if let Some(seconds) = value.deadline_seconds {
        fields.push(format!("deadline {}", elapsed(seconds)));
    }
    if fields.is_empty() {
        "unknown".into()
    } else {
        fields.join(", ")
    }
}
#[derive(Default)]
struct Totals {
    tasks: OutcomeCounts,
    steps: u64,
    tasks_with_steps: u64,
    cost: Option<Money>,
    unknown: u64,
}
fn totals<'a>(projects: impl Iterator<Item = &'a ProjectReport>) -> Totals {
    projects.fold(Totals::default(), |mut sum, p| {
        sum.tasks.accepted = sum.tasks.accepted.saturating_add(p.tasks.accepted);
        sum.tasks.cancelled = sum.tasks.cancelled.saturating_add(p.tasks.cancelled);
        sum.tasks.open = sum.tasks.open.saturating_add(p.tasks.open);
        sum.tasks.unknown = sum.tasks.unknown.saturating_add(p.tasks.unknown);
        sum.steps = sum.steps.saturating_add(p.owner_step_ins.total);
        sum.tasks_with_steps = sum.tasks_with_steps.saturating_add(p.tasks_with_step_ins);
        if let Some(cost) = p.cost {
            sum.cost = Some(sum.cost.map_or(cost, |old| old + cost));
        }
        sum.unknown = sum.unknown.saturating_add(p.cost_unknown_tasks);
        sum
    })
}
fn header(label: &str, totals: &Totals, harnesses: &str, out: &mut impl Write) -> Result<()> {
    let cost = if totals.cost.is_some() {
        format!("{} harness-reported{harnesses}", money(totals.cost))
    } else {
        "unknown".into()
    };
    write!(out,"{label}  {} accepted  {} cancelled  {} open  {} unknown   you stepped in {} times on {} tasks   {cost}, {} tasks unknown",totals.tasks.accepted,totals.tasks.cancelled,totals.tasks.open,totals.tasks.unknown,totals.steps,totals.tasks_with_steps,totals.unknown)?;
    Ok(())
}
fn metric(
    name: &str,
    before: Option<impl Display>,
    after: Option<impl Display>,
    direction: Direction,
    out: &mut impl Write,
) -> Result<()> {
    write!(
        out,
        "   {name} {}→{} {}",
        number(before),
        number(after),
        serde_json::to_value(direction)?.as_str().unwrap_or("none")
    )?;
    Ok(())
}
fn render_trend(trend: &Trend, out: &mut impl Write) -> Result<()> {
    let before = trend
        .buckets
        .len()
        .checked_sub(2)
        .and_then(|i| trend.buckets.get(i));
    let after = trend.buckets.last();
    write!(out, "  trend per {} tasks, not size-adjusted", trend.bucket)?;
    metric(
        "turns",
        before.and_then(|b| b.turns),
        after.and_then(|b| b.turns),
        trend.direction.turns,
        out,
    )?;
    metric(
        "peak context",
        before.and_then(|b| b.peak_context_percent),
        Some(format!(
            "{}%",
            number(after.and_then(|b| b.peak_context_percent))
        )),
        trend.direction.peak_context_percent,
        out,
    )?;
    metric(
        "reviews",
        before.and_then(|b| b.review_rounds),
        after.and_then(|b| b.review_rounds),
        trend.direction.review_rounds,
        out,
    )?;
    metric(
        "rework",
        before.and_then(|b| b.changes_requested),
        after.and_then(|b| b.changes_requested),
        trend.direction.changes_requested,
        out,
    )?;
    metric(
        "cost",
        Some(money(before.and_then(|b| b.cost))),
        Some(money(after.and_then(|b| b.cost))),
        trend.direction.cost,
        out,
    )?;
    writeln!(out)?;
    Ok(())
}
fn render_task(task: &TaskReport, out: &mut impl Write) -> Result<()> {
    let state = serde_json::to_value(task.state)?;
    write!(
        out,
        "  {} {}  {}  {}",
        escaped(&task.task_id.to_string(), false),
        state.as_str().unwrap_or("unknown"),
        task.agent_id
            .as_ref()
            .map(|id| escaped(&id.to_string(), false))
            .unwrap_or_else(|| "-".into()),
        elapsed(
            task.usage
                .seconds_in_state
                .values()
                .fold(0u64, |sum, s| sum.saturating_add(*s))
        )
    )?;
    if !task.usage.seconds_in_state.is_empty() {
        let states = task
            .usage
            .seconds_in_state
            .iter()
            .map(|(state, seconds)| {
                let value = serde_json::to_value(state)?;
                Ok(format!(
                    "{} {}",
                    value.as_str().unwrap_or("unknown").replace('_', " "),
                    elapsed(*seconds)
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        write!(out, " ({})", states.join(", "))?;
    }
    let steps = &task.owner_step_ins;
    write!(
        out,
        "  turns {}  reviews {}  rework {}  you {}",
        number(task.usage.turns),
        number(task.usage.review_rounds),
        number(task.usage.changes_requested),
        steps.total
    )?;
    let kinds = [
        ("verdict", steps.verdict),
        ("redirect", steps.redirect),
        ("answer", steps.answer),
        ("message", steps.message),
        ("redo", steps.redo),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .map(|(kind, n)| format!("{kind} {n}"))
    .collect::<Vec<_>>();
    if !kinds.is_empty() {
        write!(out, " ({})", kinds.join(", "))?;
    }
    write!(
        out,
        "  context {} peak, {} at submit  {}",
        percent(task.usage.peak_context_percent),
        percent(task.context_percent_at_submit),
        money(task.usage.cost)
    )?;
    if task.usage.cost.is_some() {
        write!(out, " harness-reported")?;
        let facts = task
            .cost_source
            .harnesses
            .iter()
            .chain(&task.cost_source.models)
            .map(|s| escaped(s, false))
            .collect::<Vec<_>>();
        if !facts.is_empty() {
            write!(out, " ({})", facts.join("; "))?;
        }
    }
    if let Some(over) = &task.over_budget {
        write!(out, "  over budget {}", over_budget(over))?;
    }
    writeln!(out)?;
    Ok(())
}
fn render(reply: &Reply, out: &mut impl Write) -> Result<()> {
    let report = &reply.report;
    header("Total", &totals(report.projects.iter()), "", out)?;
    writeln!(out)?;
    for project in &report.projects {
        let id = project
            .project_id
            .as_ref()
            .map(|id| escaped(&id.to_string(), false))
            .unwrap_or_else(|| "unplaced".into());
        // A partial task page cannot establish the full project's source list.
        let sources = if report.next_after.is_some() {
            BTreeSet::new()
        } else {
            report
                .tasks
                .iter()
                .filter(|t| t.project_id == project.project_id && t.usage.cost.is_some())
                .flat_map(|t| &t.cost_source.harnesses)
                .map(|s| escaped(s, false))
                .collect::<BTreeSet<_>>()
        };
        let source = if sources.is_empty() {
            String::new()
        } else {
            format!(" ({})", sources.into_iter().collect::<Vec<_>>().join(", "))
        };
        header(&id, &totals(std::iter::once(project)), &source, out)?;
        let d = &project.decisions;
        write!(
            out,
            "   decisions {} taken for you, {} agent decided, {} relayed, {} overruled, {} tasks redone",
            d.taken_for_owner, d.agent_decided, d.relayed, d.overruled, d.tasks_redone
        )?;
        if let Some(over) = &project.over_budget {
            write!(out, "   over budget {}", over_budget(over))?;
        }
        writeln!(out)?;
        render_trend(&project.trend, out)?;
        for task in report
            .tasks
            .iter()
            .filter(|t| t.project_id == project.project_id)
        {
            render_task(task, out)?;
        }
    }
    if !reply.departments.is_empty() || reply.warning.is_some() {
        writeln!(out, "\nDepartments:")?;
        for department in &reply.departments {
            let cost = if department.cost.is_none() && department.cost_unknown_tasks == 0 {
                "no costed tasks in view".to_owned()
            } else {
                money(department.cost)
            };
            write!(
                out,
                "  {} ({}): {cost}",
                escaped(&department.title, false),
                department.id
            )?;
            if department.cost.is_some() {
                write!(out, " harness-reported")?;
            }
            if department.cost_unknown_tasks > 0 {
                write!(
                    out,
                    " ({} tasks with unknown cost)",
                    department.cost_unknown_tasks
                )?;
            }
            writeln!(out)?;
        }
        if let Some(warning) = &reply.warning {
            writeln!(out, "  Warning: {}", escaped(warning, false))?;
        }
    }
    if let Some(after) = &report.next_after {
        writeln!(
            out,
            "Task page: next_after {}; project totals cover the whole scope.",
            escaped(&after.to_string(), false)
        )?;
    }
    writeln!(out,"Cost is a lower bound; budget savings are an upper bound. Whole-task totals; decisions use requester visibility and request time.")?;
    Ok(())
}
pub async fn cmd_report(args: ReportArgs) -> Result<()> {
    let params = parameters(&args, Utc::now())?;
    // Match decision.list: 22 s router plus 5 s startup/transport margin.
    let raw = super::rpc::call(
        "org.report",
        serde_json::to_value(params)?,
        Duration::from_secs(27),
    )
    .await?;
    let reply: Reply = serde_json::from_value(raw.clone()).context("invalid report from daemon")?;
    super::output::stdout(|out| match args.format {
        MessageFormat::Text => render(&reply, out),
        MessageFormat::Json => output(&raw, args.format, "", out),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::Money;
    use chrono::{DateTime, Utc};
    fn now() -> DateTime<Utc> {
        "2026-02-01T12:00:00Z".parse().unwrap()
    }
    fn args(since: Option<&str>, until: Option<&str>) -> ReportArgs {
        ReportArgs {
            project: None,
            since: since.map(str::to_owned),
            until: until.map(str::to_owned),
            bucket: 5,
            format: MessageFormat::Text,
        }
    }
    // Break caught: separate clocks/unchecked arithmetic distort relative windows.
    #[test]
    fn relative_filters_share_one_supplied_clock_and_check_overflow() {
        let p = parameters(&args(Some("2d"), Some("1h")), now()).unwrap();
        assert_eq!(p.since, Some("2026-01-30T12:00:00Z".parse().unwrap()));
        assert_eq!(p.until, Some("2026-02-01T11:00:00Z".parse().unwrap()));
        let p = parameters(&args(Some("2026-01-01T02:00:00+02:00"), None), now()).unwrap();
        assert_eq!(p.since, Some("2026-01-01T00:00:00Z".parse().unwrap()));
        assert_eq!(p.until, None);
        for (since, until) in [
            ("1h", Some("2d")),
            ("999999999999999999999d", None),
            ("2562047788015215h", None),
            ("-1h", None),
            ("1.5h", None),
            ("", None),
            ("garbage", None),
        ] {
            assert!(
                parameters(&args(Some(since), until), now()).is_err(),
                "{since:?}"
            );
        }
        let min = DateTime::<Utc>::MIN_UTC;
        assert!(parameters(&args(Some("1d"), None), min).is_err());
    }
    // Break caught: relays disappear from text, or older replies without the count stop rendering.
    #[test]
    fn report_renders_relays_and_defaults_legacy_replies() {
        for relayed in [None, Some(2)] {
            let mut value: serde_json::Value = serde_json::from_str(include_str!(
                "../../../tests/fixtures/atm_report/report.json"
            ))
            .unwrap();
            if let Some(count) = relayed {
                value["projects"][0]["decisions"]["relayed"] = serde_json::json!(count);
            }
            let reply: Reply = serde_json::from_value(value).unwrap();
            let mut out = vec![];
            render(&reply, &mut out).unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains(&format!("decisions 0 taken for you, 0 agent decided, {} relayed, 0 overruled, 0 tasks redone", relayed.unwrap_or(0))), "{text}");
        }
    }

    // Break caught: null cost becomes zero, money overflows or large wall time wraps.
    #[test]
    fn display_preserves_unknown_zero_money_and_large_durations() {
        assert_eq!(money(None), "unknown");
        assert_eq!(money(Some(Money::zero())), "$0.00");
        assert_eq!(money(Some(Money::from_microdollars(1_235_000))), "$1.24");
        assert_eq!(
            money(Some(Money::from_microdollars(i64::MAX))),
            "$9223372036854.78"
        );
        assert_eq!(
            money(Some(Money::from_microdollars(i64::MIN))),
            "-$9223372036854.78"
        );
        assert_eq!(elapsed(0), "0s");
        assert_eq!(elapsed(65), "1m5s");
        assert_eq!(elapsed(7800), "2h10m");
        assert_eq!(elapsed(u64::MAX), "5124095576030431h0m15s");
    }
}
