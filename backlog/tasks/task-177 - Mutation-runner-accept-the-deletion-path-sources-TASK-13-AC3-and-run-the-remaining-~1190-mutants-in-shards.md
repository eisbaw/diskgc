---
id: TASK-177
title: >-
  Mutation runner: accept the deletion-path sources (TASK-13 AC3) and run the
  remaining ~1190 mutants in shards
status: To Do
assignee: []
created_date: 2026-10-25 03:24
updated_date: '2026-11-02T14:15:00Z'
labels:
  - hardening
  - mutation
dependencies: []
references:
  - tools/mutation_scope.py
  - tools/mutants_container_smoke.sh
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-13 async QA (2026-10-25, b009e59). tools/mutation_scope.py and tools/mutants_container_smoke.sh only accept the five select/executor sources (TASK-27 scope). TASK-13 AC #3 needs the deletion path. QA ran it with a scratch copy of the container runner (same image, mounts, 10 GiB cap, no swap, 12 GiB MemAvailable start guard, 4 GiB stop floor, 16 GiB disk floor; only the --file choice and a log dir in the main checkout's tmp/qa13-mutants/ differed). That copy is not in the repo.
Measured cost (container, --test-workspace=true --jobs 1): baseline 96-102 s build + 116-138 s test; about 50 s to 2 min per mutant (delete.rs 13 in 24 min, 12 in 10 min; hostlock.rs 14 in 10 min, mostly unviable).
Inventory per file at b009e59 (cargo mutants --list): engine/delete 25, frames 52, mod 70, walk 57; fs_kind/judge 239, reclaim 30, liveness 56; cargo_target/eligible 108, reclaim 62; inuse/environ 18, mod 131, scan 150; liveness 54, liveness/pytest 114; hostlock 14; resolve 48 (1228 in total, about 25-40 h of container time). diskgc-linux path resolution, mountinfo and /proc code are not counted.
Done: engine/delete.rs (25: 21 caught, 4 unviable, 0 missed) and hostlock.rs (14: 4 caught, 10 unviable, 0 missed). engine/walk.rs shard 0/4: the baseline passed, then the run was stopped by the 4 GiB RAM floor before any mutant outcome.
The runner's partition check (verify_mutant_sweep.py) is tied to the five SOURCES.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 mutation_scope/runner accept the deletion-path files in a second named scope, with the guards unchanged
- [ ] #2 Every deletion-path file swept; each missed mutant classified (equivalent with reason / test gap filed / weakness filed)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02 (TASK-315): the 0.2 release criteria (docs/architecture.md, '0.2 release criteria') propose this task's AC2 as the release bar for the deletion path (every source swept, no unclassified survivor), pending Mark; 0.1 ran it async, blocking only on a found hole. The inventory above predates modules such as inuse/loops.rs and engine/overlay.rs.
<!-- SECTION:NOTES:END -->
