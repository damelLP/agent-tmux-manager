---
name: atm-member
description: Use when a bound named ATM member starts or resumes assigned work, receives a brief or answer, reaches a plan stop, or is asked to save for handover.
disable-model-invocation: true
---

# ATM named member

**Draft: unevaluated for model compliance.** These are named-agent rules, not
the legacy pane-driven `atm-implement` workflow. Repository/role/brief safety
restrictions still apply; this skill grants no new authority.

Explicit input commands (Claude Code, pi, Codex respectively; not shell commands):

```text
/atm-member
/skill:atm-member
$atm-member
```

Every numbered rule's linked record is part of this skill: it carries the
reason, source, checked **model and harness/version**, and what is unverified.
Contract checks are not new model trials. Read the relevant record/recipe when
a command or boundary is unclear; do not replace missing evidence with a guess.

## Enter and receive

- **[S1](references/atm-named-agents.md#s1)** Run the shell command `atm me`;
  read its current role body and existing memory file. If binding, role or a
  required skill is missing, stop/report rather than self-adopt or install.
  *Why: the pane, not cwd or `ATM_AGENT`, determines who speaks.*
- **[S2](references/atm-named-agents.md#s2)** Run coordination with inherited
  pane identity and the configured coordination socket. Run builds/tests in
  the project's separate seal; never scrub the pane to act as Owner.
  *Why: otherwise your words can be signed Owner or reach the wrong daemon.*
- **[S3](references/atm-named-agents.md#s3)** For `[mN] Read: atm inbox --id mN`,
  run that exact command, read the body/attachments and act on your authorized
  brief without another go-ahead. A previously handled late delivery is not
  fresh work. *Why: Delivered means the retrieval reference was accepted.*
- **[S4](references/atm-named-agents.md#s4)** After handling a **Delivered**
  message, `atm msg ack mN`. If pre-read while Queued/Failed, remember the
  handled ID, end the turn, and ack when later Delivered; never wait inside
  the busy turn for its own delivery. *Why: fetching is not acknowledgement.*
- **[S5](references/atm-named-agents.md#s5)** Use returned IDs, full
  `project/name` for task agents, and every continuation cursor with unchanged
  filters. *Why: IDs are not consecutive and one page can omit work.*
- **[S6](references/atm-named-agents.md#s6)** Use readable absolute attachment
  paths across worktrees; inspect task/inbox after uncertain writes or
  `notification: unconfirmed` before recovery. *Why: paths are not uploads,
  and a timeout or failed notification does not undo a committed task.*
- **[S7](references/atm-named-agents.md#s7)** Respect owner-only resolutions,
  away/back, retry/give-up and approved intent/judges. Never manufacture Owner
  identity, answer feedback surveys, or treat editor suggestions as approval.
  *Why: missing authority cannot be supplied by the agent itself.*

## Work one unit

- **[M1](references/atm-named-agents.md#m1)** Work only in the brief's scoped
  worktree/branch. Ask the lead in end-of-turn text if inputs or scope are
  missing; write a requested plan to a file, not an approval dialog. Do not
  start/drive other seats. *Why: unattended dialogs and overlapping work stall
  the organisation; role/brief/repository restrictions remain binding.*
- **[M2](references/atm-named-agents.md#m2)** If given a canonical task `tN`,
  `atm task get tN`, compare its `agent_id` with `atm me`, and branch on state.
  If `accepted`/`cancelled` or no longer assigned to you, stop obsolete work;
  do not submit, resume or clear on a note alone. For a review, also read its
  `judges` task before continuing. Invalidation Notes do not interrupt a running
  turn, so re-check at work boundaries, not only after messages arrive.
  Keep `submitted`/`in_review`
  unchanged and await the verdict; withdraw a `submitted` task only on an
  explicit authorized request (`in_review` cannot return to `working`). From
  `assigned`/`working`/`waiting`/`blocked`/`changes_requested`, set `working`
  only when authorized work can proceed. When stopping that work, use
  `waiting --reason '...'` or `blocked --reason '...'`. If help/choice is needed,
  send an explicit question to the lead (or M5 decision request for an owner
  choice); those states/reasons do not automatically ask anyone for action.
  *Why: setting a submitted task to working retracts it, not just resumes it.*
- **[M3](references/atm-named-agents.md#m3)** At a plan stop submit with
  `--what plan` and stop until acceptance. On the lead's task-correlated answer
  after plan acceptance or changes requested, re-read `atm task get tN` before
  resuming under M2. A submitted result still needs a verdict; never approve
  your own assigned task.
  *Why: plan acceptance resumes work; result acceptance finishes it.*
- **[M4](references/atm-named-agents.md#m4)** For a durable task, submit its
  plan/result and files **once** with `atm task submit`; it already queues a
  report to the creator. For message-only work without a canonical task ID,
  send one correlated report/question to the manager printed by `me`, with
  report-file attachment. Do not invent a task or send a duplicate automatic
  report. Keep a short visible final marker (`QUESTION:`, `BLOCKED:`, `WAITING:`,
  `DONE: plan <path>` or `DONE: <result>`); put long evidence in the file.
  *Why: each extra delivered progress message starts another lead turn.*

## Choices, away and save

- **[M5](references/atm-named-agents.md#m5)** Request owner choices from your
  bound pane with `atm decisions request`, including context, options,
  recommendation and truthful explicit `--may-be-undone`/`--changes-intent`
  values (see recipe). Consume the recorded disposition/answer before dependent
  work; a returned decision ID is not approval. Withdraw only your own Pending
  request with `atm decisions withdraw dN --reason '...'`.
  *Why: durable requests enter the owner/away workflow; ordinary chat does not.*
- **[M6](references/atm-named-agents.md#m6)** While away, obey the recorded
  recommended disposition: detail may be agent-decided; undoable intent
  change may be provisional; irreversible/external, action/hire/budget/unknown
  kinds or no recommendation wait. Do not reclassify for permission or start
  further work around a provisional cap. *Why: absence is not blanket approval.*
- **[M7](references/atm-named-agents.md#m7)** On a save request, update the exact
  memory path from `me` with task/branch/head, evidence, handled-unacked IDs,
  decisions/dependencies, open questions and next action; finish the turn.
  Do not raw-clear yourself or initiate self-handover without express role
  authority. *Why: the daemon needs the save turn's end and a changed regular
  memory file before clearing.*
