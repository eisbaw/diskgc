---
id: TASK-15
title: >-
  Config file: lookup, YAML schema v1, groups/tiers/protect/owners, static
  validation and check
status: In Progress
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-22 01:51'
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

From task-6: the IPC policy maps to two values the config must set together: engine::Rules::block_ipc (walk) and inuse::Options::sockets (ask the kernel's socket diagnostics). 'live only' = block_ipc false + sockets true. inuse::Options::users must be the same list as Rules::owners (diskgc's uid, or owners: under --allow-root): it decides which unreadable processes are 'the user's' (docs/safety.md, In use).

From task-6 round 2: use inuse::Options::from_rules(&rules) rather than setting users/sockets by hand; delete_unused refuses options that disagree with the walk's rules.

From task-7 (selection grammar): each tier's select (and --select) goes through diskgc_core::select::compile(text, &catalog) -> Result<Checked, SelectError>; the catalog is task-8's implementation of select::Catalog. SelectError Display is the full message (syntax: 'invalid selection `EXPR` at column N: ...'; rules: 'invalid selection `EXPR`: e1; e2'); prefix it with the tier name. check prints Checked::expr() (Display is the normalised form, docs/reference.md examples table) and can list the profiles from expr().terms (source and NotProfile factors). Checked::kind() is the expression's one kind. YAML caveat: a select value starting with '!' or '&' is a YAML tag/anchor; such expressions are invalid anyway (a term starts with profile:), but the YAML error will come first. Roots (tier roots apply only to roots: required sources; NotProfile takes the tier's roots) are not checked by select; that is task-8/15.

From the task-7 review (H7): config/--select errors in --json need stable error codes (one per Syntax and CheckError variant); document in docs/reference.md that machines match codes, not text. Also: compile errors now quote the expression as written (CheckErrors.expr), so prefix with the tier name only.

From task-8 (profile model, diskgc-core profile): embed the config's profiles as Vec<profile::ProfileSpec> (implements Deserialize; unknown and repeated keys rejected) and build the catalog with Library::with_user_profiles(&specs) -> Result<Library, LoadErrors>; each LoadError's 'at' is 'profiles[i] (`name`)'. Library implements select::Catalog: compile(tier.select, &library). Tier roots: library.bind_roots(&checked, tier.roots.as_deref()) -> Err(RootsError::Missing|Unused|Empty|Path) is the AC #1 load error (prefix the tier name). protect entries: library.roots_of(name, roots) (bare name of a required profile -> Missing; {profile, roots} of a fixed one -> Unused); the 'only kinds fs and cargo-target can be protected' rule is yours. units::parse_duration exists for tier timeout (sizes do not). reference.md now documents the profile fields (Profiles table) and that command actions have no timeout of their own: the tier's timeout bounds them.

From task-10: diskgc-core/src/config.rs has the lookup (pure, tested) and a minimal version: 1 parse (deny_unknown_fields; profiles loaded through Library::with_user_profiles; non-empty protect and any owners refused; groups accepted unread as IgnoredAny, NOT validated). Extend ConfigSpec rather than adding a second loader; remove the protect/owners refusals as they get implemented (task-16, task-21). The binary reads the file through LinuxHost::read_config_file (follows symlinks, regular files only, MAX_CONFIG_BYTES = 1 MiB). check is not implemented yet (exit 2).

From task-80: docs/reference.md (User profiles) now also asks check to warn about every user profile that says checkouts: allow (its candidates may be repositories with uncommitted work). The broad-pattern warning does not exist in code yet, so both land here, each with a test.

From task-80 QA r3: reference.md says an example config ships as docs/example.yaml, but that file does not exist yet.

2026-10-20 isolated TASK-15 implementation: typed version-1 groups, ranking, tiers/order/timeout, roots, protect, owners and IPC policy; all groups validated before ensure action, check machine-independent with normalized tier output and warnings. Targeted config tests and CLI tests pass; nix develop -c just check exit 0; Nix sandbox check derivation exit 0. A real ensure still intentionally refuses nonempty protect and ipc: live-only until TASK-16/21, config groups execution until TASK-20, and --allow-root until TASK-21. Full integrated e2e and independent reviews remain for the integration commit. Integration must switch the old-worktree config example check to profile::examples::failures(&library) from TASK-17.

2026-10-20 integration checkpoint on 179ebb3: typed config/check plus pure task16 evaluator, nix develop -c just check PASS, nix develop -c just e2e PASS 90/90. Config example validation is strict for check/ensure; profiles test loads examples lax to report mismatch exit 1. UserOverride warnings regression added. Runtime group execution/protect/ipc live-only/root mode remain task16/20/21; task stays To Do. QA/MPED final review pending.

Final checkpoint gate: parallel QA and MPED re-reviews GO after override-warning and protection-root fixes; both regressions pass. Task remains open for runtime ACs.

2026-10-22: Added real-binary disposable-container regression invalid_config_never_falls_back_or_reclaims: --config typo overrides a valid DISKGC_CONFIG and search config; DISKGC_CONFIG typo overrides a valid search config; both check and ensure exit 2 without deletion; malformed existing config gives the same check/ensure error. Focused nix develop -c just e2e-one invalid_config_never_falls_back_or_reclaims passed 1/1. AC3 static-rule coverage audit and integrated gate/review still pending; do not mark complete yet.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
