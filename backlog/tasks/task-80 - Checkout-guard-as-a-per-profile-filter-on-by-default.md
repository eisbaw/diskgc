---
id: TASK-80
title: 'Checkout guard as a per-profile filter, on by default'
status: Done
assignee: []
created_date: '2026-10-18 21:58'
updated_date: '2026-10-19 00:03'
labels:
  - profiles
  - irreversible
  - wave-1
dependencies:
  - TASK-71
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: decided 2026-10-18 in review of task-71. diskgc should not special-case profiles in code (neither pytest-tmp nor cargo-target). Make the .git/.hg/.svn refusal a profile-level filter that is ON by default (fail closed) and that a profile, built-in or user, can turn off explicitly. This supersedes the 'there is no opt-out' sentence in docs/safety.md and replaces the code-level Cargo exemption (engine::walk_cargo_profile_entry): cargo-target turns the filter off in its profile definition instead. Whether pytest-tmp turns it off is decided separately, after the read-only survey of its leftover repos (age, worktrees, real work).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A profile field (name decided and documented in reference.md) turns the checkout refusal off; absent means on; the loader rejects anything else
- [x] #2 cargo-target's built-in definition turns it off and the code-level exemption is removed; Cargo eligibility and reclaim behave as before (existing tests stay green)
- [x] #3 A user profile with the filter off can match a folder holding .git; with it absent the folder is contains_checkout (model tests, each bitten)
- [x] #4 The path rule (proj/.git/objects) follows the same per-profile setting, or the docs say why it does not
- [x] #5 An allow is visible: ensure and --dry-run log it once per tier, and --json plan entries carry the setting (checkouts); docs/safety.md (single home) describes the default, the opt-out and its risk. profiles list/show: task-17 AC
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented (task-80 implementer, 2026-10-19):
- Field: checkouts: refuse | allow on source profiles of any kind (reference.md, Profiles fields table). Absent means refuse; the loader rejects any other value (serde strict enum: Allow, yes, off, true, empty, a list) and the field on a filter. A field, not a where predicate: where narrows matches, and an absent predicate would read the wrong way round.
- Type: engine::Checkouts {Refuse (#[default]), Allow}; engine::Rules gains checkouts; Rules::new(owners) refuses, so anything built without stating the setting refuses. engine::walk reads rules.checkouts; walk_cargo_profile_entry (the code-level Cargo exemption) is removed, and cargo_target::eligible::walk_entries calls engine::walk.
- Plumbing: profile::Source.checkouts -> executor::Tier.checkouts -> the run's rules (executor rules(tier, owners, log)); the Cargo kind gets allow only from builtin.yaml (cargo-target: checkouts: allow). Test code and the e2e driver read it from the built-in library too, never state it.
- Path rule: engine::checkout_metadata_below(checkouts, root, at) -> Option<Skip>, the same setting; fails closed when at is not below root (mped r3 SHOULD; unit test cases /other/.git/x, /rx/.git/y, /rx/y). Applied by the fs planner and now also by the Cargo kind (tally/eligibility before reach, and the reclaim protocol before any lock), so a user cargo-target profile that refuses checkouts refuses a target inside .git metadata.
- Visibility: profiles list/show do not exist yet (task-17, note added; reference.md says what they will show). An allow is logged once per tier by ensure and --dry-run: '<tier>: profile `<name>` says `checkouts: allow`: ...'. JSON unchanged.
- Overlap: refuse wins (safety.md); not reachable yet (one source per tier in this build; unions are task-16/20).
- AC #5 is half done: the dry-run (and ensure) log and safety.md are; profiles show/list are task-17's (noted there).

Round 2 (2026-10-19): null values refused (checkouts:, ~, null, Null, NULL; on sources and filters), CheckoutsSpec in spec.rs (serde off the engine type); a candidate not below its root is unsafe_path whatever the setting; Library::checkouts shared by tests and the e2e driver; --json plan entries carry checkouts (reference.md Output, report tests; pre-freeze note on task-26); the reclaiming ensure path's allow log line is tested; pytest-tmp says checkouts: allow (Mark's decision; task-65); reference.md library table is the one home for which built-ins allow; PRD wording (Safety, Nesting rows, bullet) for Mark's review. Gate: six flake checks plain and --rebuild, just check, just e2e, just e2e-full, all green.

Superseded (round 2/3): in the round-1 note above, 'JSON unchanged' no longer holds (--json plan entries carry checkouts since round 2), and 'AC #5 is half done' no longer holds (AC #5 was rewritten to this task's scope and checked; profiles list/show moved to a task-17 AC).
Round 3 (2026-10-19), Codex BLOCKER fixed: under checkouts: refuse the Cargo kind's path rule now runs from the profile root down to every entry it would delete (cargo_target::eligible::judge_checkout_paths, over the inspected profile entries), so the target's own name, a target triple's and the profile directory's count: .git/debug, target/.git and target/.git/debug (x .git, .hg, .svn) are contains_checkout in eligibility and in the reclaim protocol (before any lock, and again under the locks via judge_inspected). Regression test: ensure.rs a_cargo_target_with_a_marker_between_root_and_its_entries_is_refused (dry run, ensure with and without the tally; nothing unlinked, never locked; the built-in allow keeps them eligible). Bitten. Also: more null spellings refused (!!null, a null alias), nested values; one name table (Checkouts::NAMES); reference.md rests pytest-tmp's allow on pytest's own retention; a test ties the library table's checkouts: allow to the shipped built-ins.
<!-- SECTION:NOTES:END -->
