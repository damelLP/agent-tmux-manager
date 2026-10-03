#!/usr/bin/env bash
# PreToolUse(Bash): stop agents from bypassing the git hooks instead of fixing
# what they report. Greps the raw hook JSON so it needs no jq.
# A speed bump against reflexive bypasses, not a security boundary.
input=$(cat)

block() {
    echo "Blocked: $1 The git hooks are the project's guardrails (AGENTS.md §4)." >&2
    echo "Fix the reported problem instead; ask the user if you believe the hook is wrong." >&2
    exit 2
}

# git accepts any unambiguous prefix of a long option (--no-ver, --no-v).
if grep -qE -- '--no-v(e(r(i(f(y)?)?)?)?)?\b' <<<"$input"; then
    block "--no-verify skips the git hooks."
fi
if grep -qE 'git[^"]*\bcommit\b[^"]*[[:space:]]-[a-zA-Z]*n[a-zA-Z]*([[:space:]]|")' <<<"$input"; then
    block "'git commit -n' skips the git hooks."
fi
# git config keys are case-insensitive.
# Force pushes in any position, including the `+branch` refspec form.
if grep -qE 'git[^"]*\bpush\b[^"]*([[:space:]]-[a-zA-Z]*f[a-zA-Z]*([[:space:]]|")|--force|[[:space:]]\+[^[:space:]"]+)' <<<"$input"; then
    block "force pushes rewrite shared history."
fi
if grep -qiE 'core\.hookspath' <<<"$input"; then
    block "changing core.hooksPath disables the git hooks."
fi
exit 0
