---
id: TASK-330
title: >-
  Container tests with the real tools for the TASK-40 caches (huggingface_hub,
  cargo, node-gyp, @electron/get); Gradle and Maven as a separate low-priority
  task
status: To Do
assignee: []
created_date: '2026-11-03T13:13:00Z'
labels:
  - test
  - profiles
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-40 (merged 2026-11-03). After Mark's admission: container tests with the real producers. Gradle/Maven stay excluded (no safe offline cleaner); sccache deferred (server-aware). Also: just scratch-tally fails because du cannot read rootless overlay work/work dirs that container runs leave in worktrees (t79 seen) - make scratch-tally tolerate or the harness clean those.
<!-- SECTION:DESCRIPTION:END -->
