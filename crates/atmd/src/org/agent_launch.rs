//! Pure agent launch text: stage 0 deliberately ignores CLI `[harness.<name>]`
//! configuration. Adapter policy supplies the model flag, unlike legacy spawn's
//! definition flag (notably Codex's `-m`). Only printable ASCII may be staged.
//! A safe bare binary keeps shell alias expansion; a command prefix puts that
//! binary outside command position, so alias-only installs cannot use a prefix.

// Not called until slice B's Task 6 wires up the launch flow.
#![cfg_attr(not(test), allow(dead_code))]

use super::{agent_flow::launch_text_is_safe, agent_project::PreparedAgent, agents::AgentError};
use crate::control::{build_spawn_command, launch_policy, shell_quote, spawn_env_override};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LaunchCommand {
    pub text: String,
    pub binary: String,
}

pub(super) fn launch_command(
    agent: &PreparedAgent,
    env: impl Fn(&str) -> Option<String>,
) -> Result<LaunchCommand, AgentError> {
    let config = &agent.role.config;
    let unsupported =
        || AgentError::Refused(format!("no agent launch policy for {}", config.harness));
    let definition =
        atm_core::find_harness_definition(config.harness.short_tag()).ok_or_else(unsupported)?;
    let policy = launch_policy(config.harness).ok_or_else(unsupported)?;
    let cwd = agent
        .cwd
        .to_str()
        .ok_or_else(|| AgentError::Refused("launch cwd is not UTF-8".into()))?;
    let bin_override = spawn_env_override(definition.id, "BIN", &env);
    let args_override = spawn_env_override(definition.id, "ARGS", &env);
    let defaults = definition
        .default_args
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>();
    let built = build_spawn_command(
        definition.binary,
        &defaults,
        Some(policy.model_flag),
        None,
        config.model.as_deref(),
        bin_override.as_deref(),
        args_override.as_deref(),
        &config.spawn_args,
        policy.required_args,
    );
    let mut text = format!(
        "cd {} && ATM_AGENT={} ",
        shell_quote(cwd),
        shell_quote(&agent.agent_id.to_string())
    );
    for word in &config.command_prefix {
        text.push_str(&shell_quote(word));
        text.push(' ');
    }
    text.push_str(&built);
    if !launch_text_is_safe(&text) {
        return Err(AgentError::Refused(
            "launch text must be printable ASCII".into(),
        ));
    }
    Ok(LaunchCommand {
        text,
        binary: bin_override.unwrap_or_else(|| definition.binary.to_owned()),
    })
}
