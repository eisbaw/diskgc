---
id: TASK-17
title: >-
  Built-in library infrastructure: overrides, profiles list/show/test, golden
  match sets
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 12:01'
labels:
  - wave-2
  - config
dependencies:
  - TASK-16
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the library must grow without silently changing meaning. Implement override: true for shadowing built-ins, the profiles list/show/test commands, examples running against an in-memory model, and per-built-in golden match sets over fixtures that fail CI on any change unless the profile was renamed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Shadowing a built-in without override: true is a load error
- [ ] #2 Changing a built-in's pattern without renaming fails the golden-set test (proved on a scratch profile)
- [ ] #3 profiles test exits 1 on a failing example
- [ ] #4 Golden sets also cover the profiles shipped in wave 1 (cargo-target and the idle-* filters)
- [ ] #5 profiles test matches examples on paths in diskgc-core (it does not depend on the test-only model crate)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.

From task-8: override: true is NOT implemented: it is an unknown key in ProfileSpec today, and load.rs (library()) rejects any user profile named like a built-in with 'overriding a built-in is not supported yet'; replace that with the override rule. Examples are stored (Source.examples, absolute paths checked) but never matched: no matcher yet. Built-in-only rules live in profile/load.rs (category and description required, verified_with required on sources and forbidden elsewhere, fs built-ins need examples with a match); tests: profile::tests::a_broken_built_in_does_not_load and the_built_in_library_loads. Action has a Display (used by the one-action error), not a YAML rendering for 'profiles show'. The select tests' stand-ins (crates/diskgc-core/src/select/test-profiles.yaml) collide with any built-in that ships under the same name, which makes the test catalog fail to load: delete the stand-in then. Golden sets for cargo-target are structural (fixtures), the idle-* filters have no paths.

From task-8's review: the select tests' catalog loads crates/diskgc-core/src/select/test-profiles.yaml as user profiles; with override not implemented, a stand-in that should override a built-in cannot be written (any shared name is a load error). When override lands, keep the stand-ins without override: true, so a built-in shipping under a stand-in's name still fails the test catalog and forces the stand-in's removal. User fs profiles now need examples with at least one match (user decision U2), same as built-ins.

From task-68: fs_kind::example_failures(source, uid) checks examples against the patterns (paths only); fs_kind::tests runs it over crates/diskgc-core/src/fs_kind/spike-profiles.yaml (all 27 fs sources of the spike). Open for `profiles test`: (1) roots: required examples can only be checked as "the last names match a pattern below some root", so a no_match is weak, and a profile whose last segment is `*` (cmake-build, cachedir-tagged) cannot have any no_match; (2) `{uid}` in patterns: the static check needs a uid; the spike uses 1000 and writes examples with it; (3) a source's own `where` and the entry type are not part of an example; the in-memory model of AC #5 would cover both. spike-profiles.yaml uses the library's names as user profiles: when a built-in of the same name ships, the file fails to load (no override), which is the reminder to delete or rename its row.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
