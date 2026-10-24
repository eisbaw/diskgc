---
id: TASK-151
title: 'Missing host lock: no fix-it command, and JSON cause ''usage'' like a typo'
status: To Do
assignee: []
created_date: '2026-10-24 21:10'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/32-no-host-lock.txt; /run/diskgc moved aside, i.e. tmpfiles not applied). The error prints the two tmpfiles lines but not how to apply them now (e.g. 'sudo systemd-tmpfiles --create /path/to/diskgc.conf', and where the package installed the drop-in: lib/tmpfiles.d/diskgc.conf). Under --json the error has cause 'usage' and status usage_error, the same as a mistyped flag, so a harness cannot tell 'the host is not provisioned' (alert ops) from 'my call is wrong' without parsing message text. reference.md documents exit 2/usage_error for it; the cause could be 'host' (contract question, CLI surface is irreversible: decide before 0.1).
<!-- SECTION:DESCRIPTION:END -->
