# Named-agent workflows and verification records

**Draft: unevaluated for model compliance.** Installation/metadata checks are
not agent behavior tests. New skill evaluations and real-harness turns require
separate approval. Do not enable unattended use on the strength of these files.

This reference is installed beside both named skills. Each rule record below
carries its reason, durable source, checked model/harness and remaining limits.
Source paths refer to the ATM repository, not the installed skill directory.
Contract records were inspected at `30475a20` on 2026-11-16; a source inspection
is not a new test run. Historical captures/incidents do not verify this wording.

## Commands, not a second protocol

The normal new-session line is
`[mN] New session: run the shell command atm me`. Run the shell command, not a
personal skill. `atm me` shows the current role body, manager, memory path,
unread references and assigned unfinished tasks. It does not read memory or
load a skill. The role's `skills = [...]` and `profile` fields do not execute.
A role body can explicitly direct a read of the installed `SKILL.md`; the
operator retains existing priming until that load is verified for its harness.

### Message recipe

For S1–S6 and M4, substitute the IDs/manager returned by the daemon:

```sh
atm me
atm inbox --id m41
# Read the body and files listed under attach: with the native file tool.
atm msg ack m41  # only after handling AND status=delivered
# Message-only assignment: one report, not a duplicate of task submit.
atm msg send --to project/lead --kind report --task unit-1 \
  --body 'DONE: result; checks; not verified: ...' --attach /absolute/report.md
```

Kinds include `report` and `question`. `--task unit-1` is correlation text, not
a task creation. Read IDs from command output: event-sequence IDs are not
consecutive. A pre-read queued message may be delivered after the current
turn ends; remember that it was handled and later ack without repeating work.
Failed messages remain unread but cannot be acknowledged until Delivered.

### Durable-task recipe

For L2/M2–M4: a creator/lead creates and assigns; only the assignee works and
submits. Use an exact measure approved in the project's `.atm/intent.md`.
These examples are alternative steps at the appropriate state, not a script
to run unconditionally:

```sh
# Creator/lead: attach brief-kind files before assigning; assign queues a brief.
atm task create 'Build the change' \
  --measure '{"kind":"mechanical","command":"cargo test --workspace"}' \
  --file '{"path":"/absolute/brief.md","kind":"brief","note":null}'
atm task assign t41 project/member
# Assigned member: inspect the state first, including after a verdict answer.
atm task get t41
# Keep submitted/in_review unchanged and await the verdict. Withdraw submitted
# only on an explicit authorized request; in_review cannot return to working.
# From assigned/working/waiting/blocked/changes_requested, if work may proceed:
atm task set-state t41 working
# When stopping that work:
atm task set-state t41 waiting --reason 'Waiting for decision d52'
atm task set-state t41 blocked --reason 'Required fixture is missing'
# At a plan stop, submit the plan and stop; accepted plan resumes working.
atm task submit t41 --what plan \
  --file '{"path":"/absolute/plan.md","kind":"plan","note":null}'
# When the result is ready, submit queues a report to the task creator.
atm task submit t41 \
  --file '{"path":"/absolute/report.md","kind":"report","note":null}'
```

`--measure` is JSON, not a JSON filename; other measures are
`{"kind":"reviewed","rubric":".atm/measures/review.md"}` and `{"kind":"owner"}`.
An absent intent file permits legacy creation; that is not approval to invent
a judge or edit intent. Paths are literal, not uploaded or checked for existence.
Task commands require full `project/name` addresses, without bare-name lookup.

For L4: review and verdict require a submitted target. A reviewed result needs
a submitted independent review (unless Owner accepts); an owner measure needs
Owner. The assigned agent never accepts its own task:

```sh
atm task create 'Review the change' --kind review --judges t41 \
  --measure '{"kind":"owner"}'
atm task assign t63 project/reviewer
atm task review t41 t63
# Only after t63 is submitted as a result, and the evidence passes:
atm task verdict t41 accepted --review-task t63 --note 'Checked the measure'
# Alternative after a submission that needs changes:
atm task verdict t41 changes_requested --note 'Missing regression coverage'
```

