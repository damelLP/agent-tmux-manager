//! Parity demonstration: same conceptual operation, two vendors, same
//! `LifecycleEvent` output.
//!
//! Reads real wire payloads as the daemon would receive them, runs each
//! through its respective adapter's translation, and prints the
//! resulting `LifecycleEvent` side-by-side.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p atm-pi-adapter --example parity_demo
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use atm_claude_adapter::RawHookEvent;
use atm_core::{AgentType, Model, SessionDomain, SessionId};
use atm_pi_adapter::RawPiEvent;

fn fresh_session() -> SessionDomain {
    SessionDomain::new(
        SessionId::new("demo"),
        AgentType::GeneralPurpose,
        Model::Sonnet4,
    )
}

fn main() {
    println!("=== Parity demo: same op, two vendors ===\n");
    println!("For each case we feed equivalent wire payloads into the two");
    println!("adapters' `to_lifecycle_event()` translators, and also apply the");
    println!("results to a fresh Session to show downstream state convergence.\n");

    // ---- CASE 1: starting a Bash tool call ----
    let claude_json = r#"{
        "session_id": "demo",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "toolu_abc",
        "tool_input": {"command": "ls /tmp"}
    }"#;
    let pi_json = r#"{
        "event": "tool_call",
        "payload": {
            "type": "tool_call",
            "toolName": "Bash",
            "toolCallId": "toolu_abc",
            "input": {"command": "ls /tmp"}
        }
    }"#;
    show_pair("Case 1: Tool call start (Bash)", claude_json, pi_json);

    // ---- CASE 2: session-end with reason ----
    let claude_json = r#"{
        "session_id": "demo",
        "hook_event_name": "SessionEnd",
        "reason": "clear"
    }"#;
    let pi_json = r#"{
        "event": "session_shutdown",
        "payload": {"type": "session_shutdown", "reason": "clear"}
    }"#;
    show_pair("Case 2: Session end with reason", claude_json, pi_json);

    // ---- CASE 3: needs-input (the vendor-asymmetric one) ----
    // Claude: explicit Notification(permission_prompt)
    // Pi:    extension synthesizes via tool_call with needs_user_input flag
    let claude_json = r#"{
        "session_id": "demo",
        "hook_event_name": "Notification",
        "notification_type": "permission_prompt"
    }"#;
    let pi_json = r#"{
        "event": "tool_call",
        "payload": {
            "type": "tool_call",
            "toolName": "Bash",
            "toolCallId": "toolu_xyz",
            "needs_user_input": true
        }
    }"#;
    show_pair(
        "Case 3: Needs-input (different paths, same state)",
        claude_json,
        pi_json,
    );

    // ---- CASE 4: provider/model change (pi-only event) ----
    println!("──────────────────────────────────────────────────────────────────");
    println!("Case 4: Provider/model change (pi-only — Claude has no analog)");
    let pi_json = r#"{
        "event": "model_select",
        "payload": {
            "type": "model_select",
            "provider": "openai-codex",
            "model": "gpt-5.5"
        }
    }"#;
    let pi: RawPiEvent = serde_json::from_str(pi_json).unwrap();
    println!("  pi    : {pi_json}");
    println!("    →   {:?}", pi.to_lifecycle_event());
    println!("  claude: (no equivalent — pi is provider-agnostic by design)");
    println!();
}

fn show_pair(label: &str, claude_json: &str, pi_json: &str) {
    println!("──────────────────────────────────────────────────────────────────");
    println!("{label}");
    let claude: RawHookEvent = serde_json::from_str(claude_json).unwrap();
    let pi: RawPiEvent = serde_json::from_str(pi_json).unwrap();
    let claude_le = claude.to_lifecycle_event().expect("claude translates");
    let pi_le = pi.to_lifecycle_event().expect("pi translates");

    println!("  claude → {claude_le:?}");
    println!("  pi     → {pi_le:?}");
    if claude_le == pi_le {
        println!("  ✓ LifecycleEvent identical");
    } else {
        println!("  ≠ LifecycleEvents differ (vendor-specific fidelity preserved)");
    }

    // Now apply each to a fresh Session and compare downstream state.
    let mut s_claude = fresh_session();
    let mut s_pi = fresh_session();
    s_claude.apply_lifecycle_event(&claude_le);
    s_pi.apply_lifecycle_event(&pi_le);

    let claude_state = format!(
        "{}/{}",
        s_claude.status,
        s_claude
            .current_activity
            .as_ref()
            .map(|a| a.display())
            .as_deref()
            .unwrap_or("—")
    );
    let pi_state = format!(
        "{}/{}",
        s_pi.status,
        s_pi.current_activity
            .as_ref()
            .map(|a| a.display())
            .as_deref()
            .unwrap_or("—")
    );
    println!("  claude session → status/activity = {claude_state}");
    println!("  pi     session → status/activity = {pi_state}");
    if claude_state == pi_state {
        println!("  ✓ downstream session state identical");
    } else {
        println!("  ≠ downstream state differs");
    }
    println!();
}
