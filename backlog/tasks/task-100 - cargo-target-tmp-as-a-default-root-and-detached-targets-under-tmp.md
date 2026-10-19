---
id: TASK-100
title: 'cargo-target: /tmp as a default root, and detached targets under /tmp'
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
labels:
  - profiles
dependencies:
  - TASK-65
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: ~4.9 GiB of Cargo targets sit in /tmp/*review-target (CACHEDIR.TAG present, made by agents with CARGO_TARGET_DIR). The tally never gave cargo-target /tmp as a root, and these are detached targets (no Cargo.toml attached), which are refused whenever any cargo process runs (in-use rule 6). Decide whether cargo-target gets /tmp as a fixed root in a /tmp tier, and whether rule 6 can be narrowed for old detached targets. Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A decision on /tmp roots and on detached targets, documented in safety.md/reference.md
- [ ] #2 If narrowed: model tests show a detached target is refused while a cargo uses it and taken when idle with unrelated cargo processes running
<!-- AC:END -->
