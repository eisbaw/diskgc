---
id: TASK-8
title: >-
  Profile model and embedded built-ins (cargo-target, idle filters) with roots
  fixed vs required
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 19:31'
labels:
  - wave-1
  - config
dependencies:
  - TASK-7
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the first end-to-end path needs profiles to select Cargo targets. Implement the profile schema types (find/where/action/kind/examples), embedded built-in YAML (initially cargo-target and the idle-* filters), roots fixed vs required, and the idle_for measure per kind. The rest of the library comes in wave 4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A roots: required profile without roots is a load-time error; roots on a fixed-root profile's tier is an error
- [ ] #2 Two idle filters in one term combine to the longest (unit test)
- [ ] #3 Built-in YAML is validated at build time (a broken built-in fails the build)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.

From task-14: diskgc-model loads YAML with serde-saphyr 2 (workspace dependency; serde_yaml is deprecated). Its errors carry line/column and a snippet, and deny_unknown_fields works through it. Reuse it for product YAML unless there is a reason not to. Test YAML under crates/ is already a flake build input (fileFilter on .yaml in flake.nix).

From task-7 (selection grammar): implement diskgc_core::select::Catalog over the profile model (built-ins plus user profiles, overrides applied). Interface: type Action: PartialEq + Display (your action type; two sources have the same action iff equal, and Display appears in the 'one action' error, which docs/reference.md quotes as `delete` / `command`, so render delete as 'delete' and command actions as 'command' or update that table row); fn lookup(&self, name) -> Option<Entry<'_, Action>> with Entry::Source { kind, action: &Action } for profiles with find, Entry::Filter { predicates: Vec<Predicate> } for where-only profiles. select::Kind and select::Predicate (with Predicate::kinds / supported_by, the Predicates table) already exist in select/check.rs: reuse them (move them to the profile module and re-export from select if you prefer), do not define a second copy. Profile names must satisfy select::is_name (ASCII letters, digits, - and _), the grammar's NAME (docs/reference.md); reject others at load. The tests' FakeCatalog (select/tests.rs) mirrors the built-in table by hand; once the real catalog exists, consider testing the doc examples against it too. docs/reference.md now lists idle-2d among the built-in filters (the example config uses it).

Amends the task-7 note above (review S5, H6): Kind and PredicateName (no longer select::Predicate) are in diskgc_core::kind, also re-exported from select. Catalog::Action equality must cover the action type plus all its options (delete with make_writable, command argv, cwd, timeout if it is part of the action); two command actions that differ only in argv are different actions. Its Display must show what differs: the one-action message must never read 'removes with `command` but ... with `command`' (e.g. render command as its argv). docs/reference.md's messages table shows `delete` vs `command` from the test FakeCatalog; update that row when the real rendering exists.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
