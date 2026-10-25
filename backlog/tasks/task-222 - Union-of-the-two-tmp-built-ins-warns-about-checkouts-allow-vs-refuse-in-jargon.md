---
id: TASK-222
title: >-
  Union of the two /tmp built-ins warns about checkouts: allow vs refuse in
  jargon
status: To Do
assignee: []
created_date: '2026-10-25 13:05'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: ensure --select 'profile:pytest-tmp || profile:nix-shell-rc' /tmp warns on every run: profile `pytest-tmp` says `checkouts: allow`; in a union, a candidate also admitted by any refusing source uses `checkouts: refuse`. The two /tmp built-ins cannot match the same candidate, and a new user combining the shipped profiles cannot act on it. Warn only when the union can actually change a verdict, or say it in user terms. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/15-ensure-config-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->
