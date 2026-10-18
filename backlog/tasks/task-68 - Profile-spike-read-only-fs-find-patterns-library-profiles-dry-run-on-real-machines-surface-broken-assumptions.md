---
id: TASK-68
title: >-
  Profile spike (read-only): fs find patterns + library profiles dry-run on real
  machines, surface broken assumptions
status: To Do
assignee: []
created_date: '2026-10-18 07:44'
labels:
  - wave-1
  - spike
  - profiles
dependencies:
  - TASK-10
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Mark (2026-10-18) wants the profile library exercised now, before more hardening, so that wrong assumptions in the PRD, profile schema, selection grammar and kind model surface early instead of after the wave-1 hardening and the schema freeze. Read-only: no deletion path is added or changed here. The deep-gated delete work stays in task-18, and the full selection engine with its property tests stays in task-16.
Scope: implement just enough segment-wise find-pattern matching (no **) for the fs kind's discovery and eligibility, reusing the engine walk and the in-use checks, so that ensure --dry-run --select profile:X plans fs candidates. Write the library profiles from docs/reference.md (Built-in profile library) as user profiles in a scratch config: the /tmp group (pytest-tmp, nix-shell-rc, nix-shell-tmpdir, claude-code-tmp, chromium-tmp, mktemp-dir, tmp-entry, playwright-tmp, node-compile-cache, zephyr-tmp, renode-tmp) and the roots-required ones that are plain deletes (pytest-cache, cmake-build, west-build, jest-cache, platformio-cache, gradle-caches, maven-repo). Command-action profiles (pip/go/npm/ccache/uv/pnpm/yarn) are written as YAML and validated but not run. Dry-run them on this machine (read-only, nice/ionice) and compare with ncdu/du for the same paths.
Output: a findings list. For every profile, which schema, grammar or kind assumption it could not express or got wrong (any-depth finds without **, per-uid paths, PrivateTmp, ownership, idle semantics for plain dirs, in-use false positives, etc.). Each finding is filed as a backlog task or recorded as a PRD/docs question, and the docs are fixed where the answer is clear.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fs find patterns plan candidates in ensure --dry-run for user profiles, with model tests that bite (red without the matcher, green with it)
- [ ] #2 Every listed library profile exists as a user profile with examples; those that the schema cannot express are listed with the reason
- [ ] #3 A read-only dry run of the profiles on this machine is recorded in the task notes (eligible counts and sizes per profile, skip reasons), with no reclaiming run
- [ ] #4 Each broken or questionable assumption is filed as a task, or as a docs/PRD change in this task, with the decision named as Mark's where it needs one
- [ ] #5 No change to any deleting code path (Delete port, engine delete, cargo reclaim, sandbox gate)
<!-- AC:END -->
