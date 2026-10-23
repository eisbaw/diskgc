---
id: TASK-112
title: >-
  Producer liveness contracts: decide when a /tmp or cache tree is provably
  unused
status: To Do
assignee: []
created_date: '2026-10-23 15:24'
labels:
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Cornerstone for the full product (Mark 2026-10-23: full product, not a Cargo-only MVP). TASK-65 found that 5 of 6 proposed /tmp built-ins (pytest-tmp, nix-shell-rc, nix-shell-tmpdir, mktemp-dir, tmp-entry) have checked-in live-session counterexamples with no open fd and no lock, and containers-image-tmp deleted an idle-but-reused spool (VM proof 2026-10-23). TASK-79 showed idle (newest mtime/ctime) cannot tell an active read-mostly tree (node_modules, .venv) from an abandoned one.

Design and implement per-producer liveness evidence that a profile can require (in the spirit of TASK-74 Chromium SingletonLock / app-not-running): e.g. owner process lifecycle markers (pid files, session ids, parent shell pid in the dir name), producer-specific lock files, the session that created it no longer existing. A profile without a verified liveness contract must not be admissible as a /tmp built-in. Document in docs/safety.md and docs/reference.md (profile schema).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A liveness-contract mechanism exists in the profile schema and core, documented in docs/reference.md and docs/safety.md
- [ ] #2 Every checked-in live-session counterexample from TASK-65 is refused (ModelHost YAML case per counterexample, proven to bite)
- [ ] #3 At least the /tmp profiles whose producers have a verifiable lifecycle are re-admitted with a contract and a dead-session positive case
- [ ] #4 Profiles whose producer has no verifiable lifecycle are explicitly excluded from built-ins with the reason recorded
<!-- AC:END -->
