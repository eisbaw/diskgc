---
id: TASK-280
title: >-
  Release identity: master builds report 0.1.0 and the release-01-candidate
  package from master lifts the gate for watch
status: To Do
assignee: []
created_date: '2026-10-31T22:47:00Z'
updated_date: '2026-10-31T23:27:00Z'
labels:
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS 2026-10-31: /nix/store/cgldrwb...-diskgc-release-01-candidate-0.1.0 built from master reports 'diskgc 0.1.0' and its help lists a reclaiming watch with the deletion gate lifted, although no release includes watch (TASK-276 undecided). Evidence from master builds (incl. the TASK-277 host dogfood) cannot be told apart from the tagged 0.1.0. Fix: master version 0.2.0-dev; the candidate package from master named for the next release (or the 0.1 candidate built only from release-0.1); a git sha in --version for non-tagged builds; the release catalog/gate decision for watch made explicit (TASK-276). Also: docs/reference.md '0.1 release notes' still say 'Status: draft ... not released until TASK-67 closes' although v0.1.0 is tagged.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01 implementer (branch t280-release-identity, commit 080389d, not merged): master 0.2.0-dev; --version prints 'diskgc VERSION (COMMIT)' for a pre-release version (flake stamps self.shortRev/dirtyShortRev into the default and release-candidate packages only), plain version for a release. Renamed release-01-candidate -> release-candidate, release-01-package -> release-package, e2e-runner-release-01[-debian] -> e2e-runner-release[-debian]; catalog feature and scope unchanged. Docs: watch scope line (TASK-276 undecided), 0.1 notes released + fixed-on-master list + TASK-267/268 open. Gates: just check 0; flake check --no-build 0; checks diskgc-static, release-package, release-excludes-e2e, release-excludes-clock-offset, release-gate-needs-catalog, deployment-units, nixos-module-eval, config-check-sandbox, e2e-tests, e2e-clippy all 0; release runners nix 8/8, debian 8/8; docs-vs-binary dev 240/0 mismatches, release 174/0. Full just e2e (VM checks, fast/docker suites) not run.
<!-- SECTION:NOTES:END -->
