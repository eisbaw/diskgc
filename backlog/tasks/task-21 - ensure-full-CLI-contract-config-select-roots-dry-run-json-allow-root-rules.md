---
id: TASK-21
title: >-
  ensure: full CLI contract (config, --select/--roots, --dry-run, --json,
  --allow-root rules)
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-19 04:49'
labels:
  - wave-2
  - cli
dependencies:
  - TASK-20
  - TASK-54
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: CI hooks and harnesses depend on the exact flags and exit codes. Complete ensure per docs/reference.md#ensure: config groups, --select/--roots validity rules, --dry-run plan, --json one object always, --allow-root (owners required, command actions refused), size and duration syntax, mount printed in every report.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every flag and rule in docs/reference.md#ensure has a test
- [ ] #2 --dry-run deletes nothing and takes no lock (deletion oracle + lock probe)
- [ ] #3 Bare numbers and month durations are usage errors
- [ ] #4 --allow-root: owners required and resolving to non-system users; the config file and every directory above it root-owned and not writable by others; command actions refused
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-10: CLI parsing is hand-rolled in crates/diskgc/src/main.rs (parse/parse_ensure: --flag v and --flag=v, repeated single flags and unknown flags are usage errors, PATH must be absolute, --roots needs --select, --timeout 0 = none, default 10m; --tally-budget/--no-index parsed and ignored until the index). --json detection happens before parsing so usage errors print one JSON object. Contract decisions written into reference.md: reports name the --select tier and group '--select'; every exit 2 has status usage_error (root refusal, missing host lock, sandbox gate included); PATH absolute; empty/relative --config or DISKGC_CONFIG is exit 2; an unreadable user config is exit 2, not a fallback; config may be a symlink (LinuxHost::read_config_file follows symlinks: the --allow-root StrictModes check of the config and its directories must therefore resolve the real path and check every directory of it); non-UTF-8 path bytes show as U+FFFD in JSON; a part-way deletion is in actions and in skipped/errors; held_by_deleted_files empty when no reclaim ran. Still to do here: --allow-root (currently always refused, executor::privileges), owners:, command-action refusal, aggregation of skipped (task-19), the 'index' field, and the full flag-rule test sweep (AC #1). check is 'not implemented' (exit 2) in this build.

From task-63's review: owners: by name on the static musl binary cannot resolve LDAP/sssd accounts (no NSS; nscd only): task-91. AC #4's name resolution must take it into account.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
