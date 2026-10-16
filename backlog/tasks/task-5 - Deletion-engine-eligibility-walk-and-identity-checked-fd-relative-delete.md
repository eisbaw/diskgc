---
id: TASK-5
title: 'Deletion engine: eligibility walk and identity-checked fd-relative delete'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:52'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-3
  - TASK-14
  - TASK-51
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: this is the code that removes files. Implement the eligibility walk (ownership of every entry, .diskgc-keep anywhere, the Cargo structural recogniser, sockets/FIFOs, mounts) and the delete: reopen each directory relative to its parent and match (dev, inode), fstatat identity+owner check before every unlinkat, make_writable after the checks, ENOTEMPTY reported. See docs/safety.md#deletion.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A mixed-owner tree is skipped as not_owner; a nested .diskgc-keep or Cargo target makes the candidate kept / contains_special (container tests)
- [ ] #2 A 100k-entry, 30-level-deep tree is deleted without exceeding a low RLIMIT_NOFILE (e.g. 64)
- [ ] #3 The deletion oracle shows nothing outside the candidate disappeared in every engine test
- [ ] #4 An entry replaced between walk and unlink is never deleted and the candidate is changed (ModelHost event test, plus one container test using the pause wrapper)
- [ ] #5 A mode-000 directory inside a candidate makes it ineligible (fail closed), never treated as empty
- [ ] #6 A single directory with 200,000 entries and a tree deeper than PATH_MAX are deleted completely by a non-recursive walk
- [ ] #7 A mount appearing inside a candidate mid-delete stops the delete there (EXDEV) and marks the candidate changed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
