#!/usr/bin/env bash
# PreToolUse(Bash): stop agents from bypassing the git hooks instead of fixing
# what they report, and from force pushing. Heredoc bodies and quoted strings
# are dropped first, so commit messages that mention a flag don't trip it.
# A speed bump against reflexive bypasses, not a security boundary.
cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0

block() {
    echo "Blocked: $1 The git hooks are the project's guardrails (AGENTS.md §4)." >&2
    echo "Fix the reported problem instead; ask the user if you believe the hook is wrong." >&2
    exit 2
}

# Drop heredoc bodies, then quoted strings, then put each command on its own line.
cmds=$(awk '
    delim != "" { line = $0; if (strip) sub(/^\t+/, "", line); if (line == delim) delim = ""; next }
    { print; s = $0; gsub(/<<</, "", s) }
    match(s, /<<-?[ \t]*[\047"]?[A-Za-z_][A-Za-z0-9_]*/) {
        d = substr(s, RSTART, RLENGTH); strip = (d ~ /^<<-/)
        sub(/^<<-?[ \t]*[\047"]?/, "", d); delim = d
    }' <<<"$cmd" |
    sed -E "s/'[^']*'//g; s/\"([^\"\\\\]|\\\\.)*\"//g" |
    tr ';&|()`' '\n')

git='(^|[[:space:]/])git[[:space:]](.*[[:space:]])?'
end='([[:space:]]|$)'

# git accepts any unambiguous prefix of a long option (--no-ver, --no-v).
if grep -qE -- "${git}--no-v(e(r(i(f(y)?)?)?)?)?$end" <<<"$cmds"; then
    block "--no-verify skips the git hooks."
fi
if grep -qE -- "${git}commit[[:space:]](.*[[:space:]])?-[a-zA-Z]*n[a-zA-Z]*$end" <<<"$cmds"; then
    block "'git commit -n' skips the git hooks."
fi
# Force pushes in any position, including the `+branch` refspec form.
if grep -qE -- "${git}push[[:space:]](.*[[:space:]])?(-[a-zA-Z]*f[a-zA-Z]*|--force(-with-lease[^[:space:]]*)?|\+[^[:space:]]+)$end" <<<"$cmds"; then
    block "force pushes rewrite shared history."
fi
# git config keys are case-insensitive. Reading the key is fine.
if grep -iE -- "${git}.*core\.hookspath" <<<"$cmds" | grep -qvE -- "config[[:space:]]+(--get|get)[[:space:]]"; then
    block "changing core.hooksPath disables the git hooks."
fi
exit 0
