---
id: TASK-8
title: >-
  Profile model and embedded built-ins (cargo-target, idle filters) with roots
  fixed vs required
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 23:50'
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
- [x] #1 A roots: required profile without roots is a load-time error; roots on a fixed-root profile's tier is an error
- [x] #2 Two idle filters in one term combine to the longest (unit test)
- [x] #3 Built-in YAML is validated by tests in the gates: a broken built-in fails just check and nix flake check (workspace-tests); the package build itself has doCheck = false
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.

From task-14: diskgc-model loads YAML with serde-saphyr 2 (workspace dependency; serde_yaml is deprecated). Its errors carry line/column and a snippet, and deny_unknown_fields works through it. Reuse it for product YAML unless there is a reason not to. Test YAML under crates/ is already a flake build input (fileFilter on .yaml in flake.nix).

From task-7 (selection grammar): implement diskgc_core::select::Catalog over the profile model (built-ins plus user profiles, overrides applied). Interface: type Action: PartialEq + Display (your action type; two sources have the same action iff equal, and Display appears in the 'one action' error, which docs/reference.md quotes as `delete` / `command`, so render delete as 'delete' and command actions as 'command' or update that table row); fn lookup(&self, name) -> Option<Entry<'_, Action>> with Entry::Source { kind, action: &Action } for profiles with find, Entry::Filter { predicates: Vec<Predicate> } for where-only profiles. select::Kind and select::Predicate (with Predicate::kinds / supported_by, the Predicates table) already exist in select/check.rs: reuse them (move them to the profile module and re-export from select if you prefer), do not define a second copy. Profile names must satisfy select::is_name (ASCII letters, digits, - and _), the grammar's NAME (docs/reference.md); reject others at load. The tests' FakeCatalog (select/tests.rs) mirrors the built-in table by hand; once the real catalog exists, consider testing the doc examples against it too. docs/reference.md now lists idle-2d among the built-in filters (the example config uses it).

Amends the task-7 note above (review S5, H6): Kind and PredicateName (no longer select::Predicate) are in diskgc_core::kind, also re-exported from select. Catalog::Action equality must cover the action type plus all its options (delete with make_writable, command argv, cwd, timeout if it is part of the action); two command actions that differ only in argv are different actions. Its Display must show what differs: the one-action message must never read 'removes with `command` but ... with `command`' (e.g. render command as its argv). docs/reference.md's messages table shows `delete` vs `command` from the test FakeCatalog; update that row when the real rendering exists.

CORRECTION: NAME is [a-z0-9][a-z0-9_-]* (user decision, task-7); the earlier 'ASCII letters, digits, - and _' is superseded. Enforce is_name on every profile and filter name at load.

