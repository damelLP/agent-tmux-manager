# atm skills

Four skills installed for Claude Code, pi and Codex. Choose one protocol:

## Bound named agents (drafts)

- **`atm-lead`**: the designated lead's recovery, assignment, review, decision,
  away and handover workflow.
- **`atm-member`**: a named builder/reviewer/merger's one-unit workflow, durable
  task state, report and save rules.

**Unevaluated for model compliance.** Each rule links to its reason, source,
checked model and harness/version, with unverified behavior stated explicitly
in [`shared/references/atm-named-agents.md`](shared/references/atm-named-agents.md).
Source/installation checks are not permission for unattended activation.
Decision creation has no CLI; decision lists are requester-scoped and only
Owner can inspect another inbox. A2 digest/A3 drift scheduling are not built.
The skills do not create substitute clients or bypass those authority limits.

At each new session run the shell command `atm me`, read its current role and
existing memory, and explicitly load the appropriate skill. `me` does not load
it: an approved role **instruction body** may direct reading the installed
`SKILL.md` in the same turn; `skills = [...]`/`profile` frontmatter alone does
not execute. Role updates/priming are separate operator decisions. Retain the
existing priming/header until bootstrap has been verified for that harness and
model. Setup does not adopt an agent or enable automatic handover.

## Legacy unnamed panes

- **`atm-coordinate`**: the coordinator loop. Plan tasks with the user, send
  each one to a long-lived implementer, wait for it, read its report, review,
  and clear the implementer before its next task.
- **`atm-implement`**: the implementer's rules. One task, in its own worktree,
  and every turn ends with a `QUESTION:`, `BLOCKED:`, `WAITING:` or `DONE:`
  line the coordinator can read.

Everything goes through atm (`atm list`, `peek`, `send`, `reply`, `spawn`),
so the loop is the same whichever harness runs on either side. The commands,
their refusals and what to do instead are in
[`shared/references/atm-commands.md`](shared/references/atm-commands.md).

The legacy pair remains available with its profiles and scripts. Named
lifecycle/message targets now support names; do not carry its pane/waiter loop
or fixed 40% handover threshold into the named skills. `atm profile show`
remains unimplemented; legacy skills use the profile script.

## Prerequisites

- Named skills need the stage-1 commands `atm me`, `atm task`, `atm decisions`
  and the named-agent message/lifecycle commands. Check command `--help` rather
  than assuming the release number identifies those capabilities.
- **Legacy loop: `atm` built after #119.** Its `atm send` pastes text and
  refuses unsafe sends. Check: `atm send --help` mentions "bracketed paste";
  `atm-coordinate` stops if it is missing.
- `atm setup` run for each harness you use: it installs atm's hooks, so the
  sessions show in `atm list`, and these skills.
- `tmux`; the legacy coordinator scripts also require `jq` on `PATH`.
- Codex named members and leads need approved access to ATM's socket too.
  Ask the operator if the sandbox refuses it; do not disable the sandbox as a
  workaround. See the historical
  [command reference](shared/references/atm-commands.md#a-codex-coordinator-needs-its-sandbox-opened).
  Use `--no-daemon` for correct pane attribution (the ATM launcher adds it).

## Install

`atm setup` installs all four skills for each harness it finds, as plain skill
folders: no plugin, no marketplace and no settings entry. The files are built
into the `atm` binary, so `cargo install` and then `atm setup` is all it takes.
Run `atm setup` again after updating atm to update the skills. `atm uninstall`
removes them.

| Harness | Installed in | Invoke |
|---|---|---|
| Claude Code | `~/.claude/skills/<name>/` | `/atm-lead`, `/atm-member`; legacy `/atm-coordinate`, `/atm-implement` |
| pi | `~/.pi/agent/skills/<name>/` | `/skill:atm-lead`, `/skill:atm-member`; legacy `/skill:atm-coordinate`, `/skill:atm-implement` |
| Codex CLI | `~/.codex/skills/<name>/` | `$atm-lead`, `$atm-member`; legacy `$atm-coordinate`, `$atm-implement` |

These are harness **input commands**, not shell commands. Keep a Claude skill
call to one short line, at most 800 characters and 3 lines (recorded on Claude
Code 2.1.316; current-version boundary not rechecked). Put long content in a
brief file. If transporting a call through a shell, single-quote it: otherwise
`$atm-member` expands as a shell variable.

- Each installed skill folder holds its `SKILL.md` and a copy of everything in
  `shared/`: the command reference, the scripts, and `agents/openai.yaml`.
  That file keeps Codex from invoking the skills on its own, since Codex
  ignores `disable-model-invocation`. Claude Code and pi ignore it.
- Loader discovery was checked without a model on Claude Code 2.1.316,
  pi 0.73.1 and codex-cli 0.163.0 (2026-11-03); namespaced invocation and paste
  findings are historical, not fresh named-skill compliance results.
  Claude/Codex picked up skills in running sessions in those checks. Current
  pi 1.5.2 skill docs say `/reload` after an edit/install; that route is
  docs-backed, not trialed here. Restart pi if reload is unavailable.
- The folders are the harnesses' defaults. With `CLAUDE_CONFIG_DIR` or
  `CODEX_HOME` pointing elsewhere, that harness doesn't read them, as with
  atm's hooks.
- `atm setup` replaces atm's four folders whole, so edits there, or a symlink
  in their place, are lost on its next run. To try a change to the skills,
  edit them here, rebuild atm and run `atm setup` again.
- `atm uninstall` removes all four folders and ATM staging, not other skills.
  An older binary's setup/uninstall knows only its old pair, so rollback can
  leave the new pair installed; it does not retire them automatically.

## Layout

- `atm-lead/SKILL.md`, `atm-member/SKILL.md`: the named-agent drafts.
- `atm-coordinate/SKILL.md`, `atm-implement/SKILL.md`: the legacy pair.
- `shared/`: copied into every installed skill folder at the same paths.
- `tests/scripts.sh`: tests for the scripts, which `cargo test` runs. It and
  this README aren't installed.

## Profiles

For the legacy pair, `--profile <name>` adds a reusable brief: a Markdown file
`<name>.md` in `.atm/profiles/` (the first one found walking up from the
current directory) or in atm's config dir (`~/.config/atm/profiles/` on
Linux). The project's file wins; they aren't merged. This repository's
profiles are in [`/.atm/profiles/`](../../../../.atm/profiles/).

## Tests

`tests/scripts.sh` tests the unchanged legacy scripts against a stand-in `atm`
(`cargo test` runs it). Root `tests/setup_atm_skills.rs` checks real setup and
uninstall in private homes: all four folders, exact source bytes, upgrade,
symlinks and failure isolation. `tests/atm_named_skills.rs` checks metadata,
reference/anchor layout, evidence slots and short entry examples, **not agent
behavior**.

[`evals/atm-skills/`](../../../../evals/atm-skills/) holds the legacy billed
`claude -p` decision tests. **Do not run them without approval or reuse the old
runner unchanged for the named skills**: it retains parts of personal setup.
K3 must adapt the cases/client stand-ins and use a reviewed sealed launcher
without personal connectors/skills, with every turn watched including startup.
New baseline/compliance evaluations, current-harness invocation checks, and the
fresh-lead/day-away acceptance tests remain pending separate authorization.
