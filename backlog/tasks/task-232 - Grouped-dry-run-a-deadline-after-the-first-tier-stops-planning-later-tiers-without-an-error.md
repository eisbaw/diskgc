---
id: TASK-232
title: >-
  Grouped dry run: a deadline after the first tier stops planning later tiers
  without an error
status: Done
assignee: []
created_date: '2026-10-25 17:29'
updated_date: '2026-10-25 20:22'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 t232 (not merged): 802e85c. Root cause: dry_run_grouped_inner stopped at the overall deadline (after tier 1, loop top, after a later tier) with status deadline and no error, so later tiers went unplanned silently. Fix: one operation error per unplanned tier ('deadline passed before this tier was planned'); 124 unchanged. Same class: a tier's own timeout spent after its plan completed gave status error/exit 3 with empty errors; now dry_run/0 unless the part recorded an error. Live grouped reclaim unchanged by contract (deadline is no error in reclaim). Model tests red on master, green on branch; loop condition mutation-checked. Loop-top record_unplanned site has no test. Gate: nix develop -c just check exit 0, 1199 passed / 0 failed / 1 ignored. No e2e or vector drives a grouped dry run into a deadline. Review: mped-architect ship; findings 1 and 3 fixed; 2 (fold first tier into the loop) and 4 (doc sentence that reclaim does not list untried tiers) open.

Merged to master as aea6913 on 2026-10-25T20:22Z.
<!-- SECTION:NOTES:END -->
