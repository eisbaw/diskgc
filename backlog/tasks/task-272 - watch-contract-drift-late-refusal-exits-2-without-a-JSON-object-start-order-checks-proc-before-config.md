---
id: TASK-272
title: >-
  watch contract drift: late refusal exits 2 without a JSON object; start order
  checks /proc before config
status: Done
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T05:25:00Z'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
t272 branch a85e259: config and --select checked (invocation::load) before the host startup in ensure and watch (bad config + broken /proc: 3 -> 2); Watch start renumbered to the code's order (kernel//proc before privileges); late Ended::Refused prints the usage-error object. Tests: binary ensure_/watch_checks_the_configuration_before_the_host_starts, a_late_refusal_prints_the_usage_error_object_and_exits_2; model a_percentage_target_below_its_minimum_told_after_start_is_refused. Awaiting review round (not Done).

2026-11-01: merged in the watch contract batch after one review round (verification note on the merge).
<!-- SECTION:NOTES:END -->
