---
id: TASK-273
title: Docs and dead code drift after the index/watch window
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - docs
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 11-13, 15, 16 + NITs: testing.md names 8 tests that do not exist without an open marker (index and collapse; Index on the real kernel); architecture.md Implementation lists nixosModules.default and a module-evaluation check that flake.nix lacks (TASK-45); reference.md lists backend_unavailable without marking it reserved; stale module docs (diskgc-core lib.rs 'not yet wired (TASK-31)', executor.rs 'Task-31's index is separate', report NULL_FOR_NOW blaming task-28/29 instead of TASK-244, main.rs omits watch, tally/mod.rs 'watch (TASK-32)', diskgc-linux lib.rs module list); reference.md sandbox gate exit 3 vs code exit 2; watch JSON index.* bullets need '(null, TASK-244)'; architecture 'no shell scripts' claim false; nix_gc_* scenarios need '(TASK-36)'; dead Host::hostname, Budget::time_left; SourceFind::CargoTarget reserved for TASK-243; narrow watch internals to pub(crate). Add a cargo doc check with -D rustdoc::broken_intra_doc_links.
<!-- SECTION:DESCRIPTION:END -->
