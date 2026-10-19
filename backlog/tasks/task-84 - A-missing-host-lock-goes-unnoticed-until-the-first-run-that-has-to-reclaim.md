---
id: TASK-84
title: A missing host lock goes unnoticed until the first run that has to reclaim
status: To Do
assignee: []
created_date: '2026-10-19 02:14'
updated_date: '2026-10-19 02:54'
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
- [ ] #1 An ensure whose goal is already met reports a missing or unusable host lock (stderr at least), still exits 0 and takes no lock
- [ ] #2 reference.md says where the warning appears; a container test covers it and the example hook's own check can be dropped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Raised to high (task-12 round 2): this breaks the PRD's "fail loudly and early" promise for the main 0.1 user, the CI operator: a runner provisioned without the drop-in passes every pre-job hook until the first low-disk job fails with exit 2. task-67 (0.1 release) now depends on it.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
