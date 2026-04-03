# Phase 1: Data Model + SubagentStart/Stop Wiring — Implementation Plan

**Beads:** agent-tmux-monitor-47f
**Design:** docs/plans/2026-04-03-atm-v2-workspace-manager-design.md
**Prerequisite:** None (this is the unblocked root)

---

## Task 1: Add project/worktree fields to SessionDomain

### What
Add 3 new `Option<String>` fields to `SessionDomain` for project/worktree grouping.

### Files to modify

**`crates/atm-core/src/session.rs`**

After `tmux_pane` field (line 573), add:

```rust
/// Git project root (resolved from working_directory).
/// Shared across all worktrees of the same repo.
#[serde(skip_serializing_if = "Option::is_none")]
pub project_root: Option<String>,

/// Git worktree path (specific checkout directory).
/// For the main checkout, this equals project_root.
#[serde(skip_serializing_if = "Option::is_none")]
pub worktree_path: Option<String>,

/// Git branch name for this worktree.
#[serde(skip_serializing_if = "Option::is_none")]
pub worktree_branch: Option<String>,
```

In `SessionDomain::new()` (line 578), add after `tmux_pane: None`:

```rust
project_root: None,
worktree_path: None,
worktree_branch: None,
```

### Verification
`cargo build -p atm-core` compiles. All existing tests pass unchanged (fields are `Option`, `new()` sets them to `None`).

---

## Task 2: Add parent/child relationship fields to SessionDomain

### What
Add fields to track subagent parent-child links.

### Files to modify

**`crates/atm-core/src/session.rs`**

After `worktree_branch` (added in Task 1), add:

```rust
/// Parent session ID (set when this session is a subagent).
#[serde(skip_serializing_if = "Option::is_none")]
pub parent_session_id: Option<SessionId>,

/// Child subagent session IDs spawned by this session.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub child_session_ids: Vec<SessionId>,
```

In `SessionDomain::new()`, add:

```rust
parent_session_id: None,
child_session_ids: Vec::new(),
```

### Verification
`cargo build -p atm-core` compiles. Existing tests still pass.

---

## Task 3: Add corresponding fields to SessionView

### What
Surface the new domain fields through the view DTO that crosses the wire.

### Files to modify

**`crates/atm-core/src/session.rs`** — `SessionView` struct (line 985)

Add after `tmux_pane` field (line 1059):

```rust
/// Git project root (for grouping in tree view)
#[serde(skip_serializing_if = "Option::is_none")]
pub project_root: Option<String>,

/// Git worktree path
#[serde(skip_serializing_if = "Option::is_none")]
pub worktree_path: Option<String>,

/// Git branch name for this worktree
#[serde(skip_serializing_if = "Option::is_none")]
pub worktree_branch: Option<String>,

/// Parent session ID (if this is a subagent)
#[serde(skip_serializing_if = "Option::is_none")]
pub parent_session_id: Option<SessionId>,

/// Child subagent session IDs
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub child_session_ids: Vec<SessionId>,
```

In `SessionView::from_domain()` (line 1064), add after `tmux_pane`:

```rust
project_root: session.project_root.clone(),
worktree_path: session.worktree_path.clone(),
worktree_branch: session.worktree_branch.clone(),
parent_session_id: session.parent_session_id.clone(),
child_session_ids: session.child_session_ids.clone(),
```

### Fix test helpers that construct SessionView as struct literals

These 4 helpers will fail to compile. Add `..Default::default()` rest syntax to each:

1. **`crates/atm/src/app.rs:335`** — `create_test_session()` — add `..Default::default()` at end of struct literal
2. **`crates/atm/src/client.rs:628`** — `create_test_session()` — same
3. **`crates/atm/src/ui/mod.rs:88`** — `create_test_session()` — same
4. **`crates/atm/src/ui/session_list.rs:285`** — `test_session()` — same

### Verification
`cargo build --workspace` compiles. `cargo test --workspace` passes. The new fields default to `None`/empty, so all existing behavior is unchanged.

---

## Task 4: Forward subagent fields through ApplyHookEvent pipeline

