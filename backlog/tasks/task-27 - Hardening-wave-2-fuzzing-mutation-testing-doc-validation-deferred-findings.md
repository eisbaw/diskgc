---
id: TASK-27
title: 'Hardening wave 2: fuzzing, mutation testing, doc validation, deferred findings'
status: In Progress
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-11-04T13:43:00Z'
labels:
  - wave-2
  - hardening
dependencies:
  - TASK-26
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-2 surfaces are frozen; harden them once. cargo-fuzz on the grammar parser and the YAML loader, cargo-mutants on selection and executor, doc validation of docs/reference.md and docs/safety.md, every finding deferred during wave 2.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No surviving mutants in selection semantics, or each justified
- [ ] #2 Each fuzzer (grammar, YAML loader, mountinfo, maps, environ parsers) runs 30 minutes without crashes; crashes found become regression tests
- [x] #3 fuzz and mutants recipes exist in the justfile
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: The musl target is set only in flake.nix (CARGO_BUILD_TARGET in the crane args). There is no .cargo/config.toml, and the dev shell builds for gnu. cargo-fuzz was not run in task-1. It needs a nightly toolchain, so add a separate dev shell (e.g. devShells.fuzz from a dated rust-overlay nightly) instead of changing rust-toolchain.toml. 'just fuzz' is a placeholder (prints NOT RUN, exits 0).

Forward-carried from task-1 review (mped): doc validation needs docs/ as a build input, which conflicts with crane's source filter that keeps docs out (task-1 AC4); run doc checks outside the package build (a separate check or recipe) so docs edits still do not rebuild the binary.

For the 0.1 ensure MVP (2026-10-18): only this task's mutation-testing slice on selection and the executor (AC #1, and the executor part of the description) is needed before 0.1 (task-67); fuzzing, doc validation and the other deferred findings are not. task-67 names the slice in its description rather than depending on this whole task.

2026-10-23 integration: commit 8f0ba7d added five fuzz targets, corpus seeds, docs checks, and a bounded five-source mutation sweep. Parallel QA/MPED reviews found no source blocker; nix develop -c just check and exact nix develop -c just e2e passed on the committed tree, including Docker 7/7 and VMs. Scope inventory lists 850 mutants across five files. The preflight nix develop -c tools/mutants_container_smoke.sh --check-shard parse 0/16 exited 2: MemAvailable was about 10 GiB, below its 12 GiB guard for a 10 GiB capped container. No mutation shard or 30-minute fuzzer has run, so AC1/AC2/DoD remain open. No host files deleted.

Orchestrator 2026-10-24 (COMPASS round 2): moved back to To Do: stalled on RAM, not in progress. Mark ruled 2026-10-24: run cargo-mutants in SMALLER SHARDS that fit current free RAM with the memory guard unchanged (no dedicated window). Run the 0.1 mutation slice together with TASK-13 AC3.

2026-10-31: branches task27-clean/-hardening/-integrated archived as tags archive/task27-* (75-80% of their files already identical on master; the rest older versions). The live slice is the mutation run (TASK-177).

2026-11-05: Mark chose E3 (fuzz). In progress in tmp/worktrees/t27 (branch t27-fuzz).
<!-- SECTION:NOTES:END -->
