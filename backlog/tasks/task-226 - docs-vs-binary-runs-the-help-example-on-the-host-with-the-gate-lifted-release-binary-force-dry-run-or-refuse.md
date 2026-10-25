---
id: TASK-226
title: >-
  docs-vs-binary runs the --help example on the host with the gate-lifted
  release binary; force --dry-run or refuse
status: Done
assignee: []
created_date: '2026-10-25 14:43'
updated_date: '2026-10-25 14:55'
labels:
  - safety
  - tooling
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RC QA on 72fccd8 (2026-10-25): tools/docs_vs_binary.py's '--help Start here' probe runs whatever the help example says, without adding --dry-run. Against the release artifact (sandbox gate lifted) on the host, a help example without --dry-run would be a real reclaim on the developer's machine (CLAUDE.md forbids it). Fix: on the host, refuse to run any ensure command lacking --dry-run (fail the check loudly), or inject --dry-run; reclaiming probes only with --container NAME. Test the refusal.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Host mode refuses any ensure/watch argv without --dry-run (FAILED-UNSAFE, exit 1), container mode runs it
- [x] #2 Single choke point: Runner.diskgc/host_refusal and Runner.sh (raw script naming the binary refused on host)
- [x] #3 Unit tests prove refusal on host and allowance in container; removing either guard fails a test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t226 commit 8ac818f (not merged). Release 0.1.0 host run (nix develop -c just docs-vs-binary /nix/store/c8n9cqw7pdyghqzahp760nhlq0n48lff-diskgc-release-01-candidate-0.1.0/bin/diskgc): exit 0, 171 checks, 0 mismatches, 0 refused unsafe. Three other host probes lacked --dry-run (ensure --min-fre, ensure -h --json, watch --json) and now carry it. Limitation: sh() matches the resolved binary path only.

Merged to master as 8ac818f on 2026-10-25T14:55Z after an orchestrator read-only review (GO).
<!-- SECTION:NOTES:END -->
