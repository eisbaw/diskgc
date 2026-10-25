---
id: TASK-213
title: >-
  A Cargo target with only lock files left is in the discovery count but in
  neither JSON nor summary
status: To Do
assignee: []
created_date: '2026-10-25 12:09'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: after a reclaim left ~/src/lib/target with only lock files, `ensure --json ... --roots ~/src` logs `3 target(s) below /home/tester/src` but the JSON skipped/actions list two. reference.md says the human summary omits such targets and that --json lists every candidate. Either list it in JSON (e.g. reason empty) or document the omission for JSON too, and make the log count agree. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/11b-json-exhausted.txt.
<!-- SECTION:DESCRIPTION:END -->
