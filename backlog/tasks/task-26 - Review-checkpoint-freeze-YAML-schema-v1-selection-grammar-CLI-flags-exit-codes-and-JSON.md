---
id: TASK-26
title: >-
  Review checkpoint: freeze YAML schema v1, selection grammar, CLI flags, exit
  codes and JSON
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-19 13:40'
labels:
  - wave-2
  - review
  - irreversible
dependencies:
  - TASK-25
  - TASK-22
  - TASK-55
  - TASK-17
  - TASK-83
  - TASK-85
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: these surfaces become a contract once anyone deploys diskgc; this is the last cheap moment to change them. Run mped-architect, a CLI/UX reviewer and a safety reviewer on docs/reference.md against the implementation; resolve findings before freezing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each reviewer's findings are fixed or explicitly accepted with a reason recorded in the task
- [ ] #2 docs/reference.md and the implementation agree (spot-checked by tests for every flag, field and grammar rule)
- [ ] #3 The command-action, Docker and Nix parts of the schema are marked provisional until the release review
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Pre-release items from the task-7 review (mped), to settle before the grammar freezes:
H1: did-you-mean suggestions for unknown names and unknown qualifiers.
H2: the mixing error says 'add parentheses' even where no parenthesisation is accepted (e.g. (profile:a || profile:b && filter:f), or when both readings break a static rule); say so instead.
H3: warn on profile:a && !profile:a (selects nothing) and on duplicate terms (profile:a || profile:a).
H4: escape control characters when echoing an expression in an error.
H5: errors restated from syntax errors (wrong qualifier at a term start, profile:F after &&) lose the column; carry it.
H7: stable error codes for --json, and a sentence in docs/reference.md that machines match codes, not message text (also on task-15).
H8d: the doc-drift test reads only docs/reference.md; PRD.md's expressions (e.g. in How it works) are untested; add PRD.md to workspaceTestSrc and test them.

From task-7's final review (mped, QA), for the grammar freeze: ((profile:a) && filter:f) alone gets 'drop them' with no rewrite though profile:a && filter:f is accepted; ((profile:a || profile:b) || profile:c) && filter:f reports a nesting error instead of suggesting the flat group (a || b || c) && f; PRD.md's Composition row says '|| only with parentheses' but profile:a || profile:b is valid (fix with H8d).

From task-10's deep gate, to decide at the freeze (filed, not fixed): (1) actions entries carry no 'complete' flag: a part-way deletion is an actions entry plus a skipped/errors entry for the same candidate; consider 'complete: bool' on actions so a harness need not join the lists. (2) a dry run whose discovery fails (Stop::Failed: EIO, EMFILE) exits 0 with status dry_run and the error in errors (reference.md: dry run exits 0 except usage/deadline/signals); decide whether a dry run that could not plan should exit 3 instead.

From task-10's deep gate round 2: (3) QA: a dry run that stops on a host error (Stop::Failed during discovery or eligibility) exits 0 with errors[] filled; decide with item (2) above. (4) mped: expose 'tally cut short by --tally-budget' in the JSON (e.g. under index once it exists), since the reclaim order then differs from the dry run's plan.

(5) From task-10 round 4: a dry run with an unmeasured final sample exits 0 (or 124 at the deadline): it reports avail_after_bytes null plus an errors entry, while a reclaiming run exits 3 for the same (reference.md, Exit codes); decide with (2)/(3).

From task-10 final round: (6) a failed first statvfs (EIO) reports status usage_error / exit 2, which reads as the user's mistake; decide with (2)/(3)/(5).

