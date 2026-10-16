---
id: TASK-14
title: 'ModelHost: YAML host model, virtual clock, events on the call trace'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 23:13'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-3
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most behaviour is tested in-process and deterministically through the Host port. Implement diskgc-model (inode graph, derived free space, strict loading with deny_unknown_fields and resolved references, JSON Schema, virtual clock, events addressed by Host call, call-flag validation such as missing RESOLVE_* flags or LOCK_UN) and diskgc-modelgen value generators.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The sketch in docs/testing.md loads and runs as a passing model test
- [ ] #2 An openat2 without the required flags, or a lock released with LOCK_UN, fails the test (proves the model checks how it is called)
- [ ] #3 Unknown keys and dangling pid references are load errors
- [ ] #4 An unlinkat without a preceding identity check fails the test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
