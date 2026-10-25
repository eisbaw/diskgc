---
id: TASK-139
title: 'Under the 0.1 catalog, a --select refusal should name the catalog'
status: Done
assignee: []
created_date: '2026-10-24 20:48'
updated_date: '2026-10-25 00:54'
labels:
  - followup
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-130 (2026-10-24): '--select profile:X' under the 0.1 catalog says 'no profile or filter is named X' without mentioning the release catalog, while the config refusal does. Operators will think the name is misspelled.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25: fixed on branch docs-setup with TASK-146 (select::Catalog::withheld -> CheckError::NotInRelease); vector release/dev-only-builtin-select-refused asserts it; walked tmp/walks/2026-10-25-docs-setup/05-select-withheld-builtin.txt.

Merged to master as d2c0554 on 2026-10-25T00:54Z (branch docs-setup, rebased; just check exit 0). Orchestrator accepted two deviations: JSON cause named 'provisioning' (fault class, matches exit-2 wording); 'check' warns on a missing supplied root with exit 0 (check is machine-independent; build-time checks), while ensure exits 2.
<!-- SECTION:NOTES:END -->
