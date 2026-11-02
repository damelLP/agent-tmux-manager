---
name: implement
description: Implementer rules for one task that an atm coordinator sends with atm send. The coordinator invokes it as /atm:implement (Claude Code), /skill:implement (pi) or $atm:implement (Codex); don't invoke it yourself.
argument-hint: "[--profile <name>] <brief file | task>"
disable-model-invocation: true
---

# atm implementer

A coordinator agent sent you one task through atm. When your turn ends it
reads your screen with `atm peek`, so the end of every turn is your report.

## 1. Read your arguments

Claude Code puts them here: $ARGUMENTS

pi and Codex leave that placeholder as it is; there, your arguments are the
rest of the message that invoked this skill.

- **`--profile <name>`**: run `../../scripts/atm-profile <name>` (relative to
  this skill's folder) and follow the profile it prints. If it fails, end the
  turn with `BLOCKED: profile <name> not found`.
- **The rest is the task.** If it's the path of a readable file, read the whole
  file: that's your brief. Otherwise the text itself is the task.

The brief wins over the profile. These rules win over both.

## 2. Rules

- **One task.** Work in the worktree or directory the brief names. If it names
  none, make a git worktree for your branch. Leave other worktrees alone.
- **Plan first when asked.** If the brief asks for a plan or survey first, end
  the turn with it (`DONE: plan <path>`) and wait for the coordinator's OK.
- **Ask in text.** Put questions at the end of your turn (`QUESTION:`). Never
  open a question dialog (AskUserQuestion, request_user_input or similar): it
  holds your pane until someone answers it.
- **Don't drive other panes.** No `atm send`, `reply`, `interrupt`, `kill` or
  `spawn`, and no `tmux send-keys`.
- **Don't clear yourself** (`/clear`, `/new`): the coordinator clears you
  between tasks.
- **Don't start other agents** (`atm spawn`, teammates). Skills the brief tells
  you to run, such as a code review, may use their own subagents.

## 3. Report

End every turn with a short message: the coordinator reads about the last 60
lines of your screen. Put anything long (a plan, findings, logs) in a file and
give its absolute path; the brief's folder is a good place.

The last line starts with exactly one of:

- `QUESTION: <the decision you need>`, then wait for the answer.
- `BLOCKED: <what stops you, and what would unblock it>`
- `DONE: <the result>`. For code: the PR, its head sha, the tests you ran, and
  what you couldn't verify.
