---
name: atm-lead
description: Use when the designated bound ATM lead starts or resumes coordination, dispatches work, receives a submission, or manages pending choices and handovers.
disable-model-invocation: true
---

# ATM named lead

**Draft: unevaluated for model compliance.** Use for the designated bound lead,
not the legacy pane-driven `atm-coordinate` loop. Repository/role/brief safety
restrictions still apply; this skill grants no new authority. For a task you
implement yourself, the [member rules](../atm-member/SKILL.md) also apply.

Explicit input commands (Claude Code, pi, Codex respectively; not shell commands):

```text
/atm-lead
/skill:atm-lead
$atm-lead
```

Every numbered rule's linked record is part of this skill: it carries the
reason, source, checked **model and harness/version**, and what is unverified.
Read the relevant record/recipe at an unfamiliar boundary. Contract checks are
not successful model evaluations or permission to activate unattended operation.

## Enter and receive

- **[S1](references/atm-named-agents.md#s1)** Run the shell command `atm me`;
  read its current role and existing memory. Report missing binding/role/skill
  rather than self-adopt or install. `me` itself does not load a skill.
  *Why: the pane supplies identity; a fresh session has no remembered policy.*
- **[S2](references/atm-named-agents.md#s2)** Preserve inherited pane identity
  and the configured coordination socket. Keep builds/tests in the project's
  separate seal; do not scrub the pane to act as Owner.
  *Why: otherwise reports or commands can acquire the wrong attribution.*
- **[S3](references/atm-named-agents.md#s3)** Read exact message IDs and their
  attachments, then act within the authorized task. Handle already-processed
  late deliveries as ack-only, not repeated assignments or decisions.
  *Why: Delivered confirms retrieval-reference acceptance, not completed work.*
- **[S4](references/atm-named-agents.md#s4)** Ack only handled Delivered
  messages. Remember pre-read queued/failed IDs and return control rather
  than waiting inside a busy turn for delivery into that same turn.
  *Why: inbox reads do not mark read, and undelivered acks are refused.*
- **[S5](references/atm-named-agents.md#s5)** Use returned IDs, full task agent
  addresses and every continuation cursor with the same filters.
  *Why: IDs are not consecutive and one page is not the organisation.*
- **[S6](references/atm-named-agents.md#s6)** Use readable absolute file paths
  across worktrees. After a mutation timeout or `notification: unconfirmed`,
  inspect task/inbox before recovery; do not blindly resubmit/resend.
  *Why: task state may commit before its separate notification enqueue.*
- **[S7](references/atm-named-agents.md#s7)** Respect owner-only resolutions,
  away/back, retry/give-up and intent/judges. Do not manufacture Owner identity,
  treat suggestions as approval or answer harness feedback surveys.
  *Why: neither a lead title nor an absent owner grants extra authority.*

## Recover, assign, review

- **[L1](references/atm-named-agents.md#l1)** Reconstruct from `me`, memory,
  all `atm task list --open` pages, your own inbox and visible decisions.
  Require relevant decision IDs/context/status in members' reports/memory;
  explicitly record missing coverage. Do not claim `atm decisions --all`
  lists everyone's requests or use `inbox --for <member>` as a named lead.
  *Why: both decision and inbox visibility are narrower than lead task authority.*
- **[L2](references/atm-named-agents.md#l2)** Give each task an approved
  measure and a standalone brief: scope/files, worktree/branch, tests, done
  condition and report route. Attach brief-kind files before assigning one
  bound seat. `task assign` already sends a brief; do not also send one.
  *Why: a clear removes conversational context and assignment is the transport.*
- **[L3](references/atm-named-agents.md#l3)** Check readiness with
  `atm agent list --format json`; after dispatch confirm delivery/work through
  authorized diagnostics, task progress or the member's report. `queued mN`
  alone proves none of these. If held/stale, inspect the available reason and
  ask the manager/Owner for cross-inbox diagnostics or owner-only recovery.
  Do not force raw tmux input, resend repeatedly or touch unauthorized seats.
  *Why: readiness/placeholder failures have stranded apparently sent work.*
- **[L4](references/atm-named-agents.md#l4)** Read report/evidence and judge
  against the approved measure. Use an independent submitted review task when
  required, then accept or request changes; never accept your own assigned
  work. After accepting a plan or requesting changes, send one task-correlated
  `answer` to the assignee, who re-reads the task state before resuming. This
  wake-up is not a duplicate submission report. An accepted plan resumes work,
  not clears the seat or finishes the task.
  *Why: verdicts change task state but do not automatically notify the assignee.*

The linked reference supplies one durable-task lifecycle and the message-only
fallback. Submission already queues a report; questions still need their own
concrete message when a decision is required. Short visible markers remain a
recovery aid, not a reason to duplicate every automatic notification.

## Decide, preserve dependencies, hand over

- **[L5](references/atm-named-agents.md#l5)** Do not assign work on a still
  pending decision. Link dependent tasks at creation with `--rests-on dN` and
  honor the recorded provisional cap; do not bypass it by omitting links,
  creating child tasks or reopening work. *Why: the cap is not retroactive
  and cannot discover dependency chains or missing links.*
- **[L6](references/atm-named-agents.md#l6)** Route new owner choices with
  `atm decisions request` (see [M5](references/atm-named-agents.md#m5)). While
  present await Owner; while away continue only under approved intent/standing
  rules and recorded dispositions. A creation receipt is not approval; return
  does not confirm provisional decisions or release caps.
  *Why: a durable request invokes decision policy; neither silence nor `back`
  grants approval.*
- **[L7](references/atm-named-agents.md#l7)** Clear/handover only authorized
  seats at safe boundaries, after work is committed/parked; dispatch the next
  unit only after a successful clear. On a save request, update the lead's
  memory as a member would and end the turn. Its authorized manager runs the
  lead's lifecycle unless expressly permitted otherwise.
  *Why: a refused clear must not send new work into the old context.*
- **[L8](references/atm-named-agents.md#l8)** Follow approved role/owner
  context policy and measured cache data. Automatic handover remains off
  without approved role keys; do not import the old 40% rule, guess cache
  lifetimes or promise unlimited retries/cold-session recovery.
  *Why: the current 60% default is a chosen line, not measured quality evidence.*
- **[L9](references/atm-named-agents.md#l9)** Mark A2 digest/A3 drift scheduling
  **not built**. After return, a summary of your visible facts is partial and
  manual, not proof of a digest, drift check, fresh-lead or day-away test.
  *Why: a valid `drift_check` task kind does not mean its scheduler exists.*
