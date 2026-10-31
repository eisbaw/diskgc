---
id: TASK-272
title: >-
  watch contract drift: late refusal exits 2 without a JSON object; start order
  checks /proc before config
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - watch
  - contract
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 10+14: crates/diskgc/src/watch.rs on Ended::Refused exits 2 after start with no JSON object under --json, while reference.md says 2 is refused at start and a start failure prints an object: emit the usage-error object through refused() and add the late case to the exit-code table. watch and ensure run startup_in (kernel, /proc) before read_config, while reference.md Watch start checks config first: with a bad config and a broken /proc they exit 3 where the doc implies 2; read config first or fix the doc.
<!-- SECTION:DESCRIPTION:END -->
