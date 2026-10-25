---
id: TASK-140
title: skip unreadable messages print raw debug text for fd bases
status: To Do
assignee: []
created_date: '2026-10-24 20:48'
updated_date: '2026-10-25 12:14'
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

2026-10-25 rc-walk branch 80c858c (not merged): LinuxFd carries the path it was opened by (messages only) and Displays it; every fd subject in LinuxHost and /proc reads use paths. Before: 'cannot list it (fail closed): open LinuxFd(OwnedFd { fd: 4 })/.: EACCES (13)'. After (release-01-candidate at 80c858c, Debian walk dump tmp/walks/2026-10-25-rc-walk-debian/dumps/07-dry-config-tmp.txt): 'cannot list it (fail closed): open /tmp/nix-shell.Ab12Cd: EACCES (13)'. Also fixed Debug leaks: Identity in changed details, Unknown idle age, not_owner owners list ('not by uid 1001'), FileType, host-lock signal. Human summary groups unreadable/not_owner from two. Pinned by host test the_engine_walks_here_but_its_delete_is_refused_by_the_gate (bites), engine/report unit tests. Gate just check EXIT 0; release-01-debian runner 8/8.
<!-- SECTION:NOTES:END -->
