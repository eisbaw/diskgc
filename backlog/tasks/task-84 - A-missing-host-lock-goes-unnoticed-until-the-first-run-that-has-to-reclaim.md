---
id: TASK-84
title: A missing host lock goes unnoticed until the first run that has to reclaim
status: Done
assignee: []
created_date: '2026-10-19 02:14'
updated_date: '2026-10-20 06:13'
labels:
  - cli
  - deployment
  - wave-2
dependencies: []
references:
  - docs/safety.md
  - docs/ci-pre-job.sh
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12). ensure looks for /run/diskgc/lock only when it must reclaim (reference.md: above --min-free it "exits 0, without taking the lock"), and exits 2 with the tmpfiles lines only then. A runner provisioned without the tmpfiles drop-in therefore passes every pre-job hook silently, in human and JSON mode, until the first job that runs low on disk, which then fails with exit 2: the worst moment to learn about a setup mistake. The example hook (docs/ci-pre-job.sh) works around it by testing the file itself; every operator would have to know to do that. `check` cannot help, by design (machine-independent).

Options: a stderr warning (and a JSON `errors`-like note, or a field) from every ensure when the lock file is missing or not a regular file, without taking it, so exit 0 stays exit 0; or a host-readiness subcommand. Keep "exits 0 without taking the lock" intact.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 An ensure whose goal is already met reports a missing or unusable host lock (stderr at least), still exits 0 and takes no lock
- [x] #2 reference.md says where the warning appears; a container test covers it and the example hook's own check can be dropped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Raised to high (task-12 round 2): this breaks the PRD's "fail loudly and early" promise for the main 0.1 user, the CI operator: a runner provisioned without the drop-in passes every pre-job hook until the first low-disk job fails with exit 2. task-67 (0.1 release) now depends on it.

2026-10-20 implementation in isolated /tmp/diskgc-task84-lock-warning: hostlock::readiness shares acquire’s safe no-follow resolve and regular-file reopen but never flocks; an already-met ensure warns on stderr for missing/FIFO lock while exiting 0 with met JSON. Command-missing met branch also warns. Model covers missing/FIFO/ready without flock. Native disposable-container E2E covers missing/FIFO, and CI journey passes with the hook’s duplicate lock check removed. just check and both focused E2E cases passed. Full just e2e and parallel QA/MPED remain before commit; no real host deletion.

2026-10-20 verification update: Full just e2e passed 128/128 Nix-image and 5/5 native Docker cases. QA and MPED reviews are pending before commit.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Already-met ensure now checks whether the configured host lock can be safely opened; missing or unusable lock emits a stderr warning and returns met/exit 0 without claiming the lock. A reclaim still fails closed with exit 2. Removed duplicate CI hook precheck. ModelHost cases cover missing, FIFO, and ready lock without flock; disposable CLI and CI journey E2E cover JSON outcome and deletion oracle. Full nix develop -c just e2e passed: 128/128 Nix-image E2E plus 5/5 native Docker E2E.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
