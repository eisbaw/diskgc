---
id: TASK-130
title: >-
  Vector runner: config parsing, release catalog and account lookup through one
  product path
status: Done
assignee: []
created_date: '2026-10-23 22:39'
updated_date: '2026-10-24 20:48'
labels:
  - verification
  - architecture
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-121 review (mped-architect) findings 1, 5, 6. (1) main.rs load_config parses the config and, under feature proposed-release-01-catalog, runs enforce_proposed_release_01_config; the vector runner calls config::parse itself and synthesises the exit-2 report, and no vector can run the release catalog that would ship. Move 'config bytes -> Config' with an explicit catalog value (Development | ProposedRelease01) into diskgc_core::invocation, route its refusal through ensure's single refusal path, and let a vector say catalog: proposed-release-01. (5) The 'no configuration and no --select' refusal is decided twice (main.rs ensure_config with the tried paths, and invocation::run); keep one owner. (6) Account lookup (owners: names) is a machine read outside the Host port; move it onto the port so vector 'users' becomes host-model state. Reversible; no deletion decision changes.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented 2026-10-24 on branch task130 (commits 9810c7f + review fixes 8d9ddc9). diskgc_core::invocation now owns 'config bytes -> Config' under an explicit Catalog {Development, ProposedRelease01} (Catalog::config, Catalog::library: the only places the catalog is applied; ensure, profiles, check and --help use them). ensure takes ConfigFile {Read{path,bytes} | NotFound{tried}}; parse/catalog/no-config/--allow-root refusals are decided once in ensure's refusal path (main.rs ensure_config removed; 'tried ...' list kept). owners: names resolve via new Host::account_uid (LinuxHost getpwnam_r, ModelHost host.users); system-uid policy moved to invocation (config::SYSTEM_UID_END). Vector format: users -> host.users; new catalog: key. Vectors: 7 under tests/vectors/release/ (cargo-target via --select and configured groups reclaim; dev-only pytest-cache refused by --select and in a later group; withdrawn zephyr-tmp refused; operator profile and cargo-target override refused at load); release_vectors_are_exactly_the_proposed_catalog_ones ties the area to the key; a None library in the runner now requires the run to refuse. Bite: un-narrowed Catalog::library and skipped 0.1 config recheck each fail release vectors. Reviews: mped-architect GO (batch), qa-test-runner GO at 8d9ddc9. Follow-ups: Catalog/ConfigUse could move to profile/config; musl NSS hint text lives in core; --select refusal under the 0.1 catalog says 'no profile or filter is named X' without naming the catalog (QA note).

Merged to master (fast-forward) at 5e81db1 after nix develop -c just e2e exit 0 (2026-10-24T20:47Z; see git note on 5e81db1).
<!-- SECTION:NOTES:END -->
