---
id: TASK-152
title: 'A configured root that does not exist reads as a skipped candidate, exit 0'
status: To Do
assignee: []
created_date: '2026-10-24 21:10'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/14-ci-example-config.txt). The packaged example.yaml keeps roots: [/home/example/src]. check exits 0 (it is machine-independent by design), and ensure --dry-run prints '1 candidate(s) ... skip missing /home/example/src' and exits 0; a reclaim would exit 1 (exhausted). An operator who forgot to edit the roots learns nothing obvious. Expected: a missing root is a per-tier warning that names the tier and the root ('tier cargo-old: root /home/example/src does not exist'), not a candidate row; consider whether a reclaim with every root missing should be an error rather than exhausted.
<!-- SECTION:DESCRIPTION:END -->
