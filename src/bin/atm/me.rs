//! One-call entry point for a named agent, independent of its model or harness.

use super::msg::{escaped, output, MessageFormat};
use anyhow::{Context, Result};
use atmd::org::agent_context::AgentContext;
use clap::Args;
use std::{
    io::{self, Write},
    time::Duration,
};

#[derive(Debug, Args)]
pub struct MeArgs {
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

fn render_context(context: &AgentContext, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "Agent: {}; manager: {}",
        context.agent_id, context.manager
    )?;
    let role = escaped(&context.role_file.to_string_lossy(), false);
    match context.instructions.as_deref() {
        None => writeln!(
            out,
            "Instructions: none (no role file at {role}); reports go to {}",
            context.manager
        )?,
        Some("") => writeln!(out, "Instructions ({role}): none")?,
        Some(body) => {
            writeln!(out, "Instructions ({role}):")?;
            for line in escaped(body, true).split_terminator('\n') {
                writeln!(out, "  {line}")?;
            }
        }
    }
    writeln!(
        out,
        "Memory: {} ({})",
        escaped(&context.memory.path.to_string_lossy(), false),
        if context.memory.exists {
            "exists; read for handover"
        } else {
            "missing; create it to preserve handover"
        }
    )?;
    if context.open_tasks.is_empty() {
        writeln!(out, "Open tasks: none")?;
    } else {
        writeln!(out, "Open tasks:")?;
        for task in &context.open_tasks {
            writeln!(out, "  {}", escaped(&serde_json::to_string(task)?, false))?;
        }
    }
    if context.unread_messages.is_empty() {
        writeln!(out, "Unread: none")?;
    }
    for message in &context.unread_messages {
        let kind = serde_json::to_value(message.kind)?;
        writeln!(
            out,
            "Unread {} {} from={}: {}; read: atm inbox --id {}",
            message.message_id,
            kind.as_str().unwrap_or("unknown"),
            message.from,
            escaped(&message.first_line, false),
            message.message_id
        )?;
    }
    writeln!(out, "Report: {}", escaped(&context.report_command, false))?;
    Ok(())
}

pub async fn cmd_me(args: MeArgs) -> Result<()> {
    // 12 s router + 5 s startup/transport margin. Exactly one capability-checked
    // request; no name lookup, local identity inference or direct tmux fallback.
    let raw = super::rpc::call(
        "agent.context",
        serde_json::json!({}),
        Duration::from_secs(17),
    )
    .await?;
    let context: AgentContext =
        serde_json::from_value(raw.clone()).context("invalid agent context from daemon")?;
    let mut out = io::stdout().lock();
    match args.format {
        MessageFormat::Text => render_context(&context, &mut out),
        MessageFormat::Json => output(&raw, args.format, "", &mut out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> AgentContext {
        serde_json::from_str(include_str!("../../../tests/fixtures/atm_me/context.json")).unwrap()
    }
    fn rendered(value: &AgentContext) -> String {
        let mut bytes = Vec::new();
        render_context(value, &mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    // Break caught: a null role hides other context; an empty body implies no role file.
    #[test]
    fn me_renders_empty_body_missing_role_and_empty_lists_distinctly() {
        let mut value = context();
        value.instructions = Some(String::new());
        value.unread_messages.clear();
        value.memory.exists = true;
        let text = rendered(&value);
        assert!(text.contains("Instructions (/project/.atm/roles/worker.md): none\n"));
        assert!(
            text.contains("Memory: /project/.atm/memory/worker.md (exists; read for handover)\n")
        );
        assert!(text.contains("Unread: none\n"));
        value.instructions = None;
        value.manager = atm_org::Address::Owner;
        assert!(rendered(&value).contains("Instructions: none (no role file at /project/.atm/roles/worker.md); reports go to owner\n"));
        // A newer daemon's task summaries are not silently presented as none.
        value
            .open_tasks
            .push(serde_json::json!({"task_id":"t7","title":"Work","state":"assigned"}));
        assert!(rendered(&value).contains("\"task_id\":\"t7\""));
    }

    // Break caught: role/inbox/path controls change terminal state or spoof extra lines.
    #[test]
    fn me_renderer_preserves_unicode_and_escapes_controls() {
        let mut value = context();
        value.instructions = Some("é🙂\tcode\\path\n\u{1b}[31m\r\u{7f}\u{85}".into());
        value.unread_messages.first_mut().unwrap().first_line = "line\nspoof\u{1b}".into();
        value.memory.path = "/project/\u{1b}memory".into();
        value.role_file = "/project/\u{1b}role.md".into();
        let text = rendered(&value);
        assert!(text.contains("Instructions (/project/\\u{1b}role.md):\n"));
        assert!(text.contains("  é🙂\tcode\\path\n"));
        assert!(text.contains("line\\nspoof\\u{1b}; read: atm inbox --id m7"));
        assert!(!text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t')));
    }

    #[test]
    fn me_renderer_propagates_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(render_context(&context(), &mut Broken).is_err());
    }
}
