#!/usr/bin/env bash
# Given/When/Then integration checks for the configured Codex shell hooks.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

session="$(jq -er '.hooks.SessionStart[0].hooks[0].command' "$root/.codex/hooks.json")"
guard="$(jq -er '.hooks.PreToolUse[0].hooks[0].command' "$root/.codex/hooks.json")"
[[ "$(jq -r '.hooks.PreToolUse[0].matcher' "$root/.codex/hooks.json")" == Bash ]]

# Given a checkout with spaces, when startup runs below its root without
# Claude's environment, then the versioned Git hooks activate, idempotently.
checkout="$scratch/checkout with spaces"
mkdir -p "$checkout/.claude/hooks" "$checkout/app/src"
git init --quiet "$checkout"
cp "$root/.claude/hooks/ensure-git-hooks.sh" "$checkout/.claude/hooks/"
other="$scratch/other checkout"
git init --quiet "$other"
(
    cd "$checkout/app/src"
    CLAUDE_PROJECT_DIR="$other" bash -c "$session"
    [[ "$(git config --local --get core.hooksPath)" == .githooks ]]
    unset CLAUDE_PROJECT_DIR
    bash -c "$session"
    bash -c "$session"
)
[[ "$(git -C "$checkout" config --get core.hooksPath)" == .githooks ]]
[[ -z "$(git -C "$other" config --local --get core.hooksPath || true)" ]]

run_guard() {
    local command=$1 expected=$2 actual=0
    jq -cn --arg command "$command" \
        '{hook_event_name:"PreToolUse", tool_name:"Bash", tool_input:{command:$command}}' |
        (cd "$root" && bash -c "$guard") >"$scratch/result" 2>&1 || actual=$?
    if [[ $actual != "$expected" ]]; then
        echo "Expected exit $expected, got $actual for: $command" >&2
        cat "$scratch/result" >&2
        exit 1
    fi
    if [[ $expected == 2 ]]; then
        grep -q 'Blocked:' "$scratch/result"
    else
        [[ ! -s "$scratch/result" ]]
    fi
}

# Given a shell call, when it bypasses Git hooks or force-pushes, then the
# guard rejects it. These are hook inputs; none of these commands execute.
for command in \
    'git commit --no-verify -m test' \
    'git push --no-v' \
    'git commit -n -m test' \
    'git commit -an -m test' \
    'git config core.hooksPath /tmp/empty' \
    'git -c CORE.HOOKSPATH=/tmp/empty commit -m test' \
    'git push --force origin main' \
    'git push origin main -f' \
    'git push origin --force-with-lease main' \
    'git push origin +main'; do
    run_guard "$command" 2
done

# Given an ordinary Git call, when the guard runs, then it permits execution.
for command in \
    'git status --short' \
    'git diff --check' \
    'git commit -m test' \
    'git push origin main'; do
    run_guard "$command" 0
done

echo 'Codex hook scenarios passed.'
