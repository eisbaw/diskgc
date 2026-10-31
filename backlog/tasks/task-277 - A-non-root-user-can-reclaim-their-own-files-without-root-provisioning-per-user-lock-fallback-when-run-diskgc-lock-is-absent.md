---
id: TASK-277
title: >-
  A non-root user can reclaim their own files without root provisioning:
  per-user lock fallback when /run/diskgc/lock is absent
status: To Do
assignee: []
created_date: '2026-10-31T17:03:00Z'
labels:
  - irreversible
  - safety
  - ux
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark's requirement (2026-10-31, while dogfooding 0.1.0 on the host): 'it shall be possible for a non-root user to run diskgc ... any user shall be able to garbage collect his own files'. Today ensure/watch exit 2 (provisioning) when /run/diskgc/lock is missing, and creating it needs root. Design: (1) a provisioned host lock is used as today (host-wide serialisation); (2) a non-root run without it uses a per-user lock created by diskgc: $XDG_RUNTIME_DIR/diskgc/lock (dir 0700, file 0600, O_NOFOLLOW, owner and mode checked), else the state directory; one log line names the lock and says reclaims by other users are not serialised with this one; (3) root / --allow-root / system services still need the provisioned host lock. Safe because the lock only serialises reclaims; every safety check is per candidate and the Cargo protocol uses Cargo's own locks; the cost (documented limit): two users reclaiming at once on one filesystem may free more than either needed. watch's tally pause and the snapshot ordering use the resolved lock. Creating the lock file is a state write (TASK-30's ungated state op pattern, exact source-scan counts). Evidence: dogfood run on the host, 0.1.0 release binary: exit 2 provisioning, nothing deleted (2026-10-31).
<!-- SECTION:DESCRIPTION:END -->
