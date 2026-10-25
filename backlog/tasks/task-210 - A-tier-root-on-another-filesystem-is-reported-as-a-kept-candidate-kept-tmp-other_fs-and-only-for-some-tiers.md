---
id: TASK-210
title: >-
  A tier root on another filesystem is reported as a kept candidate (kept /tmp:
  other_fs), and only for some tiers
status: Done
assignee: []
created_date: '2026-10-25 12:09'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: `ensure --dry-run ~` with the example config prints `kept /tmp: other_fs [tmp-nix-shell-rc]` but nothing for tmp-pytest-sessions, and on /tmp prints `kept /home/tester/src: other_fs [cargo-old]`. /tmp and ~/src are roots, not candidates; listing them as kept inflates `N kept` and reads as if diskgc considered deleting /tmp. Decide: roots on another filesystem are a per-tier note (stderr / a JSON field), not a skipped candidate; make the /tmp tiers consistent. JSON contract impact (skipped rows with reason other_fs) needs a decision before 0.1 freezes. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/06-dry-config-home.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/07-dry-config-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark may veto): keep it a skipped other_fs entry but consistent: exactly once per tier, in every tier; no new JSON fields before 0.1.

2026-10-25 polish (branch polish 7abae1b (not merged)): per mark-emulator decision, other_fs root row exactly once per tier (union/negated duplicates removed), detail now says it is a root not searched (detail is advisory text; no field added); human summary names all tiers. Self-check vs reference.md Output: reason enum, field set unchanged; new bullet documents the row. Walk: tmp/walks/2026-10-25-polish/dumps/b10j-dry-union-home.json (2 rows) -> tmp/walks/2026-10-25-polish/dumps/a10j-dry-union-home.json (1), tmp/walks/2026-10-25-polish/dumps/a06-dry-config-home.txt, tmp/walks/2026-10-25-polish/dumps/a17j-live-config-home.json.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
