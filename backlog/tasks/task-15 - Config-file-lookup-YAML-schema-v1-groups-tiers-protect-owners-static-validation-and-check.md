---
id: TASK-15
title: >-
  Config file: lookup, YAML schema v1, groups/tiers/protect/owners, static
  validation and check
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:55'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
