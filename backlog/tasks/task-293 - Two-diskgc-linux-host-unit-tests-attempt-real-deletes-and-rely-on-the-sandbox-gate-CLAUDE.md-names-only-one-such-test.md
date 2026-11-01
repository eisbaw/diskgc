---
id: TASK-293
title: >-
  Two diskgc-linux host unit tests attempt real deletes and rely on the sandbox
  gate; CLAUDE.md names only one such test
status: To Do
assignee: []
created_date: '2026-11-01T08:42:00Z'
labels:
  - test
  - safety
  - needs-mark
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-282 implementer (2026-11-01): crates/diskgc-linux/src/host.rs unit tests deleting_outside_a_container_is_refused_by_the_gate and the_engine_walks_here_but_its_delete_is_refused_by_the_gate attempt real deletions on the developer host and rely on the sandbox gate to refuse. Neither CLAUDE.md nor docs/testing.md names them; CLAUDE.md says the CLI gate test is the sole host exception. Mark decides: name them as exceptions (they test the gate itself at the Host level, own temp fixtures under tmp/) or move them into a container test. Also: CLAUDE.md says 'watch without --dry-run' but watch has no dry run; its read-only form is --tally-only (wording is Mark's). Follow-ups for the TASK-282 scan (tools/host_reclaim_scan.py): one shared definition of a reclaiming argv with docs_vs_binary.host_refusal; function attribution by last def before a hit; false positives like assert_eq!(cmd, "ensure"); treat --help as read-only.
<!-- SECTION:DESCRIPTION:END -->