After accepting a **plan** or requesting changes, the lead sends one answer to
wake the assignee; an accepted result is final and does not need this resume step:

```sh
atm msg send --to project/member --kind answer --task t41 \
  --body 'Verdict recorded for t41; re-read atm task get t41 before resuming.'
# Assignee: re-read state, then follow the M2 state branches above.
atm task get t41
```

The answer is not a duplicate submission report. Unlike assign/submit, verdicts
have **no automatic assignee notification**; reminders depend on turn-end
observations, not just a later verdict. Automatic verdict wake-up is a candidate
follow-up; real-harness execution of this manual loop is **not verified**.

Creation/assignment/submission and message enqueue are separate durable writes.
`queued mN` is not delivery. `notification: unconfirmed`, a connection error or
a mutation timeout does not mean rollback: inspect `task get`, task pages and
your own inbox before recovery. Do not blindly submit or send again. Restart
replays a committed task but does not repair a missing task-notification enqueue.

For S5/L1, task lists, inboxes and decision lists are paged; `task get` returns
one whole task and has no cursor. Continue `next_after`/`next: --after` with the
same filters until there is no cursor:

```sh
atm task list --open --limit 100
atm task list --open --limit 100 --after t41
atm inbox --unread --after m41
atm decisions --all --format json
```

## Decision and visibility gaps (M5, L1, L3)

There is **no decision-request CLI** in this build. `decision.request` is an
API method, not `atm decisions request`; withdrawal is API-only too. Its request
has kind/title/context/files/options/recommended/may_be_undone/changes_intent/
deadline. Do not invent a raw-socket client inside a task. Without an approved
client, send a concrete question to the manager, state the recommendation and
what waits, and say **no durable decision was created**. A question message
does not enter automatic away processing. Complete decision creation needs a
separate CLI/client unit; see the repository README's Decision API.

A named caller's `atm decisions` lists only that caller's requests, including
with `--all`; the latter changes status filtering, not authority. Only Owner
can list all decisions or use `atm inbox --for <another-agent>`. A named lead
cannot inspect a member's returned message ID through its own inbox. Use the
member's task state/report and `atm agent list --format json` for readiness;
ask the authorized manager/Owner for cross-inbox delivery diagnostics when
needed. Do not claim a complete organisation-wide decision or receipt view.

## Away and return (M6, L5–L6, L9)

Only Owner changes `atm away`/`atm back` or resolves with `atm decide`. The
away flag is recorded state, not an inference from silence. While present,
nothing is taken for Owner. While away, the recorded request is processed as:

| Request | Outcome on its recommended option |
|---|---|
| Undoable detail, no intent change | Agent-decided, recorded by the requester |
| Undoable intent change | Provisional, recorded by system |
| Irreversible/external, action/hire/budget/unknown kind, or no recommendation | Waits |

Both allowed outcomes have `taken_provisionally` status. Wait for the recorded
disposition/answer message before dependent work. For an intent-changing
provisional decision, link every dependent task at creation with
`--rests-on dN`. Default project cap is 1 including the task already in progress:
no further task starts. The setting comes from `.atm/away.toml` at `away`.
Omitted links and dependency chains are not discovered; at larger caps a
reopened task frees a counted place. Do not exploit these limits. Do not assign
a task on a still-pending decision: becoming provisional does not stop already
assigned work. `back` neither confirms a decision nor releases its cap.

**Not built:** A2's return digest/overrule summary and A3's automatic drift-check
task. `drift_check` is a valid task kind, not a running scheduler. `back` currently
prints counts and a digest placeholder. A named lead may summarize visible
facts as a *partial manual summary*, not claim a complete digest, drift report,
day-away run or fresh-lead acceptance test.

## Save and clear (M7, L7–L8)

`atm agent handover project/member` requests a save to the memory path from
`me`, waits for that turn to end and the regular file's size/mtime to change,
then clears. A timeout can leave its ordinary save message queued; a late save
does not prove a clear occurred. Only authorized Owner recovery cancels it.
A failed clear is not permission to send the next task anyway.

