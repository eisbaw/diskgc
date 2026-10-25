---
id: TASK-235
title: 'Tally: 100,000-entry no-starvation model run of the background tally'
status: To Do
assignee: []
created_date: '2026-10-25 21:55'
labels:
  - index
  - tally
dependencies:
  - TASK-29
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-29: docs/testing.md asks for the background tally's discovery, re-check and re-tally jobs all served within twice their period on a 100,000-entry model under constant churn. TASK-29 tested it scaled down (background_serves_every_kind_of_job_within_twice_its_period, ~200 units / 600 entries over 150 virtual minutes), and with capacity to spare, so it cannot show starvation when demand exceeds throughput (mped-architect review). Add the 100k run (measure ModelHost cost first) and a demand-exceeds-capacity case.
<!-- SECTION:DESCRIPTION:END -->
