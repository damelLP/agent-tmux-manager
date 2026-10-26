# AGENTS.md — Agent Tmux Manager (ATM)

Instructions for coding agents (and humans) working in this repository.
Read this file fully before making changes. When a rule here conflicts with
a habit or a default, this file wins. `CLAUDE.md` is a symlink to this file.

## Principles

These govern every unit of work. Follow them literally.

### 1. Smallest change that works

- Deliver the requested behaviour with the fewest changed lines of
  **production code**.
- Tests and required docs don't count. Never shrink them to save lines.
- Minimal does **not** mean: skipping error handling needed for correctness,
  copying logic that already exists (reuse it), or adding special-case flags
  or branches to unrelated code.
- Change nothing the task doesn't require: no drive-by renames, reformatting,
  or "while I'm here" cleanups.

### 2. If it doesn't fit, stop and ask

- A change **doesn't fit** if finishing it would require any of:
  - changing the shape of a public type or the signature of a public
    function in `atm-core` or `atm-protocol` that other crates depend on
    (or changing the wire format);
  - moving or renaming existing files, types or crates;
  - teaching a vendor-neutral crate about a specific harness.
- A behaviour-only fix to a public function in those crates fits. Name it
  in the PR description and check every caller.
- When a change doesn't fit, **do not do the refactor yourself**, however
  small. Stop, describe the refactor needed, and ask the user.
- A refactor is its own unit of work. It **must not change behaviour**:
  existing tests pass without edits, except to follow code that moved or was
  renamed. New tests may be added.

### 3. Verify against reality before building on it

ATM sits on top of external systems (Claude Code hooks, pi extensions, Codex
hooks, tmux). Their real behaviour beats docs, and docs beat assumptions.

- **Before writing production code against an external system**, run a cheap
  real check: a minimal script, a real hook payload, a real tmux command. Log
  the actual data.
- Record what you assumed vs. what you found in the PR description, and fix
  any doc that encoded the wrong assumption.
- Past example: we assumed the status line carried token counts and a
  `PermissionRequest` hook existed. At the time neither was true (cost/duration
  only; `PreToolUse` instead). Checking first saved weeks of rework.
- Vendor behaviour changes between releases. Note the harness version you
  verified against, and re-check old findings before relying on them.
- **After building, see it work once.** A change that depends on harness
  behaviour is not done until it has run against the real harness (real
  `atmd`, real session). If you can't run it, list it under "Not verified"
  in the PR.
- **"Can't happen" is a claim to check.** Before leaving a known limit
  unfixed, name the real case you checked. If you only reasoned about it,
  write "unverified".

### 4. Panic-free core, explicit contracts

- **No panics in production code.** No `.unwrap()`, `.expect()`, `panic!`,
  `unreachable!`, `todo!`, or slice indexing (`v[i]`). Use `?`, `.get()`,
  `.ok_or_else(..)`, `unwrap_or*`, `if let`.
- Allowed only in: tests; infallible operations (e.g. a literal `Regex`) with
  `.expect("why it can't fail")`; setup where failure means "cannot proceed";
  after explicit validation with a comment saying why it's safe.
- **Expected failures are values.** Return `Result<_, ThisCrateError>`
  (`thiserror`) from libraries; `anyhow` only in binaries.
- **Keep new logic pure.** Parsing, state transitions and mapping take plain
  inputs (time, IDs passed in) and do no I/O, so they test without tmux or a
  daemon. Side effects belong in `atmd`, `atm-tmux`, and the binaries.
- Use newtypes for IDs (`SessionId(String)`, not `String`), `#[must_use]`
  where ignoring a return is a bug, and derive `Debug`/`Clone`/`Default`
  liberally.

### 5. `main` always works

- Work happens on a branch; branch commits may be WIP.
- **Nothing merges into `main` unless fmt, clippy and tests pass** (see
  Commands). `main` must always build, start the daemon, and keep existing
  behaviour.

## 1. Architecture

```
harness ──hook/extension──▶ atmd (daemon) ◀──unix socket── atm (TUI/CLI)
```

| Crate | Responsibility | May depend on |
|---|---|---|
| `atm-core` | Vendor-neutral domain types | nothing internal |
| `atm-protocol` | Vendor-neutral wire messages and protocol versioning | `atm-core` |
| `atm-<vendor>-adapter` | **All** knowledge of one harness: event vocabulary, payload shapes, mapping to core types | `atm-core` only |
| `atm-tmux` | Thin async wrapper over the tmux CLI (`TmuxClient` trait + `MockTmuxClient`) | no internal crates |
| `atmd` | Session registry, broadcast server | core, protocol, tmux, adapters |
| `atm` (crate `atm-tui`) | TUI and CLI | core, protocol, tmux, adapters |
| root `atm` package | `atm` / `atmd` binaries | everything |

