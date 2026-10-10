#!/usr/bin/env bash
# The census workflow's steps. They live on the development branch, next to the code,
# so the workflow file on the default branch never has to change when they do.
#
#   census-ci.sh resume   pending checkpoints of the last successful run
#   census-ci.sh record   record until 30 min before the job limit (CENSUS_MINUTES caps it)
#   census-ci.sh report   the day's census report over this run and the last 8
#   census-ci.sh replay   the winners analysis over the same runs' tapes (the step keeps its old name)
#
# resume, report and replay read earlier runs' artifacts with `gh` (GH_TOKEN).
set -uo pipefail

bin=./engine/target/release/copybot
summary=${GITHUB_STEP_SUMMARY:-/dev/stdout}

# ids of the last N successful census runs
runs() {
    gh run list --workflow census.yml --status success --limit "${1:-8}" \
        --json databaseId --jq '.[].databaseId' 2>/dev/null || true
}

# copy the files under $1 that match the find test in $3... to $2, keeping their folders
copy() {
    local src=$1 dst
    dst=$(realpath -m "$2")
    shift 2
    mkdir -p "$dst"
    [ -d "$src" ] || return 0
    (cd "$src" && find . -type f \( "$@" \) -exec cp --parents -t "$dst" {} +)
}

case "${1:-}" in
resume)
    mkdir -p data/census
    id=$(runs 1 | head -n 1)
    if [ -n "$id" ]; then
        gh run download "$id" --name census-state --dir data/census && echo "resuming from run $id"
    fi
    ;;
record)
    used=$(( ($(date +%s) - ${JOB_START:-$(date +%s)}) / 60 ))
    want=${CENSUS_MINUTES:-345}
    left=$(( 330 - used ))
    mins=$(( want < left ? want : left ))
    [ "$mins" -lt 1 ] && mins=1
    echo "recording for $mins minutes (setup took $used)"
    exec "$bin" census --out data/census --minutes "$mins" --raw lists
    ;;
report)
    copy data/census data/all/this-run ! -name 'trades-*'
    for id in $(runs 8); do
        gh run download "$id" --name census-tape --dir "data/all/$id" 2>/dev/null || true
    done
    today=$(date -u +%F)
    yesterday=$(date -u -d yesterday +%F)
    for day in "$yesterday" "$today"; do
        "$bin" census-report --dir data/all --day "$day" > "report-$day.md" || true
    done
    { cat "report-$today.md"; echo; echo "---"; cat "report-$yesterday.md"; } >> "$summary"
    ;;
replay)
    copy data/census data/replay/this-run -name micro.jsonl -o -name 'trades-*.jsonl.gz'
    for id in $(runs 8); do
        mkdir -p "data/replay/$id"
        gh run download "$id" --name census-trades --dir "data/replay/$id" 2>/dev/null || true
        copy "data/all/$id" "data/replay/$id" -name micro.jsonl
    done
    # working backwards from the winners: books, checkpoints and lists from data/all,
    # the trade tape from data/replay
    "$bin" winners --dir data/all --trades data/replay > winners.md || true
    { echo; echo "---"; cat winners.md; } >> "$summary"
    ;;
*)
    echo "usage: $0 resume|record|report|replay" >&2
    exit 2
    ;;
esac
