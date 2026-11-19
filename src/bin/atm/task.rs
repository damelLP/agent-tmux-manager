//! One-shot task commands. The daemon owns identity, transitions and notifications.
use super::msg::{escaped, output, status_text as word, MessageFormat};
use super::output::stdout;
use anyhow::{Context, Result};
use atm_org::{
    Address, AgentId, DecisionId, ProjectId, TaskBudget, TaskFile, TaskFileKind, TaskId, TaskKind,
    TaskMeasure, TaskState, TaskSubmission, TaskVerdict,
};
use atmd::org::tasks::{
    self, AssignParams, AttachParams, CreateParams, GetParams, ListParams, ListResult,
    MutationResult, Notification, ReviewParams, SetStateParams, SubmitParams, TaskError,
    TaskSummary, VerdictParams,
};
use clap::{Args, Subcommand};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{io::Write, time::Duration};

const MUTATION_BUDGET: Duration = Duration::from_secs(35);
const READ_BUDGET: Duration = Duration::from_secs(15);

#[derive(Debug, Args)]
pub struct TaskArgs {
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: MessageFormat,
    #[command(subcommand)]
    action: TaskAction,
}

#[derive(Debug, Subcommand)]
pub enum TaskAction {
    /// Create an open task; structured flags take the existing method JSON objects
    Create {
        title: String,
        /// Intended registered project; defaults to the named caller's project
        #[arg(long = "project")]
        project_id: Option<ProjectId>,
        /// Task kind: work, review, plan, errand, drift_check
        #[arg(long, default_value = "work", value_parser = parse_word::<TaskKind>)]
        kind: TaskKind,
        #[arg(long, value_parser = parse_task)]
        parent: Option<TaskId>,
        #[arg(long, value_parser = parse_task)]
        judges: Option<TaskId>,
        /// JSON: {"kind":"mechanical","command":"check"}, {"kind":"reviewed","rubric":"file"}, or {"kind":"owner"}
        #[arg(long, value_parser = parse_json::<TaskMeasure>)]
        measure: TaskMeasure,
        /// JSON with optional cost {"microdollars":N}, turns and RFC3339 deadline
        #[arg(long, value_parser = parse_strict_json::<TaskBudget>)]
        budget: Option<TaskBudget>,
        /// Repeatable JSON: {"path":"file","kind":"brief","note":null}
        #[arg(long = "file", value_parser = parse_strict_json::<TaskFile>)]
        files: Vec<TaskFile>,
        #[arg(long)]
        rests_on: Option<DecisionId>,
    },
    /// Assign to a full project/name agent id; queues its brief
    Assign {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        agent_id: AgentId,
    },
    /// Change state (open, working, waiting, blocked or cancelled); daemon checks authority
    SetState {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        /// State: open, working, waiting, blocked, cancelled
        #[arg(value_parser = parse_word::<TaskState>)]
        state: TaskState,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Submit a plan or result; queues a report to the creator
    Submit {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        /// Submission kind: plan, result
        #[arg(long, default_value = "result", value_parser = parse_word::<TaskSubmission>)]
        what: TaskSubmission,
        /// Repeatable JSON: {"path":"file","kind":"report","note":null}
        #[arg(long = "file", value_parser = parse_strict_json::<TaskFile>)]
        files: Vec<TaskFile>,
    },
    /// Attach a literal path; does not read the file
    Attach {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        path: String,
        /// File kind: brief, report, evidence, plan, contract
        #[arg(long, value_parser = parse_word::<TaskFileKind>)]
        kind: TaskFileKind,
        #[arg(long)]
        note: Option<String>,
    },
    /// Start an existing review task on a submitted task
    Review {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        #[arg(value_parser = parse_task)]
        review_task: TaskId,
    },
    /// Give accepted or changes_requested verdict; approval of a plan resumes work
    Verdict {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        #[arg(value_parser = parse_word::<TaskVerdict>)]
        verdict: TaskVerdict,
        #[arg(long, value_parser = parse_task)]
        review_task: Option<TaskId>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Accept a changes-requested task from a reviewed fix when its assignee is down
    CarryBack {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
        #[arg(long = "from-fix", value_parser = parse_task)]
        fix_task: TaskId,
        #[arg(long, value_parser = parse_task)]
        review_task: TaskId,
        #[arg(long)]
        note: String,
    },
    /// List a single page, optionally filtered; agent addresses must be full ids
    List {
        #[arg(long = "project")]
        project_id: Option<ProjectId>,
        #[arg(long = "agent")]
        agent_id: Option<AgentId>,
        #[arg(long)]
        creator: Option<Address>,
        /// State: open, assigned, working, waiting, blocked, submitted, in_review, accepted, changes_requested, cancelled
        #[arg(long, value_parser = parse_word::<TaskState>)]
        state: Option<TaskState>,
        /// Task kind: work, review, plan, errand, drift_check
        #[arg(long, value_parser = parse_word::<TaskKind>)]
        kind: Option<TaskKind>,
        #[arg(long, value_parser = parse_task)]
        parent: Option<TaskId>,
        #[arg(long, value_parser = parse_task)]
        after: Option<TaskId>,
        #[arg(long = "open")]
        open_only: bool,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
    },
    /// Show every task field, including attached files, measure, budget and cost
    Get {
        #[arg(value_parser = parse_task)]
        task_id: TaskId,
    },
}

fn parse_task(text: &str) -> std::result::Result<TaskId, String> {
    let id: TaskId = text.parse().map_err(|e: atm_org::IdError| e.to_string())?;
    tasks::sequence(&id).map_err(|e| e.to_string())?;
    Ok(id)
}
fn parse_json<T: DeserializeOwned>(text: &str) -> std::result::Result<T, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
fn parse_strict_json<T: DeserializeOwned + Serialize>(
    text: &str,
) -> std::result::Result<T, String> {
    let raw: Value = parse_json(text)?;
    let value: T = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
    check_json_keys(
        &raw,
        &serde_json::to_value(&value).map_err(|e| e.to_string())?,
    )?;
    Ok(value)
}
fn check_json_keys(raw: &Value, encoded: &Value) -> std::result::Result<(), String> {
    if let Some(fields) = raw.as_object() {
        for (key, value) in fields {
            let known = encoded
                .get(key)
                .ok_or_else(|| format!("unknown JSON field {key:?}"))?;
            check_json_keys(value, known)?;
        }
    }
    Ok(())
}
fn parse_word<T: DeserializeOwned>(text: &str) -> std::result::Result<T, String> {
    serde_json::from_value(Value::String(text.into())).map_err(|e| e.to_string())
}

fn command(action: TaskAction) -> Result<(&'static str, Value)> {
    Ok(match action {
        TaskAction::Create {
            title,
            project_id,
            kind,
            parent,
            judges,
            measure,
            budget,
            files,
            rests_on,
        } => (
            "task.create",
            serde_json::to_value(CreateParams {
                title,
                project_id,
                kind,
                parent,
                judges,
                measure,
                budget,
                files,
                rests_on,
            })?,
        ),
        TaskAction::Assign { task_id, agent_id } => (
            "task.assign",
            serde_json::to_value(AssignParams { task_id, agent_id })?,
        ),
        TaskAction::SetState {
            task_id,
            state,
            reason,
        } => (
            "task.set_state",
            serde_json::to_value(SetStateParams {
                task_id,
                state,
                reason,
            })?,
        ),
        TaskAction::Submit {
            task_id,
            what,
            files,
        } => (
            "task.submit",
            serde_json::to_value(SubmitParams {
                task_id,
                what,
                files,
            })?,
        ),
        TaskAction::Attach {
            task_id,
            path,
            kind,
            note,
        } => (
            "task.attach",
            serde_json::to_value(AttachParams {
                task_id,
                file: TaskFile { path, kind, note },
            })?,
        ),
        TaskAction::Review {
            task_id,
            review_task,
        } => (
            "task.review",
            serde_json::to_value(ReviewParams {
                task_id,
                review_task,
            })?,
        ),
        TaskAction::Verdict {
            task_id,
            verdict,
            review_task,
            note,
        } => (
            "task.verdict",
            serde_json::to_value(VerdictParams {
                task_id,
                verdict,
                review_task,
                note,
            })?,
        ),
        TaskAction::CarryBack {
            task_id,
            fix_task,
            review_task,
            note,
        } => (
            "task.carry_back",
            serde_json::to_value(tasks::CarryBackParams {
                task_id,
                fix_task,
                review_task,
                note,
            })?,
        ),
        TaskAction::List {
            project_id,
            agent_id,
            creator,
            state,
            kind,
            parent,
            after,
            open_only,
            limit,
        } => (
            "task.list",
            serde_json::to_value(ListParams {
                project_id,
                agent_id,
                creator,
                state,
                kind,
                parent,
                after,
                open_only,
                limit,
            })?,
        ),
        TaskAction::Get { task_id } => ("task.get", serde_json::to_value(GetParams { task_id })?),
    })
}

fn decode<T: DeserializeOwned>(raw: &Value) -> Result<T> {
    serde_json::from_value(raw.clone()).context("invalid task result from daemon")
}
fn render_summary(task: &TaskSummary, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "{} {} kind={} agent={}",
        escaped(&task.task_id.to_string(), false),
        word(task.state)?,
        word(task.kind)?,
        task.agent_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".into())
    )?;
    writeln!(out, "  title: {}", escaped(&task.title, false))?;
    if let Some(reason) = &task.reason {
        writeln!(out, "  reason: {}", escaped(reason, false))?;
    }
    Ok(())
}
fn render(method: &str, raw: &Value, out: &mut impl Write) -> Result<()> {
    match method {
        "task.list" => {
            let result: ListResult = decode(raw)?;
            if result.tasks.is_empty() {
                writeln!(out, "No matching tasks.")?;
            }
            for task in &result.tasks {
                render_summary(task, out)?;
            }
            if let Some(after) = result.next_after {
                writeln!(out, "next: --after {}", escaped(&after.to_string(), false))?;
            }
        }
        "task.get" => {
            let value = raw
                .get("task")
                .context("invalid task result from daemon: missing task")?;
            let task = value
                .as_object()
                .context("invalid task result from daemon: missing task object")?;
            // Task/GetResult are serialize-only. Reuse the shared summary subset,
            // then show all remaining raw fields rather than copying the Task DTO.
            render_summary(&decode(value)?, out)?;
            let details = task.iter().filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "task_id" | "state" | "kind" | "agent_id" | "title" | "reason"
                )
            });
            let extra = raw
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, _)| key.as_str() != "task");
            for (key, value) in details.chain(extra) {
                write!(out, "  {}: ", escaped(key, false))?;
                output(value, MessageFormat::Json, "", out)?;
            }
        }
        _ => {
            // Notification vocabularies can grow independently of the committed task.
            let mut fields = raw.clone();
            if let Some(fields) = fields.as_object_mut() {
                fields.remove("notification");
            }
            let result: MutationResult = decode(&fields)?;
            writeln!(
                out,
                "{} {}",
                escaped(&result.task_id.to_string(), false),
                if result.changed {
                    "changed"
                } else {
                    "unchanged"
                }
            )?;
            if let Some(notification) = raw.get("notification").filter(|value| !value.is_null()) {
                let status = notification
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                match status {
                    "queued" | "unconfirmed" => match decode(notification)? {
                        Notification::Queued { message_id } => writeln!(
                            out,
                            "notification: queued {message_id} (not delivery confirmation)"
                        )?,
                        Notification::Unconfirmed { error } => writeln!(
                            out,
                            "notification: unconfirmed {}: {}",
                            escaped(&error.code, false),
                            escaped(&error.message, false)
                        )?,
                    },
                    _ => writeln!(
                        out,
                        "notification: {} (task committed; status not understood by this atm)",
                        escaped(status, false)
                    )?,
                }
            }
        }
    }
    Ok(())
}

