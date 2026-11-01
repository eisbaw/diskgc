---
id: TASK-279
title: >-
  Snapshot: the writer can produce a file the reader refuses (counter sums over
  2^62); property test flakes on it
status: To Do
assignee: []
created_date: '2026-10-31T22:27:00Z'
updated_date: '2026-11-01T04:10:00Z'
labels:
  - index
  - test
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-60 implementer (cold just check, 2026-10-31): crates/diskgc-model/tests/snapshot_props.rs no_snapshot_content_panics_the_seed_or_a_later_collapse failed once (5 reruns passed; random proptest seed). Minimal input: one single plus one block whose counters are (4611686018427387904 = 2^62, 0, 0), budget 64 MiB. The collapse adds the single to the block, so the re-captured snapshot's member sum is 2^62+1, which format::decode refuses (TASK-30 caps checked sums at 2^62). Not reachable on a real host (2^62 members), and the reader fails safe (snapshot ignored). Fix: the writer applies the same limits as the reader and skips the write with one log line (same rule as MAX_SNAPSHOT_BYTES), and the generator either stays below the cap minus one collapse step or the property asserts 'decodes, or the write was skipped'. Also check the proptest regression file is committed so the seed reruns.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
t272 branch 1b9c3dc: format::check_limits in snapshot::encode (paths and 2^62 sums, the reader's own checks); property goes through the writer; seed committed in tests/snapshot_props.proptest-regressions (also in flake workspaceTestSrc). Awaiting review round.
<!-- SECTION:NOTES:END -->