Implemented (task-8, uncommitted; staged for review). Code: diskgc-core profile/ (spec.rs YAML types, load.rs static rules, mod.rs model + Library + Catalog impl, roots.rs, idle.rs, builtin.yaml), units.rs (parse_duration/format_duration), doc.rs (test-only reference.md helpers shared with select tests).
[Superseded in part by the task-8 review round below: IdleFor::admits, the hand-written visitors and a source's own idle_for are gone.]
Schema decisions written into docs/reference.md (Profiles fields table, Predicates YAML, Actions YAML): find.type is required for kind fs and is dir|file|any; action is required for fs (no default) and is 'delete' | {delete: {make_writable: true}} | {command: [argv], cwd}; command 'timeout' REMOVED from the action (the tier's timeout bounds it; reference.md had both with no precedence); cwd default resolved at load (dir: candidate, file: parent, any: per candidate; cwd: candidate with type file is an error); cargo-target's find has only roots, no action/examples; a source may have its own where (combined with the term's filters); built-ins need category, description, verified_with (sources), examples with a match (fs). Messages-table row for pytest-tmp || pip-cache now shows the real rendering.
[Superseded in part by the task-8 review round below: IdleFor::admits, the hand-written visitors and a source's own idle_for are gone.]
Loader: hand-written serde visitors for the mappings with keyword keys (where/type/match), because serde's per-field rename attribute trips the source scan's gated word 'rename'; unknown and repeated keys are still rejected. Problems of every profile are collected (LoadErrors, each with built-in profile `x` / profiles[i] (`x`)).
Roots: Library::bind_roots(&Checked, Option<&[String]>) -> name -> roots (sources and !profile:), Library::roots_of(name, ..) for protect entries; errors Missing / Unused / Empty / Path / NotASource.
[Superseded in part by the task-8 review round below: IdleFor::admits, the hand-written visitors and a source's own idle_for are gone.]
idle_for: Library::idle_for(&Term) = longest of the source's own and its filter: factors (not !filter:); IdleFor::admits(last_use: Option<Timestamp>, now) fails closed (None or future = not idle); IdleMeasure::of(kind) (newest change / last build / Docker last use). No realtime clock in the Host port yet: the consumer supplies now.
Select tests: FakeCatalog replaced by the real library + test-profiles.yaml (a, b, f, zephyr-run, and marked stand-ins that collide with a future built-in of the same name, which forces their removal) + TestAction stand-ins for docker-*/nix-store (no profile schema for those kinds yet).
Deps: serde-saphyr pinned =2.0.0, default-features off, feature deserialize (diskgc-model adds serialize); PRODUCTION_DEPENDENCIES updated with the review comment; diskgc-e2e/Cargo.lock gained diskgc-core's two edges.
Tests and bites: AC1 ac1_a_required_profile_without_roots_is_an_error, ac1_roots_for_only_fixed_root_profiles_are_an_error (bites: drop Missing, drop Unused, ignore !profile: -> fail). AC2 ac2_idle_filters_in_one_term_combine_to_the_longest (bites: last wins, shortest wins, source's own ignored, !filter combined -> fail). AC3 the_built_in_library_loads (+ idle_filter_names_say_their_duration, built_ins_are_in_the_reference_table, a_broken_built_in_does_not_load); bites: idle_for: 7 in builtin.yaml -> just check exit 101; Idle-30d -> flake workspace-tests exit 1. Also unknown_keys_are_rejected_everywhere, every_profile_name_is_a_name, actions_are_equal_only_with_every_option_equal, the_one_action_error_shows_what_differs (bites on make_writable, argv/cwd rendering, cwd default).

Limits (task-8): (1) examples are parsed and checked to be absolute paths, but not matched against the patterns: no matcher exists yet (task-16 owns pattern semantics, task-17 profiles test). (2) override: true is not implemented: it is an unknown key today and a user profile named like a built-in is always a load error ('not supported yet'), stricter than reference.md's User profiles until task-17. (3) Kinds docker-build-cache, docker-images, nix-store have no profile schema yet: a profile of those kinds is a load error 'not supported yet'. (4) cargo-target's verified_with names the Cargo versions whose lock sets were measured (docs/safety.md); discovery (task-9) and the race tests (task-11) have not run, so revisit it then. (5) Errors from the static rules name the profile (built-in profile `x` / profiles[i]), not the YAML line; serde-level errors (unknown key, bad duration, bad kind) carry line and column. (6) In the hand-read mappings (profile, find, examples) an explicit null value (kind: ~) is an error, not an absent key. (7) IdleFor::admits needs a realtime 'now'; the Host port has only a monotonic clock.

Review round (mped GO with should-fixes; user decisions U1-U3), applied before the schema freeze:
U1: a source's own where takes only has / has_not (load error for idle_for: 'when' is the tier's, through filter:s); Library::idle_for combines filter: factors only; reference.md says protect, like !profile:, ignores where. No built-in used a source idle_for. Test sources_have_no_idle_for_of_their_own.
U2: user fs profiles need examples with at least one match, like built-ins (reference.md, Profiles table and User profiles). Test user_fs_profiles_need_examples_with_a_match; test YAML files gained examples.
U3: safety.md Command actions now says killed after the tier's timeout.
S1: IdleFor::admits replaced by verdict() -> Idleness { Idle, NotIdle, Unknown(Unanswered | InFuture) }; Idleness::passes(negated) is false for Unknown either way; Unknown::skip_reason() = unsupported_fs / future_time; future_time added to reference.md's skipped enum and the filter paragraph. Tests an_unknown_age_passes_no_idle_filter, unknown_age_reasons_are_documented.
S2: cwd: candidate with type any is a load error (as with file). Test cwd_candidate_needs_directory_candidates.
S3: command[0] containing {path} is a load error. Test the_program_is_never_the_candidate.
S4: reference.md defines find.type: dir = directory, file = regular file, any = directory or regular file; symlinks, sockets, FIFOs and devices are never candidates (a symlink candidate could only be unsafe_path; tmp-entry would report every symlink in /tmp).
S6: ProfileSpec, FindSpec, ExamplesSpec are serde derives with deny_unknown_fields and raw identifiers r#where / r#type / r#match; visit_fields and the three hand-written visitors are gone. source_scan blank() reads r#ident as ident (blanks the r#; whitespace keeps a :: path), raw strings are still refused; test raw_identifiers_are_read_as_identifiers (fs::r#rename, rustix::fs::r#unlinkat, r#std::r#fs::r#remove_file caught; std::r#fs caught by os_access). Duplicate keys are rejected by serde-saphyr's default (test kept), and where_ / r#where / type_ / r#match are unknown keys (tested).
S7: AC #3 reworded via the CLI to name the gates (just check, nix flake check); the package build has doCheck = false.
Binary size: 585,320 B at a7bce15, 585,616 B now (profile code not called yet); 2,272,040 B with Library::builtin() called (temporary probe, restored). Filed as task-61 (43). Hardening items filed on task-61 (39)-(43); notes on tasks 9, 16, 17, 19.

Harness fix in this change (QA's first just e2e failed once: cargo_traced_build_within_bound@1.99.1, 'strace line without =: ???( <detached ...>', load-dependent): diskgc-e2e/src/strace.rs now classifies every strace -ff line form. parse_line -> Line { Call, Unfinished, Resumed, Unnamed, NotACall }; parse_file joins '<unfinished ...>' with its '<... NAME resumed>' line (signals may come between) and turns a call never resumed into '= ?' (Outcome::Unknown, took_effect, as a kill mid-call); 'NAME(ARGS <detached ...>' parses as '= ?' with its paths (the call can complete after the detach); '???(' lines are Unnamed (no name, no path): kept in Run.unnamed_calls and logged by the oracle, not counted as removals; the snapshot view judges every object. '+++ killed by', '+++ superseded by execve', '--- SIG' are NotACall; anything else is still an error. Tests: an_unnamed_detached_call_is_kept_apart_not_dropped, a_named_call_detached_mid_call_is_possibly_effective, unfinished_and_resumed_halves_are_joined, an_unfinished_call_never_resumed_is_possibly_effective, signal_and_exit_lines_are_not_events. Bites (each failed e2e-tests): detached named line dropped; detached named counted as failed; ??? an error again; ??? silently dropped; unresumed call dropped at EOF; unfinished not joined.

Committed as a5e79bb (tree d2a29b4) after qa-test-runner GO and mped-architect GO (two rounds; user decisions U1 source where has/has_not only, U2 user fs profiles need examples, U3 tier timeout bounds command actions). The commit also carries the e2e harness strace-parser fix for the load-dependent flake QA hit.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
