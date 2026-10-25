---
id: TASK-202
title: >-
  in_use_unknown detail blames a busy host when the clear expired because of
  slow checks after the scan
status: To Do
assignee: []
created_date: '2026-10-25 09:01'
labels:
  - shipping
  - ux
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-184's protect walk (tmp/walks/2026-10-25-t184/31-*.err): 'the host was too busy to check whether it is in use: 3 attempt(s) found no in-use scan within the freshness bound of 1000 ms; last: its clear expired before the deletion could start (the scan of 920 processes took 252 ms); kept, run again when the host is less busy'. The scan was fast; the time went to the policy re-enumeration of a 61,000-directory protect tree inside the window (TASK-200). Name what took the time between the scan and the delete (scan, policy, liveness, path checks), so the user is not told to wait for a quieter host when the cause is their config.
<!-- SECTION:DESCRIPTION:END -->
