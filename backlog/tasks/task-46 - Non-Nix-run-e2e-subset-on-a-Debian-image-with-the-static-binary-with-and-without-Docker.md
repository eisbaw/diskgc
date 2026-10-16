---
id: TASK-46
title: >-
  Non-Nix run: e2e subset on a Debian image with the static binary, with and
  without Docker
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
labels:
  - wave-5
  - deploy
  - testing
dependencies:
  - TASK-44
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: nothing may silently assume Nix or a particular tool. Run the e2e subset in a container from a common non-Nix distribution image with the static binary, without Nix installed, and in a variant without Docker.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->



## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The fast e2e suite passes on the Debian image with the static binary, without Nix, and in a variant without Docker; missing tools yield missing skips, never errors
<!-- AC:END -->
