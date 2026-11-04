---
id: TASK-335
title: Admit nix-garbage into the 0.2 release catalog
status: In Progress
assignee: []
created_date: '2026-11-04T15:25:00Z'
updated_date: '2026-11-04T16:02:00Z'
labels:
  - release
  - nix
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark admitted nix-garbage on 2026-11-04 ("do all four", answering: admit nix-garbage to the 0.2 catalog, then run the real GC on his laptop). Today the 0.2 row of crates/diskgc-core/src/releases.json uses catalog "0.1" (cargo-target, pytest-tmp, nix-shell-rc + idle filters).

Do: give 0.2 a catalog that adds nix-garbage (the descriptor rules of TASK-289 decide how a catalog is defined and named); move the profile's status line in docs/reference.md (Built-in profile library row, liveness table: 'in no release catalog until Mark admits it') and the 0.2 release notes draft; the release suites (e2e-runner-release, the nix GC container scenarios on the release-under-test build) must exercise nix-garbage through the gate-lifted package; docs-vs-binary; check that the release-candidate --help and profiles list show it.

Host evidence (tmp/walks/2026-11-04-dogfood/): dev-build dry run of --select profile:nix-garbage /nix/store bound the daemon (CppNix 2.34.8) and passed the root check in 0.16 s, plan = one bounded nix-store --gc --max-freed, size unknown; nix-store --gc --print-dead listed 19,536 dead paths, 68.1 GiB narSize (auto-optimise-store = false).

Not in scope: the release decision itself (TASK-276), the dry-run size estimate (a separate follow-up if wanted).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-04 orchestrator: implementer in tmp/worktrees/t335 (branch t335-admit-nix-garbage, from master c3760086).
<!-- SECTION:NOTES:END -->