From task-68 (profile spike) and its review (mped S3): proposals for Mark to settle at the freeze, all now in docs/reference.md and implemented by the dry run (crates/diskgc-core/src/fs_kind/pattern.rs):
1. In find.paths, `*` matches any run of bytes within one name, including a leading dot (unlike the shell): `*` matches `.X11-unix`, so tmp-entry covers hidden entries.
2. `?`, `[`, `]` and `\` are reserved in patterns (a load-time error), so a later glob syntax cannot change an existing pattern's matching set. Python's tempfile dirs (/tmp/tmpXXXXXXXX) are not expressible without them (task-16).
3. has / has_not: a regular file of that name directly in the candidate directory, not followed. Consequence (QA): a symlinked CMakeLists.txt does not count, so `has_not: CMakeLists.txt` passes a source tree whose CMakeLists.txt is a symlink (cmake-build, task-71).

From task-68 r2 (mped S1): a --roots root inside systemd-private-* is skipped silently in JSON (only a stderr log line and the private_tmp summary count); other unsearchable roots are plan skips or run errors. Reporting it needs a reason code: decide at the freeze.

Decision by Mark (2026-10-18): signed off for schema v1: (a) * matches a leading dot (unlike the shell); (b) ?, [, ], \ are reserved glob characters, refused at load; (c) has/has_not = a regular file directly in the candidate, symlinks not followed. reference.md already states these; at the freeze they are settled, not open.

Pre-freeze addition (task-71, 2026-10-18): new skip reason contains_checkout in the JSON enum (docs/reference.md, Output; Reason::ContainsCheckout), for a plain-delete candidate holding a .git, .hg or .svn entry (docs/safety.md, The eligibility walk). Review it with the rest of the enum before the freeze.

Pre-freeze addition (task-80): JSON plan entries gained "checkouts": "refuse | allow" (the source profile's setting the candidate was judged under), documented in reference.md (Output) and checked by report::tests (field names and the value set). Schema v1 also gained the profile field checkouts: refuse | allow (reference.md, Profiles). Review both at the freeze.

Pre-freeze addition (task-92, 2026-10-19): `diskgc profiles list --json` / `show NAME --json` (docs/reference.md, `profiles` command): {"config", "profiles": [profile]} and {"config", "profile": profile}, one profile object for both (every schema field always present, null where it does not apply; `origin` built-in | user, `role` source | filter, `find.roots` a list or "required" as the YAML spells it, `action` {type: delete | command | cargo-lock-protocol, make_writable, command, cwd}), and {"error": message} on exit 2. Checked against the doc's JSON blocks by profile::describe::tests::the_json_fields_are_the_docs. To review at the freeze: the mixed type of find.roots (string or list), `role` as the field name, `origin` spelled built-in (kebab, like kind names, unlike the report's snake_case enums), and whether errors should carry a stable code (H7 above). Also decided in task-92 and to confirm: `profiles list/show` run as root without --allow-root (docs/safety.md, Privileges), since they only read and print.

Update (task-92 round 2, 2026-10-19), superseding parts of the note above: the profile JSON now has `find.roots` a list or null plus `find.roots_required` (bool), never string-or-list; JSON-only values are snake_case like the report (`origin` built_in | user, `action.type` cargo_lock_protocol), YAML identifiers keep their YAML spelling (kind names, delete, command, dir); the list table uses the same spellings. Error object of every command but ensure under --json: {"status": "usage_error", "exit_code": 2, "error": message}, so agents branch on `status` as in ensure's report (ensure keeps its report). `--json` is now detected on the raw arguments (any `--json` or `--json=...`, also as another flag's value), for ensure too, so every exit prints one object. `-h`/`--help` anywhere is help, exit 0. Still to review at the freeze: `role` as a field name; whether `exit_code` belongs in ensure's report too; stable error codes (H7).

From task-92 round 3 (2026-10-19), for the freeze: (a) spelling across commands: the report's plan[].operation is human-readable text ("cargo lock protocol", a command's argv; documented so in reference.md, Output), while the profiles JSON's action.type is the enum-like "cargo_lock_protocol"; decide whether operation stays free text or gains an enum next to it. (b) mped: the profiles JSON on success has no `status` (errors have "status": "usage_error"); consider "status": "ok" so every object can be branched on one field. (c) mped: profiles JSON `where.idle_for` is duration text ("3d", the YAML spelling) while the report's durations are numbers (duration_ms, over_ms); decide one convention.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
