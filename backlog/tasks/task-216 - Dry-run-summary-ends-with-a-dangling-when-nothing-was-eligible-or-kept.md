---
id: TASK-216
title: 'Dry-run summary ends with a dangling '':'' when nothing was eligible or kept'
status: To Do
assignee: []
created_date: '2026-10-25 12:10'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk walk probe (podman 5.8.7 default seccomp, release binary): `dry run: 54.3 GiB available on / ...; at least min-free already: a reclaim would take nothing (0 eligible, 0 kept) (took 1 ms):` with nothing after the colon. Drop the colon when no lines follow. Evidence: tmp/walks/2026-10-25-rc-walk-debian/evidence/default-seccomp-probe.txt.
<!-- SECTION:DESCRIPTION:END -->