Automatic handover is off unless role `auto_handover` enables it; its context
line defaults to 60%, a chosen number, not measured quality data. Idle handover
requires a configured cache lifetime and task/decision-backed waits; message-only
waits are invisible. There is at most one attempt per session, with restart/
undelivered-request limits described in the repository README. Follow approved
role/owner policy, not a universal 40% threshold or a guessed cache lifetime.

## Verification records

### S1

Run shell `atm me`, read the current role and existing memory, and stop/report
if binding, role or required skill is missing; do not self-adopt.
**Why:** Identity is pane-bound; context only points to memory and instructions.
**Source:** `tests/atm_me.rs::me_real_daemon_context_and_report_command`; `crates/atmd/src/org/agent_context.rs::assemble_context`; actual named-session entry observation.
**Checked:** 2026-11-16; harness: pi installed package 1.5.2; model: gpt-6.1-sol/max; shell entry and identity observed, not new-skill execution.
**Not verified:** New-skill loading/compliance on pi, Claude Code or Codex.

### S2

Preserve inherited pane identity/configured socket for coordination; use the
project's separate sealed environment for builds/tests.
**Why:** Dropping the pane attributes calls to Owner; sealed tests are not work transport.
**Source:** `crates/atmd/src/org/message_service_tests.rs::sender_from_live_bound_pane` and `unknown_or_down_pane_is_owner`; captured 2026-11-15 switch rehearsal signatures.
**Checked:** 2026-11-15; harness: Claude Code 2.1.325 and pi 1.5.0; model: Haiku 5.5 and gpt-6.1-sol xhigh/low; real bound/unbound signatures. Contract inspected at 30475a20.
**Not verified:** New wording's isolation discipline; Codex named workflow.

### S3

Fetch the exact reference ID and attachments; act on an authorized brief without
an extra go-ahead. Already-handled late delivery is ack-only.
**Why:** Delivered confirms reference acceptance, not reading or execution.
**Source:** 2026-11-15 switch/chunk-7 captures: Haiku read briefs but asked again, pi acted; stale handled-message backlog incident 2026-11-15 22:59–23:01; `tests/atm_msg.rs::inbox_is_nonmutating_preserves_json_nulls_and_escapes_text_controls`.
**Checked:** 2026-11-15; harness: Claude Code 2.1.325, pi 1.5.0/1.5.2; model: Haiku 5.5 versus gpt-6.1-sol xhigh/low; capture evidence, not successful anti-repeat wording evaluation.
**Not verified:** This rule fixes Claude compliance or prevents duplicate action.

### S4

Acknowledge handled Delivered messages only. Remember pre-read queued IDs and
end the turn rather than awaiting delivery into that busy turn.
**Why:** Fetch does not mark read; one undelivered ID refuses an entire ack batch.
**Source:** `crates/atmd/src/org/message_service_tests.rs::queued_or_failed_ack_refuses_entire_batch`, `ack_batch_is_atomic`, `repeated_ack_returns_zero`; real queued-ack refusal on 2026-11-16.
**Checked:** 2026-11-16; harness: pi installed package 1.5.2; model: gpt-6.1-sol/max; actual queued read/ack refusal, source contract inspected at 30475a20.
**Not verified:** End-to-end late-delivery handling with either new skill.

### S5

Use returned canonical IDs/full task addresses and follow all filtered task-list,
inbox and decision-list pages. `task get` returns one whole task without paging.
**Why:** IDs are not consecutive and a first page is not the entire backlog.
**Source:** `src/bin/atm/task.rs`; `tests/atm_task.rs::lists_gets_controls_pagination_and_cost_are_visible_and_safe`; `crates/atmd/src/org/message_service_tests.rs::inbox_sequence_pagination_and_filters`; nonconsecutive IDs in switch capture.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free contract); model: N/A. Historical ID observation: Claude Code 2.1.325/Haiku 5.5 and pi 1.5.0/sol, 2026-11-15.
**Not verified:** Model pagination/ID discipline with new skills.

### S6

