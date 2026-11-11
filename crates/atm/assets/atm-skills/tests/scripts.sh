#!/usr/bin/env bash
# Tests for the atm skills' scripts, with no daemon and no tmux:
# - atm-wait against a stand-in `atm` that replays scripted
#   `atm list -f json` output, one line per poll;
# - atm-profile's lookup in a scratch directory tree.
#
# Run: bash crates/atm/assets/atm-skills/tests/scripts.sh
# `cargo test` runs it through tests/atm_skills_scripts.rs.

set -u

here=$(cd "$(dirname "$0")" && pwd -P)
scripts=$here/../shared/scripts
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/results"

cat > "$tmp/bin/atm" <<'EOF'
#!/usr/bin/env bash
# Stand-in atm: each call prints the next line of $SEQ (the last one repeats).
# A line reading FAIL makes the call exit 1.
n=$(cat "$SEQ.n" 2>/dev/null || echo 0)
total=$(wc -l < "$SEQ")
line=$(sed -n "$(( n < total ? n + 1 : total ))p" "$SEQ")
echo $((n + 1)) > "$SEQ.n"
[ "$line" = FAIL ] && exit 1
echo "$line"
EOF
chmod +x "$tmp/bin/atm"

# Bound a call where coreutils' timeout exists.
bounded() {
    if command -v timeout >/dev/null; then timeout 20 "$@"; else "$@"; fi
}

# row <id> <pane> <status> [parent]
row() {
    printf '{"id":"%s","tmux_pane":"%s","status":"%s"%s}' "$1" "$2" "$3" \
        "${4:+,\"parent_session_id\":\"$4\"}"
}
idle=$(row lead %5 idle)
busy=$(row lead %5 working)
dialog=$(row lead %5 attention_needed)

# wait_case <name> <expected word> <expected rc> <poll lines> -- <atm-wait args>
wait_case() {
    local name=$1 want=$2 want_rc=$3
    shift 3
    : > "$tmp/$name"
    while [ "$1" != -- ]; do
        printf '%s\n' "$1" >> "$tmp/$name"
        shift
    done
    shift
    (
        got=$(SEQ="$tmp/$name" PATH="$tmp/bin:$PATH" bounded bash "$scripts/atm-wait" "$@" 2>/dev/null)
        rc=$?
        if [ "$got" = "$want" ] && [ "$rc" = "$want_rc" ]; then
            echo "ok   atm-wait $name"
        else
            echo "FAIL atm-wait $name ($*): got '$got' rc=$rc, want '$want' rc=$want_rc"
        fi
    ) > "$tmp/results/$name" &
}

wait_case turn_runs_then_ends IDLE 0 "[$idle]" "[$idle]" "[$busy]" "[$busy]" "[$idle]" -- %5
wait_case role_metadata_does_not_change_waiting IDLE 0 \
    "[$(printf '%s' "$busy" | jq -c '. + {role: "coordinator"}')]" \
    "[$(printf '%s' "$idle" | jq -c '. + {role: null}')]" -- %5
wait_case dialog_during_turn ATTENTION 0 "[$idle]" "[$busy]" "[$dialog]" -- %5
wait_case dialog_at_first_poll ATTENTION 0 "[$dialog]" -- %5 6
wait_case after_reply_turn_ends IDLE 0 "[$dialog]" "[$dialog]" "[$busy]" "[$idle]" -- --after-reply %5
wait_case after_reply_new_dialog ATTENTION 0 "[$dialog]" "[$busy]" "[$dialog]" -- --after-reply %5
wait_case after_reply_dialog_stays TIMEOUT 3 "[$dialog]" -- --after-reply %5 3
wait_case after_reply_turn_already_ended IDLE 0 "[$idle]" -- --after-reply %5 30 3
wait_case subagent_dialog ATTENTION 0 "[$idle,$(row kid %5 attention_needed lead)]" -- %5 6
wait_case subagent_working_lead_idle NEVER_STARTED 4 "[$idle,$(row kid %5 working lead)]" -- %5 30 3
wait_case teammate_own_pane IDLE 0 \
    "[$(row mate %5 working boss),$(row boss %7 idle)]" \
    "[$(row mate %5 idle boss),$(row boss %7 idle)]" -- %5
wait_case pane_gone GONE 2 "[]" -- %5
wait_case misses_between_errors IDLE 0 "[]" FAIL "[]" FAIL "[]" "[$busy]" "[$idle]" -- %5
wait_case never_started NEVER_STARTED 4 "[$idle]" -- %5 30 2
wait_case never_started_short_timeout NEVER_STARTED 4 "[$idle]" -- %5 2
wait_case still_working TIMEOUT 3 "[$busy]" -- %5 2
wait_case already_started IDLE 0 "[$idle]" -- %5 30 0
wait_case bad_timeout "" 64 "[$idle]" -- %5 x
wait_case bad_flag "" 64 "[$idle]" -- --bogus %5
wait_case bad_pane "" 64 "[$idle]" -- 5
wait

# atm-profile: the project's file wins, then the config dir, else exit 1.
profiles=$tmp/profiles
mkdir -p "$profiles/repo/.atm/profiles" "$profiles/repo/.worktrees/x/sub" "$profiles/cfg/atm/profiles"
echo PROJECT > "$profiles/repo/.atm/profiles/a.md"
echo CONFIG-A > "$profiles/cfg/atm/profiles/a.md"
echo CONFIG-B > "$profiles/cfg/atm/profiles/b.md"

# profile_case <name> <dir> <profile> <expected stdout> <expected rc>
profile_case() {
    local got rc
    got=$(cd "$2" 2>/dev/null; XDG_CONFIG_HOME=$profiles/cfg bounded bash "$scripts/atm-profile" "$3" 2>/dev/null)
    rc=$?
    if [ "$got" = "$4" ] && [ "$rc" = "$5" ]; then
        echo "ok   atm-profile $1"
    else
        echo "FAIL atm-profile $1: got '$got' rc=$rc, want '$4' rc=$5"
    fi
} >> "$tmp/results/profiles"

nested=$profiles/repo/.worktrees/x/sub
profile_case project_wins "$nested" a PROJECT 0
profile_case config_fallback "$nested" b CONFIG-B 0
profile_case missing "$nested" c "" 1
profile_case slash_in_name "$nested" ../a "" 64
profile_case leading_dot "$nested" .a "" 64
gone_dir=$profiles/gone
mkdir "$gone_dir"
got=$(cd "$gone_dir" && rmdir "$gone_dir" && XDG_CONFIG_HOME=$profiles/cfg bounded bash "$scripts/atm-profile" b 2>/dev/null)
rc=$?
if [ "$got" = CONFIG-B ] && [ "$rc" = 0 ]; then
    echo "ok   atm-profile deleted_cwd" >> "$tmp/results/profiles"
else
    echo "FAIL atm-profile deleted_cwd: got '$got' rc=$rc, want 'CONFIG-B' rc=0" >> "$tmp/results/profiles"
fi

cat "$tmp/results"/*
! grep -q '^FAIL' "$tmp/results"/*
