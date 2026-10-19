---
id: TASK-88
title: A failed write of the result to stdout panics (exit 101) and loses the report
status: To Do
assignee: []
created_date: '2026-10-19 02:54'
labels:
  - wave-2
  - safety
  - cli
dependencies: []
references:
  - crates/diskgc/src/main.rs
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in review of the CI operator journey (task-12, round 2). `finish()` prints the result with println!, which panics when the write fails. Measured with the 0.1.0 binary (dry runs, safe on the host): `diskgc ensure --dry-run --json ... >/dev/full` prints "failed printing to stdout: No space left on device (os error 28)" and exits 101; with stdout a closed pipe, "Broken pipe (os error 32)", exit 101.

ENOSPC on stdout is exactly the low-disk CI case: a pre-job hook redirects the --json report to a file on a full disk (docs/ci-pre-job.sh does), diskgc may already have deleted targets, and then the exit code (101, "a bug" per reference.md) and the report both say nothing about it. A write error must not panic: decide the exit code (e.g. the run's own code, or 3 "the report could not be written") and print one line on stderr, and say it in reference.md (Exit codes, Output). Usage-error and --version paths print with println! too.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A stdout write error (ENOSPC, EPIPE) never panics; the exit code is defined in reference.md and covered by a test (e.g. a container test writing to a full tmpfs, and a closed pipe)
- [ ] #2 Every println! on the result path is replaced or covered
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