Use readable absolute attachment paths across worktrees; inspect committed state
and notification outcomes before recovering an uncertain mutation.
**Why:** Paths are not uploads; a failed enqueue/timeout need not undo a task.
**Source:** `crates/atmd/src/org/task_worker_tests.rs::enqueue_failure_is_a_partial_result_not_a_failed_transition` and `task_only_durable_prefix_replays_without_fabricating_a_message`; `tests/atm_msg.rs::send_preserves_multiline_body_and_literal_attachments_and_all_six_kinds`.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free contract); model: N/A. Attached-task success captured 2026-11-15: pi 1.5.2/gpt-6.1-sol low.
**Not verified:** New-skill uncertain-write recovery behavior.

### S7

Respect owner-only decisions/away/message recovery and approved intent/judges;
never manufacture Owner identity, answer feedback surveys or treat suggested
editor text as approval.
**Why:** An agent cannot grant itself the owner's authority.
**Source:** `crates/atm-org/tests/decision.rs::only_the_owner_decides_in_every_state`; `crates/atm-org/tests/away.rs::only_the_owner_sets_away_and_back`; legacy `atm-coordinate` Never section; owner-operation incidents 2026-11-15.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free authority contract); model: N/A. Legacy survey/approval wording's checked model/version: not recorded.
**Not verified:** New-skill resistance to authority/feedback temptations; attribution is not hostile-client authentication.

### M1

One scoped unit/worktree. Ask the lead in end-of-turn text when inputs/scope are
missing, write plans to files, and do not start/drive other seats or open
interactive question/plan-approval dialogs.
**Why:** Isolation avoids overlapping changes; dialogs strand unattended panes.
**Source:** Legacy `atm-implement` rules 1/2; `crates/atm-org/src/task.rs` ownership table.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free contract); model: N/A. Legacy discipline trial model/harness version: not recorded.
**Not verified:** New wording's behavioral compliance.

### M2

Get the canonical assigned task and branch on state. Keep submitted/in_review
unchanged pending verdict; withdraw submitted only on an explicit authorized
request (in_review cannot return to working). From assigned/working/waiting/
blocked/changes_requested, mark working only when authorized work can proceed;
use waiting/blocked with a reason when stopping that work.
**Why:** Durable state survives a restart; submitted-to-working retracts the submission.
**Source:** `crates/atm-org/src/task.rs` transitions; `crates/atmd/src/org/task_reminder_tests.rs`; real chunk-7 pi task capture, 2026-11-15.
**Checked:** 2026-11-15; harness: pi 1.5.2; model: gpt-6.1-sol low; get/work/submit exercised. Other state contracts inspected at 30475a20 on 2026-11-16.
**Not verified:** New-skill resume/withdrawal and waiting/blocked discipline; Claude/Codex task execution.

### M3

Submit a requested plan as plan and stop for acceptance. On a non-final verdict
answer, re-read task state before resuming under M2; result submission is not
acceptance and the assignee never gives its own verdict.
**Why:** Plan acceptance resumes work; result acceptance is final and judged.
**Source:** `crates/atm-org/src/task.rs` transition/review table; `tests/atm_task.rs::every_task_command_works_against_the_real_private_daemon`.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free task contract); model: N/A. Real result-only path: pi 1.5.2/gpt-6.1-sol low, 2026-11-15.
**Not verified:** Real-model plan-stop/acceptance behavior with either new skill.

### M4

Submit a durable task's plan/result once with its files; for message-only work
send one correlated report/question to the manager, with attachment and marker.
Keep a short visible end-of-turn marker as recovery, not a second notification.
**Why:** Submit already reports to the creator; every extra message costs a lead turn.
**Source:** `crates/atmd/src/org/tasks.rs::notification`; `crates/atmd/src/org/task_worker_tests.rs::system_created_task_report_falls_back_to_owner`; real chunk-7 pi submission capture.
**Checked:** 2026-11-15; harness: pi 1.5.2; model: gpt-6.1-sol low; automatic report observed. New duplicate-avoidance/marker wording has no model trial.
**Not verified:** New-skill report frequency, question routing and duplicate prevention.

