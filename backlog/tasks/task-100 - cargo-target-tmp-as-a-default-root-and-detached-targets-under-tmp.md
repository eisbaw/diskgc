---
id: TASK-100
title: 'cargo-target: /tmp as a default root, and detached targets under /tmp'
status: Done
assignee: []
created_date: '2026-10-19 12:19'
updated_date: '2026-10-20 03:20'
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
- [x] #1 A decision on /tmp roots and on detached targets, documented in safety.md/reference.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 decision: Keep built-in cargo-target roots: required. An explicit tier with roots: [/tmp] already discovers detached temp targets; no new fixed/default root or duplicate profile. Read-only host check: nine top-level *review-target directories, signed CACHEDIR.TAG and debug/.cargo-lock, no adjacent manifest, 3.9 GiB total. Do not narrow detached rule 6: unrelated Cargo may target these through --target-dir or .cargo/config.toml without initial environment evidence, and Cargo releases build locks before run/test binaries. Docs and model regression test in isolated worktree /tmp/diskgc-task100-cargo-tmp, pending integration and full gate.

2026-10-20 integrated in 33dea88. The explicit /tmp tier example and safety decision are documented; detached rule 6 intentionally remains conservative, so the conditional AC about narrowing was removed as inapplicable. Model regression covers a /tmp detached target with unrelated Cargo running and then idle. nix develop -c just e2e passed 118/118; QA and MPED reviews GO. No host deletion.
<!-- SECTION:NOTES:END -->
