---
id: TASK-143
title: >-
  Walkthrough driver and first walk of the PRD journeys on the 0.1 release
  binary
status: Done
assignee: []
created_date: '2026-10-24 20:56'
updated_date: '2026-10-24 22:14'
labels:
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
User story: any agent can bring up a realistic, confined walk environment (rootless Podman, seeded data, host-shaped process load, the 0.1 release binary on PATH) with one command and walk the PRD journeys (CI operator, laptop owner, agent harness) as a user would; kinks found are filed as tasks with pane dumps.

Deliverables: just walk-up / walk-down / walk-reset, a seeded data generator in the repo, just retro (retro table from verification notes), and the first walk with pane dumps under tmp/walks/<date>/.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 just walk-up starts tmux session walk-diskgc-<env> with a shell inside a disposable rootless container holding seeded data, process load, and the 0.1 release diskgc on PATH
- [x] #2 just walk-reset regenerates the data in place; just walk-down removes the container and session
- [x] #3 just retro prints the retro table from verification notes and tolerates notes without the keys
- [x] #4 The three PRD journeys plus help/error paths are walked; pane dumps saved; each kink filed as a shipping task
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
started: 2026-10-24T20:55:56Z. Branch walk, commit 7c2153b (driver + retro). Gate: nix develop -c just check exit 0, 1041 passed 0 failed 1 ignored, python unittest 31 OK. Walk evidence: tmp/walks/2026-10-24/ (00-45 pane dumps), release-01-candidate /nix/store/hzhjlyqpm38aia48dkaax2g59jm5k23l-diskgc-release-01-candidate-0.1.0, image localhost/diskgc-e2e-base:gg5r65ffsvv8xxav46fnhfi7i5v3rpi8. Kinks filed: TASK-144 (high), TASK-155 (high), TASK-146 (high), TASK-145 (medium after correction), TASK-147..152 (medium), TASK-153 (low), TASK-154 (walk tooling: old ctimes need a VM). Not walked: idle-1d+ tiers with real ages, /tmp sources (not in the 0.1 catalog), --allow-root, Debian/non-Nix install, upgrade. Session walk-diskgc-podman left running.

Merged to master as 7c2153b (fast-forward) on 2026-10-24T22:13Z. Walk tmux session walk-diskgc-podman left running.
<!-- SECTION:NOTES:END -->
