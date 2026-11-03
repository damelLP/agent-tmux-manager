#!/usr/bin/env bash
# Runs one decision test: a sealed `claude -p` run of one case folder against
# one copy of the atm plugin. See README.md beside this file.
#
# Usage: run.sh <case-dir> <plugin-dir> <out-dir> [model]
#
# The seal: the run's PATH starts with a stand-in `atm` (standin_atm.py) and a
# stand-in `tmux` that only logs, and leaves out ~/.cargo/bin, where the real
# atm lives. Even if something reached a real atm or tmux, ATM_SOCKET,
# ATM_TMUX_SOCKET and TMUX_TMPDIR point into the run dir and TMUX is unset, so
# nothing can reach the user's atmd or tmux server. The installed atm plugin
# and hindsight-memory are disabled for the run; --plugin-dir loads the copy
# under test. Everything else in the user's Claude Code setup (plugins, hooks,
# output style) stays, because that's where a real coordinator runs.

set -u

here=$(cd "$(dirname "$0")" && pwd -P)
[ $# -ge 3 ] || { echo "usage: run.sh <case-dir> <plugin-dir> <out-dir> [model]" >&2; exit 64; }
case_dir=$(cd "$1" && pwd -P) || exit 64
plugin=$(cd "$2" && pwd -P) || exit 64
mkdir -p "$3/outputs" && out=$(cd "$3" && pwd -P) || exit 64
model=${4:-claude-opus-5-5}

# Short, so socket paths fit in sun_path.
# work/ is the run's working directory (@RUN@ in the case's files); atm/ holds
# what the stand-in serves, outside it.
run=$(mktemp -d /dev/shm/atmdec.XXXXXX) || exit 1
work=$run/work
mkdir -p "$run/bin" "$run/tmp" "$run/tmux" "$run/s" "$run/atm" "$work"
cp -R "$case_dir/files/." "$run/"
grep -rlF '@RUN@' "$run" 2>/dev/null | while read -r f; do sed -i "s#@RUN@#$work#g" "$f"; done
find "$work" -type f | sort > "$run/files-before"

cat > "$run/bin/atm" <<EOF
#!/bin/sh
exec /usr/bin/python3 -I "$here/standin_atm.py" "$run/atm" "$run/atm.log" "\$@"
EOF
cat > "$run/bin/tmux" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$run/tmux.log"
echo "no server running on $run/tmux/tmux-$(id -u)/default" >&2
exit 1
EOF
chmod +x "$run/bin/atm" "$run/bin/tmux"
printf '{"enabledPlugins": {"atm@agent-tmux-manager": false, "hindsight-memory@hindsight": false}}\n' \
    > "$run/settings.json"

own_pane=$(jq -r '.own_pane // "%3"' "$run/atm/scenario.json" 2>/dev/null || echo %3)
# The prompt is prompt.md after its settings block.
prompt=$(awk 'n >= 2 { print; next } /^---$/ { n++ }' "$case_dir/prompt.md" | sed -e '/./,$!d' -e "s#@RUN@#$work#g")
node_bin=$(dirname "$(command -v node 2>/dev/null || echo /usr/bin/node)")

start=$(date +%s%N)
(
    cd "$work" && env -i HOME="$HOME" USER="${USER:-$(id -un)}" LOGNAME="${USER:-$(id -un)}" \
        LANG="${LANG:-C.UTF-8}" TERM=xterm-256color SHELL=/bin/bash \
        PATH="$run/bin:$HOME/.local/bin:$node_bin:/usr/local/bin:/usr/bin:/bin" \
        TMPDIR="$run/tmp" TMUX_TMPDIR="$run/tmux" TMUX_PANE="$own_pane" \
        ATM_SOCKET="$run/s/atm.sock" ATM_TMUX_SOCKET=atm-decisions-none \
        timeout 900 "$HOME/.local/bin/claude" -p "$prompt" \
        --model "$model" --plugin-dir "$plugin" --settings "$run/settings.json" \
        --permission-mode auto --output-format stream-json --verbose \
        --no-session-persistence --max-turns 40 < /dev/null
) > "$out/transcript.jsonl" 2> "$out/stderr.txt"
rc=$?
end=$(date +%s%N)

# Stop anything the run left behind (a background atm-wait, a stray daemon).
for p in /proc/[0-9]*; do
    if grep -qaF "$run" "$p/environ" 2>/dev/null || grep -qaF "$run" "$p/cmdline" 2>/dev/null; then
        echo "leftover $(basename "$p"): $(tr '\0' ' ' < "$p/cmdline" | cut -c1-120)" >> "$out/stderr.txt"
        kill "$(basename "$p")" 2>/dev/null
    fi
done

cp "$run/atm.log" "$out/outputs/atm.log" 2>/dev/null || : > "$out/outputs/atm.log"
cp "$run/tmux.log" "$out/outputs/tmux.log" 2>/dev/null || :
jq -r 'select(.type == "result") | .result // ""' "$out/transcript.jsonl" > "$out/outputs/final.md" 2>/dev/null
# Files the run created (a handover, a plan), with the run dir shown as @RUN@.
find "$work" -type f | sort | comm -13 "$run/files-before" - |
    while read -r f; do
        rel=${f#"$work"/}
        mkdir -p "$out/outputs/created/$(dirname "$rel")"
        sed -e "s#$work#@RUN@#g" "$f" > "$out/outputs/created/$rel"
    done
sed -i -e "s#$work#@RUN@#g" -e "s#$run#@RUN_ROOT@#g" \
    "$out/outputs/atm.log" "$out/outputs/final.md" "$out/transcript.jsonl"

jq -n --slurpfile r <(jq -c 'select(.type == "result")' "$out/transcript.jsonl") \
    --argjson ms $(((end - start) / 1000000)) --argjson rc "$rc" '
    ($r[0] // {}) as $res
    | ($res.usage // {}) as $u
    | {total_tokens: (($u.input_tokens // 0) + ($u.output_tokens // 0)
          + ($u.cache_read_input_tokens // 0) + ($u.cache_creation_input_tokens // 0)),
       duration_ms: $ms, total_duration_seconds: ($ms / 1000),
       cost_usd: ($res.total_cost_usd // null), num_turns: ($res.num_turns // null), exit_code: $rc}' \
    > "$out/timing.json"

rm -rf "$run"
exit "$rc"
