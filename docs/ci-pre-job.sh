#!/bin/sh
# Example pre-job hook for a persistent CI runner (PRD, Users: the CI
# runner fleet operator). Before each job it makes sure the filesystem of
# the builds directory has room, reclaiming Cargo targets below it with
# `diskgc ensure` (docs/reference.md: `ensure`, Exit codes, Output).
#
# Exercised end to end, as written, by the container test
# journey_ci_operator_frees_space_with_ensure
# (diskgc-e2e/src/journey_suite.rs).
#
# Settings (environment):
#   CI_BUILDS_DIR       the builds directory, an absolute path (required):
#                       where Cargo targets are looked for, and the
#                       filesystem that must have the space
#   DISKGC_MIN_FREE     space the next job needs available (default 50G)
#   DISKGC_TARGET_FREE  once below DISKGC_MIN_FREE, reclaim up to this
#                       (default: DISKGC_MIN_FREE)
#   DISKGC_SELECT       what may be reclaimed (default: Cargo targets whose
#                       last build is a day old)
#   DISKGC_TIMEOUT      deadline for the whole call (default 10m)
#   DISKGC_REPORT       where the JSON report is kept, e.g. for a dashboard
#                       (default: a new file from mktemp in ${TMPDIR:-/tmp},
#                       never a fixed name in a shared directory, which
#                       another job or user could hold or replace)
#
# Exit: 0 when the job may start; otherwise diskgc's exit code, so the job
# fails before it starts rather than half-way through, out of disk; 2 for
# a misconfigured hook or runner; 3 when diskgc left no report although it
# exited 0. diskgc's progress lines (one per reclaimed or skipped target)
# go to stderr, i.e. to the job log.

set -u

builds=${CI_BUILDS_DIR:-}
case $builds in
/*) ;;
*)
    echo "pre-job: CI_BUILDS_DIR must name the builds directory as an absolute path, got '$builds'" >&2
    exit 2
    ;;
esac
min_free=${DISKGC_MIN_FREE:-50G}
target_free=${DISKGC_TARGET_FREE:-$min_free}
select=${DISKGC_SELECT:-'profile:cargo-target && filter:idle-1d'}
timeout=${DISKGC_TIMEOUT:-10m}
if [ -n "${DISKGC_REPORT:-}" ]; then
    report=$DISKGC_REPORT
elif ! report=$(mktemp "${TMPDIR:-/tmp}/diskgc-pre-job.XXXXXX"); then
    echo "pre-job: cannot make a report file in ${TMPDIR:-/tmp}" >&2
    exit 2
fi

# The host lock is provisioned at boot (the tmpfiles.d drop-in). diskgc
# looks for it only when it has to reclaim, so a runner set up without it
# would find out on the first job that runs low on disk. Check every time.
if [ ! -f /run/diskgc/lock ]; then
    echo "pre-job: /run/diskgc/lock is missing; provision it at boot (tmpfiles.d):" >&2
    echo "pre-job:   d /run/diskgc      0755 root root -" >&2
    echo "pre-job:   f /run/diskgc/lock 0644 root root -" >&2
    exit 2
fi

# The report must be writable before diskgc runs: a redirection that fails
# would skip diskgc and look like one of its exit codes. (A subshell, since
# a redirection error on `:` may end a POSIX shell before `||` runs.)
if ! (: >"$report") 2>/dev/null; then
    echo "pre-job: cannot write the report $report" >&2
    exit 2
fi

# A fresh runner has no builds directory before its first job; diskgc
# refuses a PATH that does not exist (exit 2), so make it.
mkdir -p "$builds" || exit 2

started=$(date +%s)
diskgc ensure --json \
    --min-free "$min_free" --target-free "$target_free" --timeout "$timeout" \
    --select "$select" --roots "$builds" \
    "$builds" >"$report"
code=$?
took=$(($(date +%s) - started))

# --json always prints one object, so no report (no regular file, or an
# empty one) means diskgc did not run or could not write it.
if [ ! -f "$report" ] || [ ! -s "$report" ]; then
    if [ "$code" -eq 127 ] || [ "$code" -eq 126 ]; then
        echo "pre-job: diskgc could not be started (exit $code): is it installed and on PATH?" >&2
        exit "$code"
    fi
    echo "pre-job: diskgc ensure exited $code without writing its report to $report" >&2
    [ "$code" -ne 0 ] || code=3
    exit "$code"
fi

# One top-level scalar of the report (a one-line JSON object), without jq:
# the greedy match returns the key's LAST occurrence in the line, which is
# the top-level one only for keys no nested object carries (true of those
# used below). Where jq is installed, prefer `jq -r .KEY "$report"`.
field() {
    sed -n -E "s/.*\"$1\":(\"[^\"]*\"|[^,}]*).*/\1/p" "$report" | tr -d '"'
}
# How often a text occurs in the report.
count() {
    grep -o "$1" "$report" | wc -l | tr -d ' '
}

# Reclaimed: `actions` entries (only they carry avail_delta_bytes). Kept:
# `skipped` entries (only they carry examples), less those an earlier run
# already reclaimed (`empty`). These count entries, not candidates: once
# diskgc aggregates many skips of one reason into one entry with a `count`
# (docs/reference.md, Output), they undercount; with jq, sum `.count`.
reclaimed=$(count '"avail_delta_bytes":')
empty=$(count '"reason":"empty"')
kept=$(($(count '"examples":') - empty))
echo "pre-job: diskgc ensure exited $code after ${took}s: status $(field status)," \
    "$(field avail_before_bytes) -> $(field avail_after_bytes) bytes available" \
    "(min-free $(field min_free_bytes), target met: $(field target_met));" \
    "$reclaimed target(s) reclaimed, $kept kept, $empty already empty;" \
    "report in $report"

case $code in
0) exit 0 ;;
1) echo "pre-job: not enough space even after reclaiming every eligible target; kept targets are in the log above" >&2 ;;
2) echo "pre-job: diskgc refused to run (usage, configuration or provisioning); see its error above" >&2 ;;
3) echo "pre-job: a reclaim failed; see the errors in the log above and in $report" >&2 ;;
75) echo "pre-job: another reclaim held the host lock until the deadline; retry the job later" >&2 ;;
124) echo "pre-job: the deadline ($timeout) passed while reclaiming" >&2 ;;
130 | 143) echo "pre-job: diskgc was interrupted (exit $code); the job is not started" >&2 ;;
*) echo "pre-job: unexpected exit code $code from diskgc" >&2 ;;
esac
exit "$code"
