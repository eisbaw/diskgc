---
id: TASK-212
title: >-
  diskgc check exits 0 while warning that ensure refuses this configuration
  (exit 2)
status: To Do
assignee: []
created_date: '2026-10-25 12:09'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: the shipped example config unedited: `diskgc check` prints `warning: root /home/example/src does not exist on this machine ...; ensure refuses this configuration here (exit 2)` and exits 0. A CI operator who gates on `diskgc check` gets green and then a failing pre-job hook. Decide: a distinct exit code (or --strict) when ensure would refuse here; document in reference.md check. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/04-config-check.txt.
<!-- SECTION:DESCRIPTION:END -->
