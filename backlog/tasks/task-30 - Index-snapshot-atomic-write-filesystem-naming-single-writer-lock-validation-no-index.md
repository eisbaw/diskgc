---
id: TASK-30
title: >-
  Index snapshot: atomic write, filesystem naming, single-writer lock,
  validation, --no-index
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-26 08:12'
labels:
  - wave-3
  - architecture
dependencies:
  - TASK-29
references:
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: ensure on a runner needs a warm index from watch. Implement the snapshot per docs/architecture.md: atomic temp+rename, index-<fs> by UUID or device number, separate never-renamed lock file, version and profiles+roots hash, one-hour freshness, mode 0600, corrupt or mismatched snapshots ignored with a log line.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A second watch for the same filesystem and user exits 2
- [ ] #2 A corrupted or other-user snapshot falls back to synchronous tally and the report says so
- [ ] #3 A profile edited between runs drops only that profile's units; changing tiers or --roots invalidates nothing (per-unit finders: profile, profile hash, root)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50's review: the snapshot's temp file plus rename (and the open with O_CREAT/O_WRONLY or O_TRUNC) is a write and a rename under the source scan (crates/diskgc-linux/src/source_scan.rs, GATED), which forbids those outside allowlisted wrappers. watch --tally-only must work anywhere, so these writes are NOT behind the sandbox gate. Decision (docs/architecture.md, The Host port): diskgc's own state writes go through ONE port operation in LinuxHost, relative to the held state-directory fd (openat2 beneath it, write the temp file, renameat over the old snapshot), added to GATED_ALLOWED as an ungated entry with its exact match count. Nothing else in the product may write or rename. The separate lock file is opened read-only for flock, as the host lock is. ModelHost needs the same operation.

From task-50 round 3 (mped), pinning the state-write signature (docs/architecture.md, The Host port):
- It takes no fd or name from the caller, only a typed key (e.g. the filesystem's index key). LinuxHost opens the state directory itself.
- The temp file is made with O_CREAT|O_EXCL, or O_TMPFILE plus linkat; never O_TRUNC.
- The rename targets only index-<fs>. ModelHost checks the same shape.
- Decide who creates index-<fs>.lock: the same state operation, or a second GATED_ALLOWED entry (Gate::Ungated with its reason, exact match count).
- Remember the scan forbids CREATE, WRONLY, TRUNC, renameat etc. outside allowlisted wrappers, and core may not touch std::fs at all.

Orchestrator 2026-10-23 (design audit): design gaps: snapshot file format (only a version field is specified); hash input says 'profiles and roots, not tiers' but required roots come from tiers; lock-file creation deferred to this task (architecture.md ~L297); whether --no-index bypasses only the snapshot or also the live index; no scenarios for corrupt/mismatched/other-user snapshots. Decide and document; add ModelHost YAML cases.

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait. Applies to the snapshot single-writer lock: a writer taking it right after another writer exits can see it busy briefly; take it with a bounded wait.

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

Mark's decisions 2026-10-25 (his own, not the emulator): ensure ALSO writes the snapshot (same atomic write and single-writer lock; skip/merge appropriately if watch holds the lock), so ensure-only setups (agent pre-job hook, CI without watch) get warmer with each call. architecture.md currently says only watch writes it: change.

Design settled 2026-10-25 in fe54fe6 (branch index-design, not merged): docs/architecture.md The index / Memory budget and collapse / Order within a tier / Refusals and index updates / The tally / reclaim walk; safety.md Discovery; testing.md index scenarios. AC review needed: #3 (no whole-file shape hash; per-unit finders (profile, profile hash, root): a changed profile drops its units, tiers and --roots invalidate nothing). ensure writes the snapshot; lock file by a second ungated state op; --no-index neither reads nor writes.

Mark's decisions 2026-10-25 (his own): an ensure whose goal is already met stays an instant no-op (no tally, no snapshot write); warmth for rarely-full machines comes from watch / watch --tally-only.

AC rewrite: design settled in fe54fe6 + 076c5ec (branch index-design); Mark's decisions 2026-10-25. Old #3 (tier changes keep the snapshot; profile or root changes invalidate it) replaced: no whole-file shape hash, per-unit finders.

From TASK-31 review (2026-10-26, mped-architect on e2bb85d): (a) Refusal.at is a MonoTime; 'this run's refusal' = at >= run_started. Persisted across reboot a monotonic time can compare as this run and suppress a unit (fails safe, but it is a format decision): store wall time + boot id, or clear hints on load. (b) fs_kind::discover_beneath asserts an absolute path; a corrupted relative path in a loaded snapshot would crash the run: validate paths at load, treat bad entries as stale or discard the snapshot.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
