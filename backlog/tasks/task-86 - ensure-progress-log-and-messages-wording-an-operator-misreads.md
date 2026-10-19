---
id: TASK-86
title: 'ensure progress log and messages: wording an operator misreads'
status: To Do
assignee: []
created_date: '2026-10-19 02:15'
updated_date: '2026-10-19 02:54'
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
- [ ] #1 Each item is changed or explicitly kept with a reason recorded here; changed log lines are covered by a test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Order (task-12 round 2 review): do the "reclaiming X before judging" item first, together with task-82 AC #4 (rounding).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