### What
Thread `agent_id`, `agent_type`, and `agent_transcript_path` from `RawHookEvent` all the way to the registry actor, where they are currently dropped at `connection.rs:419`.

### Files to modify (in order)

**4a. `crates/atmd/src/registry/commands.rs:73`** — Add 3 fields to `RegistryCommand::ApplyHookEvent`:

```rust
ApplyHookEvent {
    session_id: SessionId,
    event_type: HookEventType,
    tool_name: Option<String>,
    notification_type: Option<String>,
    pid: Option<u32>,
    tmux_pane: Option<String>,
    // NEW: subagent fields
    agent_id: Option<String>,
    agent_type: Option<String>,
    agent_transcript_path: Option<String>,
    respond_to: oneshot::Sender<Result<(), RegistryError>>,
},
```

**4b. `crates/atmd/src/registry/handle.rs:139`** — Add 3 params to `apply_hook_event()` signature:

```rust
pub async fn apply_hook_event(
    &self,
    session_id: SessionId,
    event_type: HookEventType,
    tool_name: Option<String>,
    notification_type: Option<String>,
    pid: Option<u32>,
    tmux_pane: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    agent_transcript_path: Option<String>,
) -> Result<(), RegistryError>
```

And forward them into the `RegistryCommand::ApplyHookEvent` construction at line 151.

**4c. `crates/atmd/src/server/connection.rs:419`** — Forward the 3 new fields from `raw_event`:

```rust
self.registry
    .apply_hook_event(
        raw_event.session_id(),
        event_type,
        raw_event.tool_name,
        raw_event.notification_type,
        raw_event.pid,
        raw_event.tmux_pane,
        raw_event.agent_id,          // NEW
        raw_event.agent_type,        // NEW
        raw_event.agent_transcript_path, // NEW
    )
    .await
```

**4d. `crates/atmd/src/registry/actor.rs:129`** — Destructure 3 new fields in command dispatch:

```rust
RegistryCommand::ApplyHookEvent {
    session_id,
    event_type,
    tool_name,
    notification_type,
    pid,
    tmux_pane,
    agent_id,
    agent_type,
    agent_transcript_path,
    respond_to,
} => {
    let result = self.handle_apply_hook_event(
        session_id,
        event_type,
        tool_name,
        notification_type,
        pid,
        tmux_pane,
        agent_id,
        agent_type,
        agent_transcript_path,
    );
```

**4e. `crates/atmd/src/registry/actor.rs:566`** — Add 3 params to `handle_apply_hook_event()`:

```rust
fn handle_apply_hook_event(
    &mut self,
    session_id: SessionId,
    event_type: HookEventType,
    tool_name: Option<String>,
    notification_type: Option<String>,
    pid: Option<u32>,
    tmux_pane: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    agent_transcript_path: Option<String>,
) -> Result<(), RegistryError>
```

For now, just accept the params without using them yet (Task 5 will use them). This ensures the pipeline compiles end-to-end.

### Fix tests

Update all test sites that construct `ApplyHookEvent` or call `apply_hook_event()`:

- **`crates/atmd/src/registry/handle.rs:447`** — add `agent_id: None, agent_type: None, agent_transcript_path: None` to command assertions
- **`crates/atmd/src/registry/actor.rs:1056, 1100, 1133`** — add 3 `None` params to `handle_apply_hook_event` calls
- **`crates/atmd/tests/registry_integration.rs`** — update all `apply_hook_event()` calls to include 3 new `None` args (search for `.apply_hook_event(`)

### Verification
`cargo build --workspace` compiles. `cargo test --workspace` passes. No behavioral change — the new fields flow through but aren't consumed yet.

---

## Task 5: Wire SubagentStart handling in the registry actor

### What
When a `SubagentStart` hook event arrives at the actor, use the `agent_id` and `agent_type` fields to update the parent session's `child_session_ids` and prepare for the child session.

### Files to modify

**`crates/atmd/src/registry/actor.rs`** — Inside `handle_apply_hook_event`, add logic **before** the call to `session.apply_hook_event()` (around line 667):

