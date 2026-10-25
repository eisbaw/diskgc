---
id: TASK-152
title: 'A configured root that does not exist reads as a skipped candidate, exit 0'
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 00:54'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/14-ci-example-config.txt). The packaged example.yaml keeps roots: [/home/example/src]. check exits 0 (it is machine-independent by design), and ensure --dry-run prints '1 candidate(s) ... skip missing /home/example/src' and exits 0; a reclaim would exit 1 (exhausted). An operator who forgot to edit the roots learns nothing obvious. Expected: a missing root is a per-tier warning that names the tier and the root ('tier cargo-old: root /home/example/src does not exist'), not a candidate row; consider whether a reclaim with every root missing should be an error rather than exhausted.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 docs-setup branch (992ea99): decision (mark-emulator for Mark): supplied roots (tier/protect roots:, --roots) that do not exist -> ensure exit 2 (cause config; usage if only --roots), also dry-run and met-goal, naming root + 'config PATH: line N, tier/protect'; PATH-missing reported first; profile-fixed roots keep the missing skip. check keeps exit 0 (build sandboxes) and warns per root. Vectors: walk/missing-supplied-root-refused (was missing-root), config/missing-tier-root-refused, config/missing-protect-root-refused, config/absent-fixed-root-is-not-an-error. Walked: tmp/walks/2026-10-25-docs-setup/20..23.

Merged to master as d2c0554 on 2026-10-25T00:54Z (branch docs-setup, rebased; just check exit 0). Orchestrator accepted two deviations: JSON cause named 'provisioning' (fault class, matches exit-2 wording); 'check' warns on a missing supplied root with exit 0 (check is machine-independent; build-time checks), while ensure exits 2.
<!-- SECTION:NOTES:END -->
