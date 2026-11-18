//! Test-only command encoding: the actual RegistryCommand remains the dispatcher
//! input. Non-serde core inputs are encoded here, never given a public wire type.

use atm_core::{
    BackgroundActivity, ChildAgent, ChildAlias, ChildRef, LifecycleContext, StatusLineData,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::registry::RegistryCommand;

fn get<T: DeserializeOwned>(v: &Value, key: &str) -> T {
    serde_json::from_value(
        v.get(key)
            .unwrap_or_else(|| panic!("missing {key}"))
            .clone(),
    )
    .unwrap()
}

pub(super) fn reference(r: &ChildRef) -> Value {
    match r {
        ChildRef::Id(id) => json!({"id": id}),
        ChildRef::Name(name) => json!({"name": name}),
    }
}

fn parse_reference(v: &Value) -> ChildRef {
    if v.get("id").is_some() {
        ChildRef::Id(get(v, "id"))
    } else {
        ChildRef::Name(get(v, "name"))
    }
}

fn context(c: &LifecycleContext) -> Value {
    json!({
        "child": c.child.as_ref().map(|child| json!({"reference": reference(&child.reference), "agent_type": child.agent_type})),
        "alias": c.child_alias.as_ref().map(|alias| json!({"id": alias.id, "name": alias.name})),
        "background": c.background_activity.map(|bg| json!({"running": bg.running, "scheduled": bg.scheduled})),
    })
}

fn parse_context(v: &Value) -> LifecycleContext {
    LifecycleContext {
        child: v.get("child").filter(|v| !v.is_null()).map(|c| ChildAgent {
            reference: parse_reference(&c["reference"]),
            agent_type: get(c, "agent_type"),
        }),
        child_alias: v.get("alias").filter(|v| !v.is_null()).map(|a| ChildAlias {
            id: get(a, "id"),
            name: get(a, "name"),
        }),
        background_activity: v.get("background").filter(|v| !v.is_null()).map(|b| {
            BackgroundActivity {
                running: get(b, "running"),
                scheduled: get(b, "scheduled"),
            }
        }),
    }
}

// One field list for both directions; no test-only copy of StatusLineData.
macro_rules! status_codec {
    ($($field:ident),* $(,)?) => {
        fn status(data: &StatusLineData) -> Value { json!({$(stringify!($field): data.$field),*}) }
        fn parse_status(data: &Value) -> StatusLineData { StatusLineData { $($field: get(data, stringify!($field))),* } }
    };
}
status_codec!(
    session_id,
    model_id,
    model_display_name,
    cost_usd,
    total_duration_ms,
    api_duration_ms,
    lines_added,
    lines_removed,
    total_input_tokens,
    total_output_tokens,
    context_window_size,
    current_input_tokens,
    current_output_tokens,
    cache_creation_tokens,
    cache_read_tokens,
    cwd,
    version
);

pub(super) fn encode(cmd: &RegistryCommand) -> Value {
    match cmd {
        RegistryCommand::Register { session, .. } => json!({"kind":"register", "session":session}),
        RegistryCommand::UpdateFromStatusLine {
            session_id,
            pid,
            data,
            harness,
            ..
        } => {
            json!({"kind":"status", "session_id":session_id, "pid":pid, "data":status(data), "harness":harness})
        }
        RegistryCommand::ApplyLifecycleEvent {
            session_id,
            event,
            harness,
            pid,
            tmux_pane,
            context: c,
            ..
        } => {
            json!({"kind":"lifecycle", "session_id":session_id, "event":event, "harness":harness, "pid":pid, "tmux_pane":tmux_pane, "context":context(c)})
        }
        RegistryCommand::RegisterDiscovered {
            session_id,
            pid,
            cwd,
            tmux_pane,
            harness,
            ..
        } => {
            json!({"kind":"discovered", "session_id":session_id, "pid":pid, "cwd":cwd, "tmux_pane":tmux_pane, "harness":harness})
        }
        RegistryCommand::Remove { session_id, .. } => {
            json!({"kind":"remove", "session_id":session_id})
        }
        RegistryCommand::GetSession { session_id, .. } => {
            json!({"kind":"get", "session_id":session_id})
        }
        RegistryCommand::GetAllSessions { .. } => json!({"kind":"all"}),
        RegistryCommand::PaneSessions { pane, .. } => json!({"kind":"pane", "pane":pane}),
        RegistryCommand::CleanupStale => json!({"kind":"cleanup"}),
        RegistryCommand::RefreshGitInfo => json!({"kind":"refresh"}),
    }
}

pub(super) fn decode(v: &Value) -> RegistryCommand {
    match v["kind"].as_str().unwrap() {
        "register" => RegistryCommand::Register {
            session: Box::new(get(v, "session")),
            respond_to: oneshot::channel().0,
        },
        "status" => RegistryCommand::UpdateFromStatusLine {
            session_id: get(v, "session_id"),
            pid: get(v, "pid"),
            data: parse_status(&v["data"]),
            harness: get(v, "harness"),
            respond_to: oneshot::channel().0,
        },
        "lifecycle" => RegistryCommand::ApplyLifecycleEvent {
            session_id: get(v, "session_id"),
            event: get(v, "event"),
            harness: get(v, "harness"),
            pid: get(v, "pid"),
            tmux_pane: get(v, "tmux_pane"),
            context: parse_context(&v["context"]),
            respond_to: oneshot::channel().0,
        },
        "discovered" => RegistryCommand::RegisterDiscovered {
            session_id: get(v, "session_id"),
            pid: get(v, "pid"),
            cwd: get(v, "cwd"),
            tmux_pane: get(v, "tmux_pane"),
            harness: get(v, "harness"),
            respond_to: oneshot::channel().0,
        },
        "remove" => RegistryCommand::Remove {
            session_id: get(v, "session_id"),
            respond_to: oneshot::channel().0,
        },
        "get" => RegistryCommand::GetSession {
            session_id: get(v, "session_id"),
            respond_to: oneshot::channel().0,
        },
        "all" => RegistryCommand::GetAllSessions {
            respond_to: oneshot::channel().0,
        },
        "pane" => RegistryCommand::PaneSessions {
            pane: get(v, "pane"),
            respond_to: oneshot::channel().0,
        },
        "cleanup" => RegistryCommand::CleanupStale,
        "refresh" => RegistryCommand::RefreshGitInfo,
        other => panic!("unknown tape command {other}"),
    }
}
