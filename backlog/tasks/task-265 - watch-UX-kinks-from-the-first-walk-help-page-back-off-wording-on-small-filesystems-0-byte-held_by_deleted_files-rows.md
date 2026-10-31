---
id: TASK-265
title: >-
  watch UX kinks from the first walk: help page, back-off wording on small
  filesystems, 0-byte held_by_deleted_files rows
status: To Do
assignee: []
created_date: '2026-10-31T03:31:00Z'
labels:
  - watch
  - ux
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Walk 2026-10-31 (tmp/walks/2026-10-31-watch/): (1) 'diskgc watch --help' prints the global help, which starts with ensure examples; a watch user wants the watch synopsis, thresholds/interval/back-off and signals first. (2) The hold back-off line reads 'the next cycle starts not on a drop (the 465.1 MiB available cannot fall by the step of 1.0 GiB)' on a 1 GiB tmpfs: awkward word order, and on small filesystems the drop rule can never fire; say 'no drop rule here (step 1.0 GiB exceeds what is available); next cycle at the first sample below --min-free after 900 s'. (3) The cycle record's held_by_deleted_files lists a process holding a deleted file of 0 bytes ({pid 2913, python3.14, bytes 0}); decide whether 0-byte holders are reported (they free nothing).
<!-- SECTION:DESCRIPTION:END -->
