---
id: TASK-92
title: >-
  diskgc --help lists the available profiles; profiles list/show with --json
  (agent onboarding)
status: Done
assignee: []
created_date: '2026-10-19 11:48'
updated_date: '2026-10-19 13:31'
labels:
  - profiles
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the user's goal (2026-10-19): AI agents must be able to onboard themselves from the binary alone. diskgc --help must say which profiles exist (built-ins and the config's user profiles), what each finds, its category, kind and action, whether it needs roots, and its checkouts setting, and point to diskgc profiles list / show NAME for details. Pulled forward from task-17 (list/show), without its golden-set machinery. Output contract: human text plus --json (one object); documented in docs/reference.md (Commands, profiles command).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 diskgc --help ends with a 'Profiles' section listing every built-in profile (name, category, one-line description) and how to list user profiles and see details
- [x] #2 diskgc profiles list [--config FILE] [--json] lists built-in and user profiles with name, category, kind, action, roots (fixed or required), checkouts; profiles show NAME [--json] prints the full definition incl. examples
- [x] #3 An e2e or CLI test runs the real binary and checks --help and profiles list/show against the built-in library (no drift: a new built-in appears without editing the help text)
- [x] #4 docs/reference.md documents the output and the JSON shape (pre-freeze addition noted on task-26)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implementation: crates/diskgc-core/src/profile/describe.rs renders a Library (help section, list table, show YAML, one profile JSON object for list and show, close names); main.rs parses `profiles list | show NAME [--config FILE] [--json]` in the existing hand-written style, reads the config through the same load_config as ensure (ensure_config keeps ensure's "needs a config without --select" rule), and prints. `profiles test` stays NotYet (task-17): exit 2, not trivial (the example model and the roots: required / {uid} questions in task-17's notes). Deviation from the brief: cargo-target has no examples (kind cargo-target takes none, reference.md), so `show cargo-target --json` has examples: null; the CLI test checks examples on a user fs profile instead. Decisions to review: profiles run as root without --allow-root (safety.md, Privileges); profiles goes through startup() (kernel and /proc checks), like every command but --help/--version/check; the list JSON carries the full profile object (same as show), not a summary.

Round 2 (QA + mped round 1 findings): --help gained a static "Start here" block (dry-run example first, PATH vs --roots, --min-free required also with --dry-run, "without --dry-run, ensure DELETES", the --json promise, ensure's exit codes on one line, checked against reference.md's table by a CLI test), and the Profiles section says `(roots required: --roots DIR)`. -h/--help anywhere is the full help, exit 0. --json is detected on the raw arguments (json_requested), so --json=yes and --config --json still give one object; the args structs no longer carry a json flag (one source). Error object {status, exit_code, error}. JSON: find.roots_required, snake_case JSON-only values, list table in the same spelling; ACTION shows the program behind env. Container test profiles_work_as_root_while_ensure_refuses (uid 0). Origin::name / Profile::role moved next to their types, EntryType::name in spec.rs, MORE private. profiles flags may precede the subcommand.
<!-- SECTION:NOTES:END -->
