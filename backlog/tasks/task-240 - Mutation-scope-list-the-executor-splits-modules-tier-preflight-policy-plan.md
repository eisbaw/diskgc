---
id: TASK-240
title: >-
  Mutation scope: list the executor split's modules (tier, preflight, policy,
  plan)
status: To Do
assignee: []
created_date: '2026-10-25 23:09'
labels:
  - tooling
  - testing
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-196 (branch t182, 11c8a8d). tools/mutation_scope.py SOURCES and tools/mutants_container_smoke.sh name crates/diskgc-core/src/executor.rs as the executor mutation source; the split moved Tier/selection, preflight (measure, preflight_roots), policy (protect/exclude, overlap proof) and the dry-run planners into crates/diskgc-core/src/executor/{tier,preflight,policy,plan}.rs, which the sweep no longer mutates. verify_mutant_sweep.py special-cases the name executor (expected_files) and hashes one file per source. Decide which of the new files belong in the select/executor scope and list them, keeping the verifier's per-source hash and shard checks.
<!-- SECTION:DESCRIPTION:END -->
