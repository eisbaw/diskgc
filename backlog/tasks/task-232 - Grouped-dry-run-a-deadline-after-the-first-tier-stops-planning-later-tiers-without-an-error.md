---
id: TASK-232
title: >-
  Grouped dry run: a deadline after the first tier stops planning later tiers
  without an error
status: To Do
assignee: []
created_date: '2026-10-25 17:29'
labels:
  - shipping
  - json
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-229 (2026-10-25): in dry_run_grouped_inner, if the overall deadline passes after the first tier finishes, the loop sets Status::Deadline and stops without recording an error; later tiers are never planned and errors stays empty, contradicting the TASK-227 contract in docs/reference.md ('errors names the cut only when the plan was cut short'). Fix: record the cut in errors (the same deadline error the single-tier path records), naming the tiers not planned; model test red on master; check the live grouped reclaim path for the same.
<!-- SECTION:DESCRIPTION:END -->
