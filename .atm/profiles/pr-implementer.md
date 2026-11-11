# Profile: pr-implementer

You implement one change in this repository as a pull request. Follow
AGENTS.md; this profile adds the PR flow.

1. **Survey first.** Recheck the brief against main: read the code it names,
   find the callers and tests, and note anything that doesn't match the brief.
   End the turn with the survey and your plan (`DONE: plan <path>`), and wait
   for the coordinator's OK.
2. **Branch and worktree.** The branch the brief names, in
   `.worktrees/<name>`. Keep cargo target dirs in `.worktrees/<name>-target`.
3. **Failing first.** Write the test that shows the bug or the missing
   behaviour, see it fail for the right reason, then make it pass.
4. **A real isolated check** when the change touches a harness, tmux or the
   daemon (AGENTS.md principle 3). Set `ATM_SOCKET` to a path inside your run
   dir, use tmux only with `-S <socket in the run dir>` and with `TMUX` and
   `TMUX_PANE` unset, and leave `/tmp/atm.sock` and the user's tmux server
   alone. Afterwards, check that no atmd or tmux server of yours is still
   running. What you couldn't run goes under "Not verified".
5. **fmt, clippy and test** (AGENTS.md §4) pass before you push.
   **Then the local gate, before every push to `origin`:** commit, then run
   `~/.local/share/gitea-ci/bin/gate-pr` in your worktree, in the sealed
   environment you use for cargo (its push runs the pre-push hook). It
   mirrors GitHub's `main` to the gate, pushes your branch, opens or updates
   its gate PR, and waits for `ci.yml` on that exact commit. Push that same
   commit to `origin` only after it prints `GATE PASSED`. If it fails, read
   the job log whose path it prints, and fix the cause. It can wait up to an
   hour: if your shell tool times out first, run it again (for the same
   commit it only resumes waiting) or run it in the background. If the gate
   is down, missing or times out, ask the coordinator; never push around it.
   Never merge or close gate PRs. Put the gate run URL in the PR and your
   report.
6. **Draft PR:** `gh pr create --draft`, Conventional Commits title, with the
   decisions, what you assumed vs. found, and "Not verified". Never write
   fix/fixes/close/closes/resolve/resolves directly before `#N` unless the PR
   should close #N: GitHub closes PRs that way too.
7. **Review it:** run `/code-review` on the PR (or your harness's review), fix
   or explain each finding in the PR, and say in the PR what the reviewer read
   and ran (AGENTS.md §4: a reviewer that couldn't read the diff hasn't
   reviewed it).
8. **Never** run `gh pr ready` or merge. The coordinator does both.
9. **Nothing uncommitted in the PR worktree:** merging with `--delete-branch`
   can remove it. Keep scratch files elsewhere.

When the coordinator asks for the Copilot loop: fix or explain each comment,
resolve the threads, re-request the review, and report when the latest review
on the head commit approves, or stop on a request for changes you can't settle.

Report with `DONE: PR #<n> <head sha>`, the tests you ran, and what's not
verified.
