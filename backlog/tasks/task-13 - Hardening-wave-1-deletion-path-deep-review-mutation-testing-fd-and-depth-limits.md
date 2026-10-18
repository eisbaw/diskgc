---
id: TASK-13
title: >-
  Hardening wave 1: deletion path deep review, mutation testing, fd and depth
  limits
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 07:54'
labels:
  - wave-1
  - hardening
dependencies:
  - TASK-12
  - TASK-58
  - TASK-60
  - TASK-64
references:
  - docs/testing.md
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the wave-1 surfaces (host primitives, deletion engine, in-use checks, Cargo protocol) are now stable and are the hardest to walk back. Deep review (mped-architect plus a safety reviewer), cargo-mutants on the safety and executor modules (a surviving mutant in a safety check is a test bug), deep/huge tree limits, doc sync of docs/safety.md, plus every finding deferred during wave 1.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Deep review findings fixed or filed with an owner
- [ ] #2 docs/safety.md matches the implemented protocols (reviewed line by line against the code)
- [ ] #3 cargo-mutants (workspace tests) on diskgc-core and on diskgc-linux path resolution, mountinfo and /proc code: no surviving mutants in safety checks, or each justified; every safety check has a model test
- [ ] #4 Re-plan gate: if the race tests changed the Cargo protocol, task-9 and task-53 are re-reviewed under the deep gate and tasks 16, 22 and 33 re-checked; if the race tests cannot pass, cargo-target stays planned and the journey is rescoped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: 'just mutants' is a placeholder (prints NOT RUN, exits 0). Replace it with the cargo-mutants invocation.

Dependencies changed for the 0.1 ensure MVP (2026-10-18): task-52 (CI) removed, because the human decided e2e-full runs locally for now (just e2e-full on the development machine is the evidence until CI exists). task-61 removed: it is split; its real-machine and safety items moved to task-64, which this task now depends on, and the rest is deferred to wave 5.

Filed from task-10's deep gate (Codex, mped, QA; file, do not fix in task-10): (H1) refusal messages name task IDs ('not supported ... (task-16)'): user-facing text should not cite backlog IDs once released; reword before 0.1. (H2) config lookup with a dangling symlink at the user path: read_config_file gets ENOENT and the search falls back to /etc, while a dangling user config is arguably 'exists but unreadable' (exit 2, no fallback); decide and test. (H3) deadline start point: Request.timeout is turned into a deadline when the executor starts, after startup, config and selection; the contract says 'for the whole call'; start it at process start or document. (H4) restrict Host::inherit_lock to the host lock (today any Locked can be made inheritable): e.g. a HostLock-only method or a sealed type. (H5) btrfs subvolumes: other_fs compares statx devices, and every subvolume has its own anonymous device although they share one pool's space: targets on a sibling subvolume are skipped other_fs; decide (statfs f_fsid?) and test on the model. (H6) a PATH with a symlink on the way (Fedora Silverblue /home -> /var/home) is refused as unsafe_path with the generic resolve message; say which component is a symlink and how to name the real path. (H7) switch CLI parsing from the hand-rolled parser in crates/diskgc/src/main.rs to lexopt (a reviewed production dependency, PRODUCTION_DEPENDENCIES) once the flag set grows (task-21).

Filed from task-10's deep gate round 2: (H8) docs/testing.md says the other binary tests stop at usage errors before the machine is read, but crates/diskgc/tests/cli.rs diskgc() isolates only DISKGC_CONFIG and XDG_CONFIG_HOME: tests without --config still look up /etc/diskgc/config.yaml on the host (a present, invalid file there changes their outcome); pass a config of the test's own everywhere, or document the dependency. (H9) the tally does no idle check, so a target that is not idle is locked (every Cargo lock of its profiles) and walked under the host lock before reclaim skips it as not_idle: a Cargo build starting in that window waits; consider an idle pre-filter from the tally's last build before locking (the protocol still rechecks under the locks).

(H10) From task-10 round 4 (mped nit 3): resolve every root before discovery walks any of them, so a misconfigured root (unsafe_path) excludes the tier before any other root's walk is spent; today cargo_target::discover walks roots outermost first and refuses an unsafe one when it reaches it.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