fn read_method(method: &str) -> bool {
    matches!(method, "task.list" | "task.get")
}
fn request_budget(method: &str) -> Duration {
    if read_method(method) {
        READ_BUDGET
    } else {
        MUTATION_BUDGET
    }
}
fn request_error(method: &str, error: super::rpc::RpcError) -> anyhow::Error {
    use super::rpc::RpcError;
    let read = read_method(method);
    if read
        && (matches!(&error, RpcError::Timeout)
            || matches!(&error, RpcError::Daemon(reply) if reply.code == atm_protocol::error_code::TIMEOUT))
    {
        return anyhow::anyhow!(
            "task read request timed out; retry atm task get <id> or atm task list"
        );
    }
    match error {
        RpcError::Timeout => anyhow::Error::new(TaskError::Timeout),
        other => {
            let diagnostic = escaped(&other.to_string(), false);
            if !read && matches!(other, RpcError::Connection(_)) {
                anyhow::anyhow!("{diagnostic}; task may have committed; inspect atm task list and atm task get <id> before retrying")
            } else {
                anyhow::anyhow!("{diagnostic}")
            }
        }
    }
}

pub async fn cmd_task(args: TaskArgs) -> Result<()> {
    let (method, params) = command(args.action)?;
    let raw = super::rpc::call(method, params, request_budget(method))
        .await
        .map_err(|error| request_error(method, error))?;
    stdout(|out| match args.format {
        // JSON is the raw daemon answer, independent of this build's text schema.
        MessageFormat::Json => output(&raw, args.format, "", out),
        MessageFormat::Text => {
            let mut text = Vec::new();
            render(method, &raw, &mut text).map_err(|error| {
                let diagnostic = escaped(&format!("{error:#}"), false);
                if read_method(method) {
                    anyhow::anyhow!("{diagnostic}")
                } else {
                    anyhow::anyhow!("{diagnostic}; task may have committed; inspect atm task list and atm task get <id> before retrying")
                }
            })?;
            Ok(out.write_all(&text)?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Break caught: swapping read/mutation budgets reports accepted slow work as timed out.
    #[test]
    fn each_task_method_gets_its_router_budget_plus_margin() {
        for (method, seconds) in [
            ("task.create", 35),
            ("task.assign", 35),
            ("task.set_state", 35),
            ("task.submit", 35),
            ("task.attach", 35),
            ("task.review", 35),
            ("task.verdict", 35),
            ("task.list", 15),
            ("task.get", 15),
        ] {
            assert_eq!(
                request_budget(method),
                Duration::from_secs(seconds),
                "{method}"
            );
        }
    }

    // Break caught: removing the Timeout arm loses the task outcome-unknown warning.
    #[test]
    fn task_timeout_keeps_outcome_unknown_advice() {
        assert_eq!(request_error("task.create", super::super::rpc::RpcError::Timeout).to_string(),
            "task request timed out; accepted work may complete; inspect tasks and inbox before retrying");
    }

    // Break caught: the client-side read deadline is described as an uncertain mutation.
    #[test]
    fn read_timeout_has_read_only_advice() {
        for method in ["task.get", "task.list"] {
            let text = request_error(method, super::super::rpc::RpcError::Timeout).to_string();
            assert!(text.contains("task read request timed out"), "{text}");
            assert!(
                !text.contains("accepted work") && !text.contains("committed"),
                "{text}"
            );
        }
    }
}
