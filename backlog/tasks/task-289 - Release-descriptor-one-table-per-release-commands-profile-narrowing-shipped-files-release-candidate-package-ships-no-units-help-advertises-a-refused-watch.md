---
id: TASK-289
title: >-
  Release descriptor: one table per release (commands, profile narrowing,
  shipped files); release-candidate package ships no units, help advertises a
  refused watch
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T14:57:00Z'
labels:
  - release
  - refactor
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 2 + mped SHOULD 5/6 + refactor 2: the NixOS module says a reclaiming instance needs a release package, but release-candidate (flake installShippedFiles) ships no diskgc-watch@.service templates, so the module's drop-ins sit on a missing unit (Codex verified with an isolated systemd-analyze fixture); its catalog refuses a reclaiming watch anyway; main.rs HELP/WATCH_SYNOPSIS advertise a reclaiming watch in every build. The release-notes draft says the index is 'not in the release catalog' though the candidate tallies and writes index files; Changes since 0.1.0 omits TASK-272 (exit 3->2), TASK-280, TASK-270, 8edb9f5 and 63c49aa (protected/negated Cargo target fixes: check 0.1.0 exposure, maybe TASK-276 advisory). Fix: a release descriptor in core (allowed commands, profile narrowing, shipped files) with one 'release-catalog' cargo feature; main.rs help, flake installShippedFiles, a NixOS module assertion that the package provides both templates, and the release notes all read from it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t289 (branch t289-release-descriptor).

2026-11-01 t289: release descriptor crates/diskgc-core/src/releases.json (core release.rs): per build commands, catalog label, shipped files; feature release-catalog replaces proposed-release-01-catalog; main.rs help/refusals, flake installs + passthru.diskgcBuild, NixOS module assertions (templates, commands), release-notes header all read it. Release notes: index wording, missing changes, 0.1.0 exposure per fix. Gates in progress.

806a763 (WIP, t289-release-descriptor): just check exit 0 (1510 Rust tests, 111 tool tests); nix flake check --no-build ok; built release-candidate-package, release-gate-needs-catalog, release-excludes-e2e/-clock-offset, deployment-units, nixos-module-eval (41 cases), nixos-module-bad-config, version-stamp, e2e-clippy, e2e-tests, workspace-tests: exit 0; release suite nix 10/10, debian 10/10; e2e-one named_checkout_policy_cannot_expand_the_release_catalog pass; docs-vs-binary dev 243/0, release-candidate 175/0 (on the tree before the last fail-closed edit). Not run: full just e2e, VMs. Awaiting mped + Codex review.
<!-- SECTION:NOTES:END -->
