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
  - changing a public type or function in `atm-core` or `atm-protocol` that
    other crates depend on (or changing the wire format);
  - moving or renaming existing files, types or crates;
  - teaching a vendor-neutral crate about a specific harness.
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
| `atm-protocol` | Vendor-neutral wire messages + parsing | `atm-core` |
| `atm-<vendor>-adapter` | **All** knowledge of one harness: event vocabulary, payload shapes, mapping to core types | `atm-core` only |
| `atm-tmux` | Thin async wrapper over the tmux CLI (`TmuxClient` trait + `MockTmuxClient`) | no internal crates |
| `atmd` | Session registry, broadcast server | core, protocol, adapters |
| `atm` (crate `atm-tui`) | TUI and CLI | core, protocol, tmux, adapters |
| root `atm` package | `atm` / `atmd` binaries | everything |

- **Vendor knowledge lives only in its adapter.** `atm-core`, `atm-protocol`
  and `atmd`'s registry stay vendor-neutral. Adding a harness means a new
  adapter crate, not `if vendor == ..` branches elsewhere.
- Adapters never depend on each other.
- One definition per concept, in `atm-core`. Never create a field-for-field
  copy of an existing type; an adapter's raw payload type exists only because
  the vendor's shape differs.

### Async rules

- Never block the runtime; use `spawn_blocking` for sync work.
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
| No force push, no local `cargo publish` | deny rules (Claude + Codex) |
| Formatting | `cargo fmt --check` — pre-commit, CI |
| Lints, warnings | `clippy -D warnings` — pre-push, CI |
| Tests pass | `cargo test --workspace` — pre-push, CI |
| No panics in production code | review only (not yet linted) |
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
- **Before opening a PR,** review your diff (`/code-review`). Fix the
  findings, or explain them in the PR.

### Working in small contexts

- One task per session.
- Hand noisy exploration to subagents and keep only their conclusions.
- Parallel agents: one task per agent, each in its own git worktree under
  `.worktrees/`.
- Keep new files small and single-purpose. Some existing files (e.g.
  `registry/actor.rs`, `src/bin/atm.rs`) are large; don't split them as part
  of a feature — that's a separate refactor (principle 2).

## 5. Pointers

- Harness integration findings (actual payloads, event names, quirks): the
  relevant adapter's crate docs (`//!` in its `lib.rs`).
- Plans and specs: `docs/plans/` — **local, git-ignored working files**.
  They are point-in-time snapshots and go stale. Never treat one as current
  truth. Anything durable must land in code, tests, this file, or a PR
  description.

## Compaction

Write a summary prompt that loads sufficient context and kick starts the
next stage. No more than 3 sentences before the context is cleared.
