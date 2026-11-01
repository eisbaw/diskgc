---
id: TASK-280
title: >-
  Release identity: master builds report 0.1.0 and the release-01-candidate
  package from master lifts the gate for watch
status: Done
assignee: []
created_date: '2026-10-31T22:47:00Z'
updated_date: '2026-11-01T00:30:00Z'
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

2026-11-01 review round (branch t280-release-identity, rebased on master e62afd4): commits 253b8ce (S1 unstamped tested builds via mkDiskgc, version-stamp check; S4a release catalog refuses a reclaiming watch; S3 'Release notes (next release, draft)'; S2 architecture.md Release procedure + just release-verify; PRD gate bullet), 5157ce9 (awk -v fix for the notes title; caught by just e2e, not by flake check --no-build). Gates on 5157ce9: nix develop -c just e2e exit 0 (fast 237/237, release nix 9/9 and debian 9/9 incl. release_candidate_refuses_a_reclaiming_watch, docker 7/7, VM checks incl. vm-release-tmp-reclaim); docs-vs-binary dev 240/0, release-candidate 174/0. Tested-consumer drvPaths identical across a commit; only version-stamp and the shipped packages change.

2026-11-01: merged (ff) after one review round; full just e2e exit 0 on 5157ce9. Open for TASK-276: draft notes body still describes 0.1's scope; release --help lists the refused reclaiming watch; release-verify is manual at tag time.
<!-- SECTION:NOTES:END -->