```rust
// Handle SubagentStart: record pending child correlation
if event_type == HookEventType::SubagentStart {
    if let Some(ref agent_id) = agent_id {
        let child_agent_type = agent_type
            .as_deref()
            .map(AgentType::from_subagent_type)
            .unwrap_or_default();
        self.pending_subagents.insert(
            agent_id.clone(),
            PendingSubagent {
                parent_session_id: session.id.clone(),
                agent_type: child_agent_type,
                transcript_path: agent_transcript_path.clone(),
                created_at: Instant::now(),
            },
        );
    }
}

// Handle SubagentStop: remove pending correlation
if event_type == HookEventType::SubagentStop {
    if let Some(ref agent_id) = agent_id {
        self.pending_subagents.remove(agent_id);
    }
}
```

**Add `PendingSubagent` struct and storage to `RegistryActor`:**

```rust
use std::time::Instant;

struct PendingSubagent {
    parent_session_id: SessionId,
    agent_type: AgentType,
    transcript_path: Option<String>,
    created_at: Instant,
}

// In RegistryActor struct:
pending_subagents: HashMap<String, PendingSubagent>, // keyed by agent_id
```

**In `RegistryActor::new()`**, initialize: `pending_subagents: HashMap::new()`.

**`crates/atm-core/src/session.rs:712`** — Update the SubagentStart/Stop stub to set meaningful activity:

```rust
HookEventType::SubagentStart => {
    self.status = SessionStatus::Working;
    self.current_activity = Some(ActivityDetail::with_context("Spawning subagent"));
}
HookEventType::SubagentStop => {
    self.status = SessionStatus::Working;
    self.current_activity = Some(ActivityDetail::thinking());
}
```

### Verification
`cargo test --workspace` passes. Write new test: send a `SubagentStart` with `agent_id = "sub-123"` and `agent_type = "explore"`, verify the parent session status is `Working` and `pending_subagents` contains the entry.

---

## Task 6: Correlate pending subagents with discovered sessions

### What
When a new session appears (via discovery or hook), check if it matches a pending subagent entry. If so, link parent and child.

### Files to modify

**`crates/atmd/src/registry/actor.rs`** — Add a correlation method:

```rust
/// Attempts to correlate a newly registered session with a pending subagent.
/// Called after a session is registered or its session_id is upgraded.
fn try_correlate_subagent(&mut self, session_id: &SessionId, pid: u32) {
    // Strategy: check if any pending subagent's transcript path matches,
    // or if this session's PID is a child of a known parent PID.
    // For now, use transcript path matching as primary strategy.

    let mut matched_agent_id = None;
    for (agent_id, pending) in &self.pending_subagents {
        // TODO: implement matching strategy
        // Option 1: transcript path match
        // Option 2: PID ancestry check via get_parent_pid
        // For MVP: match by checking if session appeared shortly after SubagentStart
        if pending.created_at.elapsed() < Duration::from_secs(30) {
            // Heuristic: if this session's PID is a child of the parent's PID
            if let Some(parent_pid) = self.session_id_to_pid.get(&pending.parent_session_id) {
                if is_descendant_of(pid, *parent_pid) {
                    matched_agent_id = Some(agent_id.clone());
                    break;
                }
            }
        }
    }

    if let Some(agent_id) = matched_agent_id {
        if let Some(pending) = self.pending_subagents.remove(&agent_id) {
            // Link child to parent
            if let Some((child_session, _)) = self.sessions_by_pid.get_mut(&pid) {
                child_session.parent_session_id = Some(pending.parent_session_id.clone());
                child_session.agent_type = pending.agent_type;
            }
            // Link parent to child
            if let Some(parent_pid) = self.session_id_to_pid.get(&pending.parent_session_id) {
                if let Some((parent_session, _)) = self.sessions_by_pid.get_mut(parent_pid) {
                    parent_session.child_session_ids.push(session_id.clone());
                }
            }
        }
    }
}
```

**Add `is_descendant_of` helper** (can live in `actor.rs` or call through to `tmux.rs:get_parent_pid`):

