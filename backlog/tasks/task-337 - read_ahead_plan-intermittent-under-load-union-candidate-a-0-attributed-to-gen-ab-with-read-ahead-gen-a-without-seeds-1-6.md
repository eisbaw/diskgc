---
id: TASK-337
title: >-
  read_ahead_plan intermittent under load: union candidate a-0 attributed to
  gen-ab with read-ahead, gen-a without (seeds 1, 6)
status: To Do
assignee: []
created_date: '2026-11-04T17:18:00Z'
labels:
  - flaky
  - read-ahead
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-04 by the t38 implementer in nix develop -c just check (branch t38-survey, which does not touch union attribution), diskgc-linux --test read_ahead_plan, load average ~20 from parallel builds. Seeds 1 and 6: union candidate a-0 attributed to gen-ab with read-ahead and to gen-a without; size and verdict equal. Two standalone reruns and a full just check rerun passed; a passing rerun does not resolve it. Unproven hypothesis: under load gen-a's check of a-0 returned a refusal (in-use scan not settling within FRESHNESS), so the union fell through to gen-ab in one arm only. Raw log: tmp/evidence-read-ahead-flake/t38-check4-read-ahead-flake.log. Do: find the cause (is attribution order-dependent on timing, or is the test comparing timing-dependent verdicts?), fix the root cause, reproduce under stress (e.g. stress-ng load) before and after.
<!-- SECTION:DESCRIPTION:END -->
