//! Single-projection organisation overview; no local identity or reporting lookup.

use super::msg::{escaped, output, MessageFormat};
use super::output::stdout;
use anyhow::{Context, Result};
use atm_org::{Address, AgentId, ProjectId, TaskId};
use clap::Args;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
    io::Write,
    time::Duration,
};

#[derive(Debug, Args)]
pub struct OrgArgs {
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

// Binary-local CLI/TUI projections: unrendered/additive fields remain in the raw reply.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct OrgTree {
    #[serde(default)]
    pub(super) seq: Option<u64>,
    generated_at: String,
    reporting: Reporting,
    needs_owner: NeedsOwner,
    pub(super) projects: Vec<Project>,
    unplaced_tasks: Vec<OpenTask>,
}

#[derive(Debug, Clone, Deserialize)]
struct Reporting {
    state: String,
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NeedsOwner {
    away_since: Option<String>,
    owner_unread: u64,
    unnamed_attention: u64,
    items: Vec<NeedItem>,
    items_total: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct NeedItem {
    kind: String,
    id: String,
    agent_id: Option<AgentId>,
    project: Option<ProjectId>,
    since: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Project {
    id: ProjectId,
    root: String,
    pub(super) seats: Vec<Seat>,
    tasks_open: u64,
    tasks: Vec<OpenTask>,
    decisions: DecisionCounts,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Seat {
    pub(super) agent_id: AgentId,
    pub(super) pane: Option<String>,
    state: String,
    harness: Option<String>,
    context_percentage: Option<f64>,
    manager: Option<Address>,
    line_error: Option<String>,
    tasks: Vec<TaskId>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenTask {
    task_id: TaskId,
    title: String,
    kind: String,
    state: String,
    agent_id: Option<AgentId>,
    creator: Address,
}

#[derive(Debug, Clone, Deserialize)]
struct DecisionCounts {
    pending: u64,
    provisional: u64,
}

fn cell(value: impl Display) -> String {
    escaped(&value.to_string(), false)
}

fn optional(value: Option<&impl Display>) -> String {
    value.map(cell).unwrap_or_else(|| "-".into())
}

fn render_task(task: &OpenTask, indent: &str, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "{indent}{} [{}; kind={}; agent={}; creator={}] {}",
        cell(&task.task_id),
        cell(&task.state),
        cell(&task.kind),
        optional(task.agent_id.as_ref()),
        cell(&task.creator),
        cell(&task.title),
    )?;
    Ok(())
}

fn render_more(total: u64, listed: usize, indent: &str, out: &mut impl Write) -> Result<()> {
    let remaining = total.saturating_sub(listed as u64);
    if remaining > 0 {
        writeln!(out, "{indent}+{remaining} more")?;
    }
    Ok(())
}

fn render_seat(seat: &Seat, depth: usize, reference: bool, out: &mut impl Write) -> Result<()> {
    let context = seat.context_percentage.map(|p| format!("{p}%"));
    let tasks = if seat.tasks.is_empty() {
        "-".into()
    } else {
        seat.tasks.iter().map(cell).collect::<Vec<_>>().join(",")
    };
    write!(
        out,
        "    {}{} [{}; harness={}; context={}; tasks={tasks}]",
        "  ".repeat(depth),
        cell(&seat.agent_id),
        cell(&seat.state),
        optional(seat.harness.as_ref()),
        optional(context.as_ref()),
    )?;
    if reference {
        if let Some(manager) = &seat.manager {
            write!(out, " -> {}", cell(manager))?;
        }
    }
    if let Some(error) = &seat.line_error {
        write!(out, "; line_error={}", cell(error))?;
    }
    writeln!(out)?;
    Ok(())
}

fn render_seats(project: &Project, nested: bool, out: &mut impl Write) -> Result<()> {
    if project.seats.is_empty() {
        writeln!(out, "  Seats: none")?;
        return Ok(());
    }
    writeln!(out, "  Seats:")?;
    if !nested {
        for seat in &project.seats {
            render_seat(seat, 0, false, out)?;
        }
        return Ok(());
    }
    let members: BTreeSet<_> = project.seats.iter().map(|seat| &seat.agent_id).collect();
    let mut children: BTreeMap<&AgentId, Vec<&Seat>> = BTreeMap::new();
    for seat in &project.seats {
        if let Some(Address::Agent(manager)) = &seat.manager {
            if manager.project == project.id {
                children.entry(manager).or_default().push(seat);
            }
        }
    }
    let roots = project.seats.iter().filter(|seat| match &seat.manager {
        Some(Address::Agent(manager)) => {
            manager.project != project.id || !members.contains(manager)
        }
        _ => true,
    });
    let mut emitted = BTreeSet::new();
    // Append unseen seats as roots if an inconsistent graph has no reachable root.
    // Iteration + emitted IDs keep deep chains/cycles from recursing or hiding seats.
    for root in roots.chain(&project.seats) {
        let mut stack = vec![(root, 0)];
        while let Some((seat, depth)) = stack.pop() {
            if !emitted.insert(&seat.agent_id) {
                continue;
            }
            render_seat(seat, depth, depth == 0, out)?;
            if let Some(children) = children.get(&seat.agent_id) {
                stack.extend(children.iter().rev().map(|child| (*child, depth + 1)));
            }
        }
    }
    Ok(())
}

fn render_tree(tree: &OrgTree, out: &mut impl Write) -> Result<()> {
    let needs = &tree.needs_owner;
    let unnamed = if needs.unnamed_attention == 1 {
        "unnamed session needs attention"
    } else {
        "unnamed sessions need attention"
    };
    writeln!(
        out,
        "Needs you: {} items; {} unread for you; {} {unnamed}; away since {}",
        needs.items_total,
        needs.owner_unread,
        needs.unnamed_attention,
        optional(needs.away_since.as_ref()),
    )?;
    for item in &needs.items {
        writeln!(
            out,
            "  {} {} agent={} project={} since={}",
            cell(&item.kind),
            cell(&item.id),
            optional(item.agent_id.as_ref()),
            optional(item.project.as_ref()),
            optional(item.since.as_ref()),
        )?;
    }
    render_more(needs.items_total, needs.items.len(), "  ", out)?;
    writeln!(out, "Generated: {}", cell(&tree.generated_at))?;
    let nested = tree.reporting.state == "ok";
    if !nested {
        writeln!(
            out,
            "Reporting unavailable: {}; seats are listed flat.",
            optional(tree.reporting.reason.as_ref()),
        )?;
    }
    if tree.projects.is_empty() {
        writeln!(out, "Projects: none")?;
    }
    for project in &tree.projects {
        writeln!(
            out,
            "\nProject {} ({})",
            cell(&project.id),
            cell(&project.root)
        )?;
        render_seats(project, nested, out)?;
        writeln!(out, "  Open tasks: {}", project.tasks_open)?;
        for task in &project.tasks {
            render_task(task, "    ", out)?;
        }
        render_more(project.tasks_open, project.tasks.len(), "    ", out)?;
        writeln!(
            out,
            "  Decisions: {} pending, {} provisional",
            project.decisions.pending, project.decisions.provisional,
        )?;
    }
    writeln!(out, "\nUnplaced tasks: {}", tree.unplaced_tasks.len())?;
    for task in &tree.unplaced_tasks {
        render_task(task, "  ", out)?;
    }
    Ok(())
}

pub async fn cmd_org(args: OrgArgs) -> Result<()> {
    // 12 s router + 5 s startup/transport margin; no joins or fallback requests.
    let raw = super::rpc::call("org.tree", serde_json::json!({}), Duration::from_secs(17)).await?;
    let tree: OrgTree =
        serde_json::from_value(raw.clone()).context("invalid org tree from daemon")?;
    stdout(|out| match args.format {
        MessageFormat::Text => render_tree(&tree, out),
        MessageFormat::Json => output(&raw, args.format, "", out),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::io;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../tests/fixtures/atm_org/tree.json")).unwrap()
    }

    fn rendered(value: Value) -> String {
        let tree = serde_json::from_value(value).unwrap();
        let mut out = Vec::new();
        render_tree(&tree, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn empty() -> Value {
        json!({
            "generated_at":"2026-01-15T12:05:00Z", "reporting":{"state":"ok","reason":null},
            "needs_owner":{"away_since":null,"owner_unread":0,"unnamed_attention":0,"items":[],"items_total":0},
            "projects":[], "unplaced_tasks":[]
        })
    }

    // Break caught: empty/null/zero values disappear or become invented values.
    #[test]
    fn renderer_handles_empty_null_and_zero() {
        assert_eq!(rendered(empty()), "Needs you: 0 items; 0 unread for you; 0 unnamed sessions need attention; away since -\nGenerated: 2026-01-15T12:05:00Z\nProjects: none\n\nUnplaced tasks: 0\n");
        let text = rendered(fixture());
        assert!(text.contains("gamma/chief [ready; harness=pi; context=0%; tasks=-]"));
        assert!(text.contains("alpha/watch [down; harness=-; context=-; tasks=-]"));
        assert!(text.contains("1 unnamed session needs attention"));
        let mut value = empty();
        value["needs_owner"]["unnamed_attention"] = json!(2);
        assert!(rendered(value).starts_with(
            "Needs you: 0 items; 0 unread for you; 2 unnamed sessions need attention;"
        ));
    }

    // Break caught: closed display enums hide newer daemon vocabulary.
    #[test]
    fn renderer_preserves_future_vocabulary() {
        let mut value = fixture();
        value["projects"][0]["seats"][0]["state"] = json!("hibernating");
        value["projects"][0]["seats"][0]["harness"] = json!("future-harness");
        value["projects"][0]["tasks"][0]["state"] = json!("future-state");
        value["projects"][0]["tasks"][0]["kind"] = json!("future-kind");
        value["needs_owner"]["items"][0]["kind"] = json!("future-need");
        let text = rendered(value);
        assert!(text.contains(
            "alpha/lead [hibernating; harness=future-harness; context=42.5%; tasks=t11]"
        ));
        assert!(text.contains("t11 [future-state; kind=future-kind;"));
        assert!(text.contains("  future-need d41 agent=alpha/lead"));
    }

    // Synthetic: defensive inconsistent edges are not valid daemon role-reader output.
    // Break caught: recursion, cycle loops, duplicate children or lost disconnected seats.
    #[test]
    fn renderer_visits_deep_wide_and_disconnected_seats_once() {
        let mut value = fixture();
        value["projects"].as_array_mut().unwrap().truncate(1);
        let template = value["projects"][0]["seats"][0].clone();
        let seats = value["projects"][0]["seats"].as_array_mut().unwrap();
        seats.clear();
        for n in 0..200 {
            let mut seat = template.clone();
            seat["agent_id"] = json!(format!("alpha/seat-{n:03}"));
            seat["manager"] = if n == 0 {
                json!("owner")
            } else {
                json!(format!("alpha/seat-{:03}", n - 1))
            };
            seats.push(seat);
        }
        let text = rendered(value.clone());
        assert_eq!(
            text.lines()
                .filter(|line| line.trim_start().starts_with("alpha/seat-"))
                .count(),
            200
        );
        assert!(text.contains(&format!("{}alpha/seat-199 [", " ".repeat(402))));
        for seat in value["projects"][0]["seats"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .skip(1)
        {
            seat["manager"] = json!("alpha/seat-000");
        }
        let text = rendered(value.clone());
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("      alpha/seat-"))
                .count(),
            199
        );
        let seats = value["projects"][0]["seats"].as_array_mut().unwrap();
        seats.clear();
        for (id, manager) in [
            ("a", "alpha/b"),
            ("b", "alpha/a"),
            ("c", "alpha/c"),
            ("d", "alpha/missing"),
            ("e", "gamma/chief"),
        ] {
            let mut seat = template.clone();
            seat["agent_id"] = json!(format!("alpha/{id}"));
            seat["manager"] = json!(manager);
            seats.push(seat);
        }
        let text = rendered(value);
        for id in ["a", "b", "c", "d", "e"] {
            assert_eq!(text.matches(&format!("alpha/{id} [")).count(), 1, "{text}");
        }
        assert!(text.contains(" -> alpha/missing\n"));
        assert!(text.contains(" -> gamma/chief\n"));
        assert!(text.contains(" -> alpha/c\n"));
    }

    // Break caught: shared-reader failure hides seats/data or repeats the note per project.
    #[test]
    fn renderer_flattens_unavailable_reporting_once() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/atm_org/unavailable.json"
        ))
        .unwrap();
        let text = rendered(value);
        assert_eq!(
            text,
            include_str!("../../../tests/fixtures/atm_org/unavailable.txt")
        );
        assert_eq!(text.matches("Reporting unavailable:").count(), 1);
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("    ")
                    && line.contains(" [")
                    && !line.trim_start().starts_with('t'))
                .count(),
            7
        );
        assert!(!text.contains(" -> "));
        assert!(text.contains("  Decisions: 0 pending, 1 provisional\n"));
        assert!(text.contains("Unplaced tasks: 1\n"));
    }

    // Break caught: cap totals become listed counts, task order changes, or subtraction wraps.
    #[test]
    fn renderer_preserves_order_and_overflow_counts() {
        let value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/atm_org/tree-five.json"
        ))
        .unwrap();
        let text = rendered(value);
        assert_eq!(
            text,
            include_str!("../../../tests/fixtures/atm_org/tree-five.txt")
        );
        assert_eq!(text.matches("+3 more").count(), 2);
        let mut value = fixture();
        value["needs_owner"]["items_total"] = json!(0);
        value["projects"][0]["tasks_open"] = json!(0);
        value["projects"][0]["tasks"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let text = rendered(value);
        assert!(!text.contains(" more\n"));
        assert!(text.find("    t13 [").unwrap() < text.find("    t11 [").unwrap());
    }