```rust
/// Check if `pid` is a descendant of `ancestor_pid` by walking /proc.
fn is_descendant_of(pid: u32, ancestor_pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..20 {  // max depth to prevent loops
        if current == ancestor_pid {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match crate::tmux::get_parent_pid(current) {
            Some(ppid) => current = ppid,
            None => return false,
        }
    }
    false
}
```

**Call `try_correlate_subagent`** from:
- `handle_register_discovered()` (line ~380, after session is stored)
- `handle_apply_hook_event()` path C (line ~627, after auto-created session is stored)

**Note:** `get_parent_pid` in `tmux.rs` is currently `fn get_parent_pid(pid: u32) -> Option<u32>` — it's not `pub`. Make it `pub(crate)` so `actor.rs` can call it.

### Verification
Write integration test: register a parent session, send `SubagentStart` with `agent_id`, then register a child session whose PID is a descendant. Verify `parent_session_id` and `child_session_ids` are linked.

---

## Task 7: Add TTL cleanup for pending subagents

### What
Pending subagent entries that don't get matched within 30 seconds should be cleaned up to avoid memory leaks.

### Files to modify

**`crates/atmd/src/registry/actor.rs`** — Add cleanup to the existing `handle_cleanup_stale()` method (or wherever periodic cleanup runs):

```rust
// Clean up expired pending subagent correlations
self.pending_subagents.retain(|_agent_id, pending| {
    pending.created_at.elapsed() < Duration::from_secs(30)
});
```

### Verification
Write test: insert a pending subagent, advance past TTL (or just call cleanup), verify it's removed.

---

## Task 8: Add project/worktree resolution

### What
When a session has a `working_directory`, resolve `project_root`, `worktree_path`, and `worktree_branch` by walking the filesystem.

### Files to modify

**`crates/atm-core/src/session.rs`** (or new file `crates/atm-core/src/project.rs`) — Add resolution functions:

```rust
use std::path::Path;

/// Resolves the git project root from a working directory.
/// Walks up the directory tree looking for `.git`.
pub fn resolve_project_root(working_dir: &str) -> Option<String> {
    let mut path = Path::new(working_dir);
    loop {
        if path.join(".git").exists() {
            return Some(path.to_string_lossy().to_string());
        }
        // For worktrees, .git is a file (not dir) containing "gitdir: ..."
        let git_path = path.join(".git");
        if git_path.is_file() {
            // Read the gitdir pointer to find the main repo
            if let Ok(content) = std::fs::read_to_string(&git_path) {
                if content.starts_with("gitdir:") {
                    // This is a worktree — resolve the main repo root
                    // gitdir: /path/to/main/.git/worktrees/<name>
                    let gitdir = content.trim_start_matches("gitdir:").trim();
                    if let Some(main_git) = Path::new(gitdir)
                        .ancestors()
                        .find(|p| p.file_name().map_or(false, |n| n == ".git"))
                    {
                        if let Some(parent) = main_git.parent() {
                            return Some(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
            return Some(path.to_string_lossy().to_string());
        }
        path = path.parent()?;
    }
}

/// Resolves worktree information from a working directory.
/// Returns (worktree_path, branch_name).
pub fn resolve_worktree_info(working_dir: &str) -> (Option<String>, Option<String>) {
    let path = Path::new(working_dir);

    // Check if .git is a file (worktree) or directory (main checkout)
    let git_path = path.join(".git");
    let worktree_path = Some(working_dir.to_string());

    // Try to read HEAD for branch name
    let branch = resolve_branch_name(path);

    (worktree_path, branch)
}

fn resolve_branch_name(repo_path: &Path) -> Option<String> {
    let head_path = repo_path.join(".git").join("HEAD");
    // For worktrees, HEAD is at .git/worktrees/<name>/HEAD
    // but the .git file points there
    let content = std::fs::read_to_string(&head_path).ok()?;
    let trimmed = content.trim();
    if let Some(ref_name) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(ref_name.to_string())
    } else {
        // Detached HEAD — return short SHA
        Some(trimmed.chars().take(8).collect())
    }
}
```

