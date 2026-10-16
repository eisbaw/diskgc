---
id: TASK-11
title: Cargo race tests 1-9 against every supported Cargo version
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - irreversible
  - testing
  - acceptance
dependencies:
  - TASK-10
  - TASK-4
  - TASK-51
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the acceptance signal for the Cargo kind; the PRD says the Cargo kind ships only if these pass. Implement race tests 1-9 from docs/testing.md as container tests using the pinned toolchains and the deterministic helpers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 All nine tests pass on the pinned stable and nightly toolchains
- [ ] #2 Each test asserts via /proc/locks sampling and the deletion oracle, not only via diskgc's own report
- [ ] #3 If a test exposes a protocol flaw, the protocol in docs/safety.md is revised in the same change (honest failure, not a skipped test)
- [ ] #4 The tests also run with Cargo's separate build-dir and with a shared target-dir set in .cargo/config.toml
- [ ] #5 Race tests 5 and 6 use the pause/kill wrapper at seeded call positions
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
