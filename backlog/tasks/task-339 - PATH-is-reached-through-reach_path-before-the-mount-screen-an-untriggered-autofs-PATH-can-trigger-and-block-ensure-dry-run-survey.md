---
id: TASK-339
title: >-
  PATH is reached through reach_path before the mount screen: an untriggered
  autofs PATH can trigger and block (ensure, dry run, survey)
status: To Do
assignee: []
created_date: '2026-11-04T17:18:00Z'
labels:
  - autofs
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of survey (2026-11-04), verified by the t38 implementer to affect ensure --dry-run on master: executor::reach_path opens PATH (O_PATH|O_DIRECTORY) deliberately without the mount screen, so an untriggered automount PATH is triggered and can hang on a dead NFS server; candidate screening (TASK-180) comes later. Decide: screen PATH before resolution (refuse an untriggered automount PATH with a clear message) or document that naming an automount PATH triggers it.
<!-- SECTION:DESCRIPTION:END -->