**If creating a new file `crates/atm-core/src/project.rs`:**
- Add `pub mod project;` to `crates/atm-core/src/lib.rs`
- Re-export: `pub use project::{resolve_project_root, resolve_worktree_info};`

### Call resolution from the registry actor

**`crates/atmd/src/registry/actor.rs`** — In `handle_register_discovered()` (after line 343 where `working_directory` is set) and in `handle_update_from_status_line()` (when `working_directory` changes):

```rust
// Resolve project/worktree from working directory
if let Some(ref cwd) = session.working_directory {
    if session.project_root.is_none() {
        session.project_root = atm_core::resolve_project_root(cwd);
        let (wt_path, wt_branch) = atm_core::resolve_worktree_info(cwd);
        session.worktree_path = wt_path;
        session.worktree_branch = wt_branch;
    }
}
```

### Verification
Unit tests for `resolve_project_root`:
- Given `/home/user/myapp/src`, returns `/home/user/myapp` (where `.git/` exists)
- Given a worktree path, resolves to the main repo root
- Given `/tmp` with no `.git`, returns `None`

Unit tests for `resolve_worktree_info`:
- Returns branch name from HEAD
- Handles detached HEAD

Integration: register a discovered session with a real git repo path, verify `project_root` and `worktree_branch` are populated.

---

## Task 9: Make tmux.rs::get_parent_pid pub(crate)

### What
The PID ancestry check in Task 6 needs to call `get_parent_pid`. It's currently private.

### Files to modify

**`crates/atmd/src/tmux.rs:112`** — Change:
```rust
fn get_parent_pid(pid: u32) -> Option<u32> {
```
to:
```rust
pub(crate) fn get_parent_pid(pid: u32) -> Option<u32> {
```

### Verification
`cargo build -p atmd` compiles.

---

## Task 10: Write comprehensive tests

### What
End-to-end tests covering the new data flow.

### New tests to write

**`crates/atm-core/src/session.rs` (unit tests):**
- `test_session_domain_new_fields_default` — new fields are None/empty after `::new()`
- `test_session_view_includes_new_fields` — `from_domain()` copies project/worktree/parent/children fields

**`crates/atm-core/src/project.rs` (unit tests):**
- `test_resolve_project_root_standard_repo`
- `test_resolve_project_root_worktree`
- `test_resolve_project_root_no_git`
- `test_resolve_branch_name_attached`
- `test_resolve_branch_name_detached`

**`crates/atmd/src/registry/actor.rs` (unit tests):**
- `test_subagent_start_records_pending` — SubagentStart with agent_id populates pending_subagents
- `test_subagent_stop_clears_pending` — SubagentStop removes the entry
- `test_pending_subagent_ttl_cleanup` — expired entries are cleaned up

**`crates/atmd/tests/registry_integration.rs`:**
- `test_subagent_correlation` — register parent, send SubagentStart, register child with descendant PID, verify parent-child link
- `test_hook_event_forwards_agent_fields` — send SubagentStart hook with agent_id/agent_type, verify they reach the actor
- `test_project_resolution_on_discovery` — register discovered session with working_directory, verify project_root is populated

### Verification
`cargo test --workspace` — all new and existing tests pass.

---

## Dependency Graph

```
Task 1 (SessionDomain fields)
Task 2 (parent/child fields)  ──depends on──→ Task 1
Task 3 (SessionView fields)   ──depends on──→ Tasks 1, 2
Task 4 (pipeline wiring)      ──independent──
Task 5 (SubagentStart logic)  ──depends on──→ Tasks 2, 4
Task 6 (correlation)          ──depends on──→ Tasks 5, 9
Task 7 (TTL cleanup)          ──depends on──→ Task 5
Task 8 (project resolution)   ──depends on──→ Task 1
Task 9 (pub get_parent_pid)   ──independent──
Task 10 (tests)               ──depends on──→ all above
```

**Parallelizable tracks:**
- Track A: Tasks 1 → 2 → 3 → 8 (data model + project resolution)
- Track B: Tasks 4 → 5 → 6 → 7 (pipeline wiring + subagent correlation)
- Task 9 is independent and trivial

Both tracks merge at Task 10 (comprehensive tests).