### M5

Ask with context/options/recommendation/dependency and await an authoritative
answer. Without an approved request client, identify the question message as
not creating a durable decision.
**Why:** The CLI cannot create a decision and ordinary chat does not invoke away policy.
**Source:** Repository README Decision API; `crates/atmd/src/org/decision_methods.rs`; `atm decisions --help` and `atm decide --help` read on 2026-11-16.
**Checked:** 2026-11-16 source/help inspection at 30475a20; harness: N/A (model-free CLI/API contract); model: N/A.
**Not verified:** Complete CLI decision creation (not built); new-skill question discipline.

### M6

While away, obey the recorded recommendation/disposition: detail may be
agent-decided; undoable intent change may be provisional; external/irreversible,
action/hire/budget/unknown kinds or no recommendation wait. Never reclassify
for permission.
**Why:** Away changes availability, not the limits of delegated authority.
**Source:** `crates/atm-org/tests/away.rs::away_table_every_cell` and `apply_away_takes_a_change_of_intent_by_system_and_a_detail_by_its_requester`; `crates/atm-org/tests/away_kinds.rs`; `tests/away_delivery.rs`.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free policy/stand-in); model: N/A.
**Not verified:** Real-harness away decisions/new-skill compliance.

### M7

On a save request, update the exact memory path with task/branch/head, evidence,
handled-unacked IDs, decisions/dependencies, open questions and next action;
end the turn. No raw self-clear or self-handover without explicit role authority.
**Why:** Handover needs a changed regular memory file and the save turn's end.
**Source:** `crates/atmd/src/org/agent_handover_tests.rs::the_save_goes_on_only_when_delivered_idle_and_changed` and `a_turn_that_ends_without_a_changed_file_is_not_cleared`; `tests/agent_handover.rs` stand-in flows.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free handover contract); model: N/A.
**Not verified:** Real-model completeness of saved state; new-skill handover compliance.

### L1

Reconstruct from context/memory, open-task pages and own inbox/decisions; carry
members' relevant decision facts in reports/memory and record visibility gaps.
**Why:** Named leads lack global decision and cross-inbox privileges.
**Source:** `crates/atmd/src/org/decisions.rs::visible`; `crates/atmd/src/org/message_service_tests.rs::agent_inbox_isolation_and_owner_inspection`; `crates/atm-org/tests/task_fallback_replay.rs`.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free visibility/replay contract); model: N/A.
**Not verified:** Complete fresh-lead reconstruction; global decision/receipt view not built.

### L2

Create a standalone task with approved measure, scope/worktree/tests/done/report
contract and brief-kind attachments; assign one bound seat once.
**Why:** A new session cannot recover an unstated brief; assignment sends the brief.
**Source:** `tests/atm_task.rs` create/assign contract; `crates/atm-org/src/task.rs` measures; real chunk-7 task capture.
**Checked:** 2026-11-15; harness: pi 1.5.2; model: gpt-6.1-sol low; task creation/assignment delivery observed. Intent admission inspected at 30475a20.
**Not verified:** New lead skill's brief quality or approved-measure selection.

### L3

Check recipient readiness/delivery or work after dispatch; inspect holds through
authorized diagnostics, escalate Owner-only recovery and never force/resend
blindly or drive raw tmux around the guards.
**Why:** Queued is not started; placeholder sessions/readiness failures caused unnoticed stalls.
**Source:** Dispatch-stall incident 2026-11-16 00:32 (pi placeholders after restart); `crates/atmd/src/org/message_service_tests.rs::agent_inbox_isolation_and_owner_inspection`; captured `screen_styled_lead-idle-shells_2.1.325.txt` under `crates/atm-claude-adapter/tests/fixtures/`.
**Checked:** 2026-11-15/16 incidents; harness: Claude Code 2.1.325 lead and pi 1.5.2 members; model: Opus 5.5 and gpt-6.1-sol max; observed stalls/recovery, not a proven universal cause.
**Not verified:** Independent named-lead receipt inspection (not available); new-skill stall detection.

