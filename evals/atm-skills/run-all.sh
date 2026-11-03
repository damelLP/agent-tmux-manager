#!/usr/bin/env bash
# Runs every decision test against one copy of the atm skills and grades
# each run, in skill-creator's workspace layout:
#   <iteration-dir>/<case>/eval_metadata.json
#   <iteration-dir>/<case>/<config>/run-<k>/{transcript.jsonl,outputs/,timing.json,grading.json}
#
# Usage: run-all.sh <iteration-dir> <config> <skills-dir> [runs=3] [jobs=6] [model] [case...]
# These are real, billed model runs.

set -u

here=$(cd "$(dirname "$0")" && pwd -P)
[ $# -ge 3 ] || { echo "usage: run-all.sh <iteration-dir> <config> <skills-dir> [runs] [jobs] [model] [case...]" >&2; exit 64; }
iter=$1 config=$2 plugin=$3 runs=${4:-3} jobs=${5:-6} model=${6:-claude-opus-5-5}
shift $(($# < 6 ? $# : 6))
mkdir -p "$iter" && iter=$(cd "$iter" && pwd -P)

jobs_file=$(mktemp)
for case_dir in "$here"/atm-*/*/; do
    case_dir=${case_dir%/}
    name=$(basename "$case_dir")
    if [ $# -gt 0 ] && ! printf '%s\n' "$@" | grep -qx "$name"; then continue; fi
    mkdir -p "$iter/$name"
    prompt=$(awk 'n >= 2 { print; next } /^---$/ { n++ }' "$case_dir/prompt.md" | sed '/./,$!d')
    for g in "$case_dir"/graders/*.md; do awk 'n >= 2 { print; next } /^---$/ { n++ }' "$g" | sed '/./,$!d'; done |
        jq -R -s --arg name "$name" --arg prompt "$prompt" \
            '{eval_name: $name, prompt: $prompt, assertions: (split("\n") | map(select(length > 0)))}' \
            > "$iter/$name/eval_metadata.json"
    for k in $(seq 1 "$runs"); do
        printf '%s\t%s\n' "$case_dir" "$iter/$name/$config/run-$k" >> "$jobs_file"
    done
done

export here plugin model
# shellcheck disable=SC2016
tr '\t' '\n' < "$jobs_file" | xargs -d '\n' -n 2 -P "$jobs" sh -c '
    bash "$here/run.sh" "$1" "$plugin" "$2" "$model" >/dev/null 2>&1
    echo "$(basename "$(dirname "$(dirname "$2")")")/$(basename "$2"): $(python3 -I "$here/grade.py" "$1" "$2" 2>&1 | tail -1)"
' sh
rm -f "$jobs_file"
