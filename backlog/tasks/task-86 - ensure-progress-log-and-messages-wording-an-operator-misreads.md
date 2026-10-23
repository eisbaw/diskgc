---
id: TASK-86
title: 'ensure progress log and messages: wording an operator misreads'
status: Done
assignee: []
created_date: '2026-10-19 02:15'
updated_date: '2026-10-23 07:30'
labels:
  - cli
  - ux
  - wave-2
dependencies: []
references:
  - diskgc-e2e/src/journey_suite.rs
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12), reading the stderr job log literally. Each item is small; none changes what is deleted.
- "--select: reclaiming /tmp/builds/tests-running/target (est. 17.6 MiB, 141 entries, last build 1792375119)" is printed before the target is judged, and the next line is "skip in_use: ...": read alone, the first line says the running target was reclaimed. Say "trying" / "checking", or print one line per outcome.
- "last build 1792375119" is raw epoch seconds; an operator wants an age ("last build 3 d ago") or a date.
- An in_use skip is one line of 8 pieces of evidence (about 1,500 characters for one target); the first piece or two plus a count would do on stderr, the rest in --json (see the dashboard task) or a verbose flag.
- "skip locked: .../.cargo-build-lock: held by another process (a Cargo building, checking, running or testing this target)" does not name the holder, while in_use names pids; /proc/locks gives the pid.
- "profile `cargo-target` says `checkouts: allow`: ..." is printed on every run of the built-in profile, i.e. every CI job; a warning that never changes is noise in job logs (once per user profile in `check` would do: task-15).
- The tier of --select is named "--select" in every line ("--select: skip in_use"), which reads like a complaint about the flag.
- Usage errors: "--select `profile:cargo-target`: `cargo-target` has `roots: required`, and no roots were given" does not say "add --roots DIR"; "--min-free 16546754560 bytes: ... more than the filesystem's size (16534298624 bytes)" echoes bytes, not what was typed (16158940K), and no human size.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Each item is changed or explicitly kept with a reason recorded here; changed log lines are covered by a test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Order (task-12 round 2 review): do the "reclaiming X before judging" item first, together with task-82 AC #4 (rounding).

2026-10-23 source-only checkpoint in repo-local task86-ux worktree: progress says checking before outcome, candidate age is relative, in_use stderr shows two evidence pieces plus a count while JSON retains full detail, selected tier prints selection in CLI progress, missing roots names --roots DIR, and oversized sizes include human units. Focused Rust and container E2E assertions added. Formatting and diff checks passed; compilation, exact just e2e, QA/MPED review, lock-holder PID, and repeated checkout-warning policy remain open. No host deletion or commit.

2026-10-23 followup: built-in cargo-target checkout warning is now suppressed in each run; check emits one warning per operator profile with checkouts: allow. Existing ModelHost test expectations updated; focused diskgc-core operator_message_tests passed 4/4 before this latest model-test edit. CLI/check and journey tests remain unrun because free space fell to ~17.5 GiB. Lock-holder PID and original min-free spelling are not yet addressed. AC/DoD remain unchecked.

2026-10-23 review correction: QA/MPED found duplicate check warnings, repeated executor warnings, and a false promise that all in_use evidence survives JSON aggregation. Source patch now uses existing config warnings (stderr) once per configured allowance, removes per-tier non-union warning, and labels omitted stderr evidence without promising JSON retention. CLI regression checks both user and explicit built-in policies. Lock-holder PID and exact --min-free spelling remain open; tests beyond formatting/diff still blocked by disk reserve. Earlier checkpoint note about JSON retaining full detail was too broad: report aggregation drops detail above 20 same-group skips.

TASK-86 explicit keep decisions for AC #1: Lock-holder PID is omitted from the busy-flock message. A separate /proc/locks read after flock_try returns Busy is an advisory, racing observation and can name a process that no longer holds this lock; the current Host abstraction cannot sample it atomically with the failed flock. Keep the precise locked reason without a potentially false PID. Exact lexical --min-free spelling is also omitted: parse_size canonicalizes the value to Size and the core Request carries only that value through filesystem measurement. Preserving the spelling requires extending the request data model and all callers, beyond a wording-only task. This patch adds a human-readable binary size alongside bytes; the CLI message remains accurate while the original spelling is not retained.

2026-10-23 review followup: replaced semicolon-splitting evidence count with a bounded character preview and an honest omitted-detail suffix, because process arguments can contain semicolon-space. Added a ModelHost regression that asserts the changed filesystem and filesystem-union checking lines before their successful outcomes. Four core message tests and the new model test passed in a disposable tmpfs Podman container as host UID. Prior full core/model suites and 29 CLI tests passed on the preceding patch state; exact just e2e remains mandatory before commit. The old checkpoint language about two evidence pieces plus a count is superseded by the bounded preview.

2026-10-23 final gate: exact nix develop -c just e2e passed on the task86-ux branch based on master 8483070, including formatting, locked metadata, Clippy, workspace tests, release/static checks, Nix and Docker container journeys, and historical-ctime plus pinned-kernel VM checks. Parallel QA and MPED source reviews found no blocker. Lock-holder PID and exact min-free spelling remain explicitly kept with reasons above; all changed messages have regression coverage. No developer-host diskgc run or host deletion.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
