---
id: TASK-67
title: '0.1 release: lift the sandbox gate in the release package (ensure MVP)'
status: To Do
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-19 02:54'
labels:
  - wave-2
  - release
  - irreversible
  - safety
dependencies:
  - TASK-26
  - TASK-21
  - TASK-22
  - TASK-55
  - TASK-11
  - TASK-65
  - TASK-66
  - TASK-63
  - TASK-64
  - TASK-84
  - TASK-88
references:
  - PRD.md
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the human approved a scoped MVP release, "diskgc 0.1, the ensure MVP" (PRD.md, Scope, "Releases"; docs/safety.md, Pre-release sandbox gate). It is the first build that may delete on a real machine, so it is irreversible for whoever runs it.

What:
- Build the release package with the release-without-sandbox-gate feature set of diskgc-linux in exactly one place (e.g. the diskgc package's cargoExtraArgs in flake.nix). The e2e binaries, the test driver and every check stay gated.
- Update the test sandbox_gate_flag_is_enabled_nowhere (crates/diskgc-linux/src/sandbox.rs) to allow exactly that place (it must also scan nested .cargo/config files, task-64 group K). Consider a flake check that the non-release package links the marker path /.diskgc-test-sandbox and the release package does not (task-49 note).
- Deep review (QA, mped-architect, Codex) of the release scope against PRD.md's 0.1 scope.
- Release notes stating the honest limits (PRD.md, docs/safety.md) and what is out of scope (watch, the index, survey, Docker, Nix, command actions).

Evidence, by task: Cargo race tests 1-9 on every pinned toolchain (task-11); journeys for the CI operator (task-12, and on Debian with the static binary, task-63), the /tmp laptop owner (task-22) and the agent harness (task-55); ensure's full CLI contract (task-21); the contract freeze review (task-26); wave-1 hardening (task-13, task-64); the /tmp built-ins (task-65); packaging (task-66). Wave-2 hardening for 0.1 is only task-27's mutation-testing slice on selection and the executor: that slice must be green before this task closes, but the rest of task-27 (fuzzing, doc validation, other deferred findings) is not a dependency.

The v1 release (task-49) builds on this one.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The release package is the only build with the gate flag, enforced by a test (sandbox_gate_flag_is_enabled_nowhere allows exactly one place)
- [ ] #2 All MVP evidence is green: Cargo race tests 1-9 on every pinned toolchain; the journeys for the CI operator, the /tmp laptop owner and the agent harness; the contract freeze review (task-26); wave-1 and wave-2 hardening (task-13, task-64, and task-27's mutation-testing slice on selection and the executor)
- [ ] #3 Release notes list the limits and the deferred scope
- [ ] #4 A dry-run on a real machine is reviewed before the first real run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