    // Break caught: any unescaped free-form field can spoof rows or change terminal state.
    #[test]
    fn renderer_escapes_every_display_field() {
        let mut value = fixture();
        let control = "\n\t\r\u{001b}\\é🙂\u{007f}\u{0085}";
        value["generated_at"] = json!(format!("generated{control}"));
        value["needs_owner"]["away_since"] = json!(format!("away{control}"));
        value["needs_owner"]["items"][0]["kind"] = json!(format!("need-kind{control}"));
        value["needs_owner"]["items"][0]["id"] = json!(format!("need-id{control}"));
        value["needs_owner"]["items"][0]["since"] = json!(format!("since{control}"));
        value["projects"][0]["root"] = json!(format!("root{control}"));
        value["projects"][0]["seats"][0]["state"] = json!(format!("seat-state{control}"));
        value["projects"][0]["seats"][0]["harness"] = json!(format!("harness{control}"));
        value["projects"][0]["seats"][0]["line_error"] = json!(format!("line-error{control}"));
        value["projects"][0]["seats"][0]["tasks"] = json!([format!("seat-task{control}")]);
        value["projects"][0]["tasks"][0]["task_id"] = json!(format!("task-id{control}"));
        value["projects"][0]["tasks"][0]["title"] = json!(format!("title{control}"));
        value["projects"][0]["tasks"][0]["kind"] = json!(format!("task-kind{control}"));
        value["projects"][0]["tasks"][0]["state"] = json!(format!("task-state{control}"));
        value["unplaced_tasks"][0]["title"] = json!(format!("unplaced{control}"));
        let text = rendered(value.clone());
        for prefix in [
            "generated",
            "away",
            "need-kind",
            "need-id",
            "since",
            "root",
            "seat-state",
            "harness",
            "line-error",
            "seat-task",
            "task-id",
            "title",
            "task-kind",
            "task-state",
            "unplaced",
        ] {
            assert!(
                text.contains(&format!(
                    "{prefix}\\n\\t\\r\\u{{1b}}\\\\é🙂\\u{{7f}}\\u{{85}}"
                )),
                "{prefix}: {text}"
            );
        }
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
        value["reporting"]["state"] = json!("unavailable");
        value["reporting"]["reason"] = json!(format!("reason{control}"));
        let text = rendered(value);
        assert!(text.contains("reason\\n\\t\\r\\u{1b}\\\\é🙂\\u{7f}\\u{85}"));
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    }

    // Break caught: renderer reports success after the consumer closed stdout.
    #[test]
    fn renderer_propagates_write_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let tree = serde_json::from_value(fixture()).unwrap();
        assert!(render_tree(&tree, &mut Broken).is_err());
    }
}
