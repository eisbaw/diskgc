---
id: TASK-89
title: >-
  Exercise filter:idle-1d selecting old Cargo targets through the binary
  (sandbox-gated clock offset)
status: Done
assignee:
  - '@claude'
created_date: '2026-10-19 02:54'
updated_date: '2026-10-23 15:57'
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
- [x] #2 Release packages contain no trace of the offset (release-excludes-e2e or a sibling check), and a development build outside a container ignores the file
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-22: Development LinuxHost reads a bounded decimal wall-clock offset only after sandbox markers pass; malformed files stop construction. CI operator journey uses the real hook default idle-1d selection with 1-second fresh refusal and 2-day old-target reclaim; Nix Cargo 1.90.0 and 1.99.1 focused journeys passed. Release-excludes-clock-offset checks the feature-built control against a development positive control. AC2 remains open: the default Nix package still has the pre-release gate/offset until TASK-67 switches release packaging; TASK-89 also awaits the full gate and deep reviews. A uniform offset cannot model mixed historical/fresh ctime cohorts in TASK-25.

2026-10-22 review fixes: offset reader now reads one byte past the 12-byte bound and rejects valid-prefix trailing data (including 00000000001 followed by newline and X); focused sandbox regression passes. e2e-full now includes both release exclusion checks. Shared E2E report diagnostic retains argv, exit, signal, stdout and stderr. A prior full gate had intermittent empty stdout in native Docker image-prune despite expected exit 1; 20/20 grouped fast Docker runs passed after diagnostics, with no proven cause. TASK-109 tracks investigation; do not call it fixed. Parent will rerun full e2e and deep reviews before commit.

2026-10-23 close-out. AC2 against the real candidate: diskgcRelease01 (flake.nix, sole gate-lift point, features proposed-release-01-catalog + diskgc-linux/release-without-sandbox-gate) is what release-excludes-clock-offset and release-excludes-e2e check. Strengthened diskgc-e2e/src/release_check.rs from one needle to four (offset path, 'read clock offset' context, 'must contain decimal seconds and one newline', 'test clock offset exceeded timestamp range'), each absent in the candidate and present in the development control (positive control per needle; unit test checks each needle in both directions). `nix build -L --no-link .#checks.x86_64-linux.release-excludes-clock-offset .#checks.x86_64-linux.release-excludes-e2e` exit 0 against /nix/store/kv25h5m07qs4hd7jc2m8d39px9zgzzcp-diskgc-release-01-candidate-0.1.0: 'test clock offset absent from release candidate and present in development control'. Both checks are in just e2e and e2e-full. Dev build outside a container ignores the file: sandbox.rs clock_offset_requires_both_markers_and_valid_bounded_file (in just check) asserts 0 with no markers, with containerenv only (toolbox/distrobox case, added today) and with the image marker only. Deep gate (DoD 2): mped-architect GO (both marker/offset orderings fail safe; LinuxHost::wall_clock is the only Realtime read). Cross-model reviewer codex (codex exec, read-only): no high/critical findings; three Low: (a) markers checked once at construction, so removal mid-run keeps the offset; the gate also caches its decision, documented in docs/safety.md; (b) /run/.containerenv is an existence check (pre-existing gate property, shared with the gate); (c) the release check is byte-string based and would miss an equivalent offset written without those strings, documented. Gate: just check exit 0 (952 passed, 1 ignored). Note: the default Nix package stays gated by design; only diskgcRelease01 lifts the gate.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A development LinuxHost reads a bounded decimal wall-clock offset from /.diskgc-test-clock-offset only inside a marked test container; the CI operator journey uses it to have filter:idle-1d select a 2-day-old Cargo target and refuse a fresh one through the binary. The release-without-sandbox-gate feature compiles the offset out, and the four-needle release-excludes-clock-offset check proves its absence in diskgcRelease01 against a positive control. Deep review: mped plus codex, Low findings documented.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
- [x] #2 Reviewed with the weight of the pre-release sandbox gate (deep gate: mped plus a cross-model reviewer)
<!-- DOD:END -->
