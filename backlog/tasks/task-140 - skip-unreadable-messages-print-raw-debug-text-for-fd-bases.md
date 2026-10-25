---
id: TASK-140
title: skip unreadable messages print raw debug text for fd bases
status: To Do
assignee: []
created_date: '2026-10-24 20:48'
updated_date: '2026-10-25 11:49'
labels:
  - followup
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-130: a pre-existing diagnostic prints e.g. 'LinuxFd(OwnedFd { fd: 4 })/.' in skip unreadable messages instead of a path.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-10-25 in the TASK-156 VM walk with the 0.1 release binary: another user's 0700 /tmp dirs show 'open LinuxFd(OwnedFd { fd: 4 })/.: EACCES (13)' in the human summary. User-visible on any multi-user host; fix before 0.1.
<!-- SECTION:NOTES:END -->
