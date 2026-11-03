# Decision tests for the atm skills

Each test freezes a coordinator or implementer at one moment where it must
choose what to do next, and checks what it does. The cases live beside the
skills, in skill-creator's case-folder layout:
`skills/coordinate/evals/<case>/` and `skills/implement/evals/<case>/`.

| Case | The moment | Passes when |
|---|---|---|
| `done-plan-no-clear` | alpha reported `DONE: plan <path>` | the plan is read first, an OK or changes goes to %7, no clear |
| `context-over-40` | the coordinator's own row reads 43.4% | a new handover is written |
| `clear-new-id-proceeds` | `/clear` sent; %7 shows a new id and Claude's grey `❯ /clear` | the skill call goes to %7, no second clear |
| `clear-same-id-holds` | `/clear` sent; same id, old report still on screen | no skill call |
| `idle-no-marker` | alpha's turn ended without a marker line | the first send to %7 asks for one |
| `pane-reused` | %7 now holds a pi session in another project | nothing goes to %7; the user is told |
| `brainstorm-ends-with-marker` | an implementer with an unsettled design (superpowers loaded) | its last line is a marker; no dialog tools |

They test the model's decisions, not atm. Whether atm, tmux and the harnesses
behave the way the skills say is checked with real harnesses (see the PR).

## How a run works

`run.sh <case> <plugin-dir> <out-dir> [model]` runs one sealed
`claude -p` in a fresh directory under `/dev/shm`:

- The prompt is `prompt.md` after its settings block. A coordinate case
  resumes the loop from a handover (`files/work/handovers/handover-1.md`).
- `files/work/` becomes the run's working directory; `@RUN@` in any file
  becomes its path.
- A stand-in `atm` (`standin_atm.py`) serves `files/atm/list.json` and
  `files/atm/peek-<N>.txt` and logs every call. After the first write to the
  watched pane (`files/atm/scenario.json`) that pane's session is gone, which
  ends the run soon after the decision. A stand-in `tmux` only logs.
- The seal: `~/.cargo/bin` (the real atm) is off PATH, `TMUX` is unset, and
  `ATM_SOCKET`, `ATM_TMUX_SOCKET` (a `tmux -L` label) and `TMUX_TMPDIR` point
  into the run directory, so nothing can reach your atmd or tmux server. The
  installed atm plugin and hindsight-memory are disabled for the run, and
  `--plugin-dir` loads the plugin copy under test. The rest of your Claude
  Code setup (plugins, hooks, output style) stays, as for a real coordinator.

`grade.py <case> <out-dir>` checks each grader file against the stand-in's log,
the transcript and the files the run created, and writes `grading.json` in
skill-creator's format.

`run-all.sh <iteration-dir> <config> <plugin-dir> [runs] [jobs] [model] [case...]`
runs and grades every case (or the ones named) `runs` times, in the workspace
layout skill-creator's `aggregate_benchmark` and eval viewer read.

**These are billed model runs** (about $0.30 to $0.60 each on Opus 5.5). Don't
point `claude plugin eval` at these folders: its runs would use your real
`atm` and tmux.

## Fixtures

`gen_cases.py` writes every case folder: the cases share a handover, a small
`widgets.py` project and Claude Code's screen frame, so change them there and
regenerate (`python3 gen_cases.py extensions/atm-skills`) rather than editing
the folders. Each case's `case.yaml` comments say which files are captured and
which are synthetic.

Fixtures have to hold together, because the model reads them the way a real
coordinator would:

- Code matches the handover. In the first trial, the coordinator rightly
  stopped on a `widgets.py` that lacked the tasks the handover called done.
- Numbers are believable. A fresh run shown its own row at 43.4% of a 1M
  window (434K tokens) doubted it and carried on; at 43.4% of 200K it wrote
  the handover.
