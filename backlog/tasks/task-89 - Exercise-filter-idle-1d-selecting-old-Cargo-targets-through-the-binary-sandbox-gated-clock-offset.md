---
id: TASK-89
title: >-
  Exercise filter:idle-1d selecting old Cargo targets through the binary
  (sandbox-gated clock offset)
status: In Progress
assignee: []
created_date: '2026-10-19 02:54'
updated_date: '2026-10-22 07:03'
labels:
  - wave-2
  - testing
  - safety
dependencies: []
references:
  - docs/testing.md
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12). No test has the binary's idle filter select an old target: a target's last build is the newest mtime or ctime of its entries, a fixture cannot set a ctime back, and the shortest built-in filter is idle-1h. Refusal (not_idle) through the binary is covered; selection only by model tests and the test driver's --idle-for 1s.

Approaches ruled out (mped, round 2): time namespaces (CLOCK_REALTIME is not namespaced), libfaketime (the binary is static musl and diskgc reads the clock directly), setting the clock (needs CAP_SYS_TIME on the host; a container would change the host clock).

Proposal: a clock offset that diskgc's clock reading honours only in sandbox-gated (development) builds and only when the test container marker is present, read from a file next to the marker (so no environment variable can carry it out of a container), absent in release packages like the gate itself (release check extended). It changes an input to a deletion decision (idleness), so it gets the same review weight as the sandbox gate.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 With the offset file in the container, ensure --select 'profile:cargo-target && filter:idle-1d' reclaims a target the harness made old by the offset, and refuses a fresh one; the CI operator journey uses it for its main cases
- [ ] #2 Release packages contain no trace of the offset (release-excludes-e2e or a sibling check), and a development build outside a container ignores the file
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-22: Development LinuxHost reads a bounded decimal wall-clock offset only after sandbox markers pass; malformed files stop construction. CI operator journey uses the real hook default idle-1d selection with 1-second fresh refusal and 2-day old-target reclaim; Nix Cargo 1.90.0 and 1.99.1 focused journeys passed. Release-excludes-clock-offset checks the feature-built control against a development positive control. AC2 remains open: the default Nix package still has the pre-release gate/offset until TASK-67 switches release packaging; TASK-89 also awaits the full gate and deep reviews. A uniform offset cannot model mixed historical/fresh ctime cohorts in TASK-25.

2026-10-22 review fixes: offset reader now reads one byte past the 12-byte bound and rejects valid-prefix trailing data (including 00000000001 followed by newline and X); focused sandbox regression passes. e2e-full now includes both release exclusion checks. Shared E2E report diagnostic retains argv, exit, signal, stdout and stderr. A prior full gate had intermittent empty stdout in native Docker image-prune despite expected exit 1; 20/20 grouped fast Docker runs passed after diagnostics, with no proven cause. TASK-109 tracks investigation; do not call it fixed. Parent will rerun full e2e and deep reviews before commit.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
- [ ] #2 Reviewed with the weight of the pre-release sandbox gate (deep gate: mped plus a cross-model reviewer)
<!-- DOD:END -->