### L4

Read report/evidence, require independent review when the measure calls for it,
then accept/request changes. After plan acceptance or changes requested, send one
task-correlated answer to the assignee, who re-reads state before resuming. This
wake-up is not a duplicate submission report; an accepted plan is not a finished
task or clear boundary.
**Why:** Verdict authority/measures constrain acceptance; verdicts do not notify the assignee.
**Source:** `crates/atm-org/src/task.rs` Reviews and measures; `crates/atmd/src/org/tasks.rs::execute_mutation`; `crates/atmd/src/org/task_reminder.rs::Watch::observe`; `tests/atm_task.rs`; legacy `evals/atm-skills/atm-coordinate/done-plan-no-clear/` scenario (not run for this change).
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free verdict contract); model: N/A. Legacy scenario result/version not recorded here.
**Not verified:** New-skill review quality/plan handling and real-harness verdict wake-up; billed evaluation deferred. Automatic assignee notification on task.verdict is absent, a candidate follow-up.

### L5

Do not assign work on a pending decision; link provisional dependencies with
`--rests-on dN`, honor the cap, and do not bypass it with chains/reopening/omitted links.
**Why:** The cap is not retroactive and cannot discover unrecorded dependencies.
**Source:** `crates/atm-org/tests/away.rs::default_cap_holds_a_task_that_rests_on_a_provisional_decision_until_it_is_answered` and `a_cap_of_two_lets_one_further_task_start`; A1 review incident 2026-11-15 (already-assigned tasks continue).
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free cap contract); model: N/A.
**Not verified:** Model dependency-link discipline; arbitrary transitive-chain enforcement is not built.

### L6

Route new owner choices while present; while away, continue only under approved
intent/rules and recorded dispositions. Return does not confirm provisional choices.
**Why:** Availability is not approval; external/publish/promotion/spend authority stays separate.
**Source:** `tests/away_delivery.rs::a_decision_asked_while_away_is_taken_told_capped_and_survives_a_restart`; `crates/atm-org/tests/away.rs::nothing_is_taken_after_the_owner_is_back`.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free policy/stand-in); model: N/A.
**Not verified:** Real-model away/return behavior with these skills.

### L7

Clear/handover authorized seats at safe boundaries only; dispatch the next unit
only after successful clear. Save the lead like a member; let its authorized
manager run its lifecycle unless expressly permitted otherwise.
**Why:** A refused clear once let a new brief reach the old high-context session.
**Source:** Clear-then-send incident 2026-11-15 20:25; `crates/atmd/src/org/agent_handover_tests.rs::a_refused_clear_after_the_save_says_that_the_state_is_saved`.
**Checked:** 2026-11-15 incident; harness: Claude Code 2.1.325 lead, pi member (exact running version not recorded); model: Opus 5.5 and gpt-6.1-sol; source contract inspected at 30475a20.
**Not verified:** New-skill clear ordering; raw self-clear is not granted by API support for self-handover.

### L8

Use approved role/owner context policy and measured cache data; automatic
handover remains off without approved keys, with no invented quality/cache threshold.
**Why:** 60% is a chosen default; automatic attempts and cold-session recovery have limits.
**Source:** `crates/atm-org/src/auto_handover.rs`; `tests/agent_auto_handover.rs`; repository README Handing an agent over. Legacy 40% is not measured data.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free handover policy); model: N/A.
**Not verified:** Model quality at a context line, cache lifetime or unattended real-model operation.

### L9

Keep missing digest/drift automation and complete day-away/fresh-lead guarantees
explicitly pending; any summary from visible facts is partial and manual.
**Why:** A valid drift_check kind is not a scheduler; back currently prints a placeholder.
**Source:** `tests/atm_away.rs::back_prints_the_time_the_counts_and_the_digest_placeholder`; repository README Away.
**Checked:** 2026-11-16 source inspection at 30475a20; harness: N/A (model-free CLI contract); model: N/A.
**Not verified:** A2 digest/A3 drift scheduling (not built), day-away/fresh-lead model acceptance.