- **Vendor knowledge lives only in its adapter.** `atm-core`, `atm-protocol`
  and `atmd`'s registry stay vendor-neutral. A harness's events, payloads and
  parsing go in its adapter crate; other crates get only the data entries
  and the one dispatch arm listed in "Adding a harness", never
  `if vendor == ..` logic. Two documented exceptions:
  - **Built-in harness metadata.** `atm-core`'s `Harness` (`harness.rs`) and
    `HarnessDefinition`, `BUILTIN_HARNESSES`, `ProcessMatcher`,
    `ArgvExclude` and `PromptMode` (`harness_registry.rs`) hold static facts
    about each CLI as data: its name, command, process matchers and prompt
    mode. Spawning and discovery read them instead of branching per vendor
    (c23888d, #44). They never hold payload schemas, event names or parsing.
  - **Per-harness wire envelopes.** `atm-protocol`'s `MessageType` has an
    envelope variant for each kind of payload a harness sends: `PiEvent`
    and `CodexEvent`, and Claude's `HookEvent` and `StatusUpdate`, which
    predate the naming. Each carries the payload as opaque JSON that only
    the adapter parses.
  - Not exceptions: Claude-specific code that predates this rule and is
    still in `atm-core`, for example model ids and pricing in `model.rs`,
    `AgentType::from_subagent_type`, tool classification in `tool.rs`,
    `SessionDomain::claude_code_version` and `ContextUsage::exceeds_200k`.
    Each moves out in its own approved refactor, never inside an unrelated
    change.
- Adapters never depend on each other.
- One definition per concept, in `atm-core`. Never create a field-for-field
  copy of an existing type; an adapter's raw payload type exists only because
  the vendor's shape differs.

### Adding a harness

Steps 2 and 3 change public `atm-core` and `atm-protocol` types, so agree a
new harness with the user before starting (principle 2). That one approval
covers the steps below. Codex (#30) touched 23 files:

1. A new `crates/atm-<vendor>-adapter`: raw payload types, event vocabulary
   and the mapping to `atm-core` lifecycle events. Its crate and module docs
   (`//!`) record the verified payloads, quirks and harness version.
2. `atm-core`: a `Harness` variant and a `BUILTIN_HARNESSES` entry, static
   facts only. Amp, Qwen and Gemini already have both, with
   `discovery_enabled: false`. Set it to `true` once the adapter exists, and
   update `discovery.rs`'s test that lists the enabled harnesses.
3. `atm-protocol`: a `MessageType::<Vendor>Event { data }` variant and its
   constructor (`message.rs`), plus an arm in `tests/proptest_roundtrip.rs`,
   whose coverage guard fails without one. `version.rs` says additive
   changes bump the minor version, but `CURRENT` stayed 1.0 through
   `PiEvent` and `CodexEvent`, and the hook scripts hard-code 1.0; decide
   the bump with the user.
4. `atmd`: the adapter dependency, and dispatch of the new message to the
   adapter in `server/connection.rs`. Discovery reads the registry entry;
   add the harness's process shapes to `discovery.rs`'s tests.
5. The forwarder the harness runs: a hook script under `crates/atm/scripts/`
   or an extension. It must honour `ATM_SOCKET` (default `/tmp/atm.sock`),
   as `atm-hook` and `atm-codex-hook` do. `extensions/pi-atm` is
   authoritative, `crates/atm/assets/pi-atm` is its install mirror, and
   `tests/setup_pi_package.rs` guards them.
6. `atm` (crate `atm-tui`): the adapter dependency in `crates/atm/Cargo.toml`
   (`setup.rs` reads the adapter's hook event list), and install and
   uninstall in `setup.rs`. A badge colour in `ui/session_list.rs` is
   optional; unknown harnesses get grey.
7. The root `Cargo.toml` (`[workspace.dependencies]` and members) and
   `.github/workflows/release.yml` (§2): the adapter publishes before
   `atmd` and `atm-tui`.
8. Tests: adapter tests on real captures (§3), an end-to-end replay in
   `crates/atmd/tests/server_integration.rs`, and setup tests in
   `tests/setup_*.rs`.
9. `README.md`: the supported harnesses and their setup.

### Async rules

- Never block the runtime; use `spawn_blocking` for sync work.
  - **Exception: blocking I/O that can hang forever** (a read of a FIFO, a
    stuck filesystem) runs on a detached thread instead. tokio's runtime
    waits for every `spawn_blocking` task when it drops, so a hung one stops
    the process from ever exiting. Spawn it with `std::thread::Builder`, so
    a failed spawn is an error rather than a panic. Report back through a
    `oneshot` with a timeout, and keep at most one call in flight per
    resource: keep the pending receiver instead of spawning again, or hung
    calls pile up threads. #84 introduces the pattern. Older
    `spawn_blocking` reads of hook-supplied paths predate this rule.
- Every external operation (tmux, socket, subprocess) has a timeout.
- Handle channel closure (`None`/`Err`) as a normal shutdown path.
- Use `CancellationToken` for cooperative shutdown.

## 2. Dependency policy

- All shared versions live in `[workspace.dependencies]` in the root
  `Cargo.toml`; crates use `{ workspace = true }`.
- Adding a dependency needs a one-line justification in the PR description.
- **Adding, renaming or removing a crate** also updates
  `.github/workflows/release.yml` (release dispatch inputs and the
  dependency-ordered crates.io publish list) in the same change. A crate must
  publish after everything it depends on.

## 3. Testing

- Pure logic: plain inputs and outputs, in the crate's own tests.
- tmux: use `MockTmuxClient` (a fake), not a mocking framework. Real tmux is
  exercised in `tests/e2e_with_tmux.rs`.
- TUI rendering: `insta` snapshots (`crates/atm/tests/ui_snapshots.rs`).
  Accept new snapshots **only** when the UI change is intended, and say so in
  the commit message. Never accept just to make a test pass.
- Adapters: test against **real captured payloads** from the harness, not
  hand-imagined ones (principle 3).
- Mark each real capture where it's documented (the adapter's crate or
  module docs, or a comment beside the test) with
  `Captured: <harness> <version>, <YYYY-MM-DD>, <method>.`, writing
  "not recorded" for any part nobody recorded. Mark a hand-built payload in
  an adapter's tests with `Synthetic: <why>`. Unmarked payloads in other
  crates' tests are hand-written, and the adapter's crate docs say so. Put
  the harness version in captured fixture filenames
  (`claude_tui_session_2.1.310.jsonl`).

## 4. Workflow and guardrails

### Commands

- Format: `cargo fmt --all`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- **Work is not done until all three pass.**

### What enforces what

Git hooks live in `.githooks/`. Agent sessions activate them automatically
(SessionStart); humans run `git config core.hooksPath .githooks` once.

If a check fails, fix the cause; never bypass it (no `--no-verify`, no
force push). If you believe a check is wrong, stop and ask the user.

Codex loads `.codex/config.toml`, `.codex/hooks.json`, and `.codex/rules/`
for trusted checkouts. Its command rules mirror `.claude/settings.json` —
change both together. After changing hooks, restart the session and trust
them with `/hooks`.

| Rule | Enforced by |
|---|---|
| No force push, no `--no-verify`, no local `cargo publish` | deny rules (Claude + Codex) |
| Formatting | `cargo fmt --check` — pre-commit, CI |
| Lints, warnings | `clippy -D warnings` — pre-push, CI |
| Tests pass | `cargo test --workspace` — pre-push, CI |
| No `unwrap`/`expect` in production code | `clippy::unwrap_used`/`expect_used` — pre-push, CI (tests exempt via `clippy.toml`) |
| No other panics (`panic!`, indexing, ...) | review only |
| Vendor isolation, crate deps | review only |
| Release publish order | review only (`release.yml`) |

### Commits and PRs

- Conventional Commits for commit subjects and PR titles, e.g.
  `fix: install new-window hook on workspace create`.
- **Commits are the progress log.** One logical step each, with a body saying
  what changed and why. A new session recovers context from `git log`.
- Record decisions in the PR description: what you chose, why, and what you
  rejected. Don't silently reverse a past decision; find it with
  `git log --grep "<topic>"` or `gh pr list --state merged --search "<topic>"`,
  link it, and say why it no longer holds.
- **Open PRs as drafts** (`gh pr create --draft`). Before marking one
  ready, review it with `/code-review <PR#>` and fix or explain the
  findings. A reviewer that could not read the diff has not reviewed it.
  Say in the PR what the reviewer read and ran. Ready also means
  principle 3's "see it work once" is done or listed under "Not verified".
- **Stacked PRs:** retarget the child to `main` before merging the parent.
  Never delete a branch that another open PR is based on.

### Working in small contexts

- One task per session.
- Hand noisy exploration to subagents and keep only their conclusions.
- Parallel agents: one task per agent, each in its own git worktree under
  `.worktrees/`.
- Keep new files small and single-purpose. Some existing files (e.g.
  `registry/actor.rs`, `src/bin/atm/main.rs`) are large; don't split them as
  part of a feature — that's a separate refactor (principle 2).

## 5. Pointers

- Harness integration findings (actual payloads, event names, quirks): the
  relevant adapter's crate docs (`//!` in its `lib.rs`) and module docs
  (`//!` atop each module, e.g. `atm-claude-adapter`'s `status_line.rs` and
  `atm-codex-adapter`'s `translate.rs`).
- Plans and specs: `docs/plans/` — **local, git-ignored working files**.
  They are point-in-time snapshots and go stale. Never treat one as current
  truth. Anything durable must land in code, tests, this file, or a PR
  description.

## Compaction

Write a summary prompt that loads sufficient context and kick starts the
next stage. No more than 3 sentences before the context is cleared.
