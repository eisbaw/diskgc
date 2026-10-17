---
id: TASK-15
title: >-
  Config file: lookup, YAML schema v1, groups/tiers/protect/owners, static
  validation and check
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 13:31'
labels:
  - wave-2
  - config
dependencies:
  - TASK-13
references:
  - docs/reference.md
  - docs/safety.md#validation
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the YAML schema is frozen once users deploy it. Implement config lookup (explicit file only when given; XDG then /etc otherwise), schema version 1 with unknown keys rejected, groups/ranking/seed, tiers (select, roots, order, timeout), protect entries (bare for fixed-root, {profile, roots} for roots-required, kinds fs/cargo-target), owners (run-time resolution), and the static validation list in docs/safety.md#validation exposed as diskgc check (machine-independent).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A typo in --config or DISKGC_CONFIG exits 2 and never falls back
- [ ] #2 check runs in a Nix build sandbox with no access to the target machine (test)
- [ ] #3 Every rule in the static validation list has a failing-config test
- [ ] #4 check prints each tier's expression normalised with the profiles it resolves to
- [ ] #5 check warns about a user profile whose last path segment is only wildcards outside /tmp and /var/tmp
- [ ] #6 An invalid config exits 2 and deletes nothing, and check reports the same error
- [ ] #7 check works outside a container (the sandbox gate only guards deletion)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: crates/diskgc/src/main.rs runs the minimum-kernel check (startup()) for every invocation except --version/--help. docs/safety.md (Path traversal, Minimum kernel) also exempts 'check', which is machine-independent. Add 'check' (and 'profiles', if it stays machine-independent) to the exemption when they land, with a test.

Forward-carried from task-3 review (mped): main.rs currently applies the kernel >= 5.8 check to every command; docs/safety.md says check is machine-independent, so check must be exempt from the kernel check.

Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/): the config schema needs the socket/FIFO policy decided on task-5 (block on any vs only live ones). Also, the example config's idle thresholds were the only ones the dogfood probe could use (the catalog defines no defaults); the simulated reclaim moved from 27G to 120G between idle 30d and 1d, so the defaults matter.

From task-5 (deletion engine): the user's dogfooding decision made contains_ipc a policy, now a flag of the walk: diskgc_core::engine::Rules::block_ipc (true, the default: any socket or FIFO keeps its candidate; false: the walk records them in WalkRecord::ipc_entries() and the in-use check of task-6 refuses the live ones). docs/safety.md (eligibility walk) describes both and says only the default is reachable until the schema defines the setting: name the key here (in docs/reference.md, Configuration), default = block any. The PRD already carries the wording (Safety boundaries and What bad looks like, edited during task-5's review: a socket or FIFO deleted under the default policy, or, under any policy, one a visible process holds open or has bound); keep the config text consistent with it.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
