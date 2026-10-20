---
id: TASK-40
title: 'Library: language and package caches, verified'
status: In Progress
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-20 03:37'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-39
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: per-tool caches need the tool's own cleaner pointed at the right directory. Ship pip-cache, uv-cache, go-build-cache, npm-cache, pnpm-store, yarn-cache, jest-cache, ccache, gradle-caches and maven-repo, each command explicitly naming the cache location.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each command action measurably cleans the directory diskgc measured (container test per tool)
- [ ] #2 Each profile has examples, a golden set and verified_with
- [x] #3 Profiles whose tool cannot be verified offline in the harness (e.g. Gradle, Maven) stay planned, not shipped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/). Missing language/package profiles (sizes on this machine): bun-cache (~/.cache/.bun, 3.6G; bun pm cache rm), go-mod-cache (~/go/pkg/mod, 2.6G, read-only tree; go clean -modcache or delete with make_writable), cargo-registry (~/.cargo/registry/src and cache, ~1.7G), python-venv (.venv with pyvenv.cfg, 5.5G), node-modules (node_modules with .package-lock.json, 3.9G), mypy/ruff caches (CACHEDIR.TAG, 1.0G), lean lake-build (.lake/build, 1.3G), browser-cache (~/.cache/BraveSoftware etc., 1.8G), huggingface-cache (~/.cache/huggingface hub models/datasets, 1.05G; pressure tier), xdg-cache-entry (generic ~/.cache/* with a long idle filter). Proposed definitions in catalog format are in the dogfood report.

From task-68: none of jest's, PlatformIO's, Gradle's or Maven's caches exists on the laptop (dry runs found 0). jest's default cacheDirectory is $TMPDIR/jest_<x>, so jest-cache wants fixed roots (/tmp and TMPDIR containers, task-69), not roots: required. Command-action rows (pip, go-build, npm, ccache, uv, pnpm, yarn) are written in crates/diskgc-core/src/fs_kind/spike-profiles.yaml and load (static rules), never run. has-cachedir-tag as a probe (any dir <= 3 deep below $HOME and $HOME/topics holding CACHEDIR.TAG, 38 s): 185 eligible, 7.85 GiB: uv's .venv (37), .pytest_cache (56), .ruff_cache (47), .mypy_cache (19), per-project .cargo/registry and ~/.cargo/registry (1.7 GiB), fontconfig (in use). A broad "rebuildable" signal, but deleting a .venv costs a re-sync; decide whether it is a filter for pressure tiers only.

2026-10-20 TASK40 implementation in /tmp/diskgc-lang-cache (uncommitted). Seven command built-ins: pip-cache, uv-cache, go-build-cache, npm-cache, pnpm-store, yarn-cache, ccache. Exact paths and argv have a core assertion; all seven have positive and negative examples, model golden match sets, and verified_with. Nix-pinned cleaner versions actually run: pip 26.1.2, uv 0.13.0, Go 1.26.8, npm 11.19.0 on Node 24.21.0, pnpm 12.9.0, Yarn Classic 1.22.22, ccache 4.14.1. Focused real Nix container cleaner tests passed 7/7: each invokes the product command on its measured cache, reduces its bytes, and passes the deletion oracle. pnpm uses pnpm store add of an offline local package tarball; a fabricated file was correctly ignored by prune. uv needed cwd parent because uv refuses to clean a cache from inside that cache. Model bite trials: for each of the seven profiles, mutating only its find.type from dir to file made command_cache_builtins_have_golden_match_sets red (exit 101); restoring all definitions made the focused golden green. nix develop -c just check passed after the fixes.

Real-host read-only diskgc ensure --dry-run --no-index on 2026-10-20, using the same Nix-pinned tool PATH and /home/mpedersen as both root and mount: pip-cache 1 candidate eligible 504549376 B; uv-cache 1 candidate skipped contains_checkout; go-build-cache 1 eligible 464199680 B; npm-cache 1 candidate skipped contains_checkout; pnpm-store 0 candidates; yarn-cache 0 candidates; ccache 1 eligible 90648576 B. All seven dry runs exited 0 with no report errors. No host deletion occurred. The uv/npm checkout skip is an intentional conservative limit, now documented in docs/reference.md. Jest, Gradle and Maven command profiles remain planned: no offline tool/layout verification here; no claimed verification or shipped definitions for them.

2026-10-20 integrated seven verified cleaners in commit 03467ea. Final nix develop -c just check passed and nix develop -c just e2e passed 114/114; QA and MPED reviews returned GO. Each of pip, uv, Go build, npm, pnpm, Yarn, and ccache had a real cleaner/container run with positive measured available-space delta and deletion oracle clean. Jest, Gradle and Maven remain unshipped and unverified; this task remains In Progress for those named profiles. Host dry runs made no deletion.

2026-10-20 Jest follow-up in /tmp/diskgc-task40-remaining (uncommitted, pending integration): npm lock pins Jest package 30.2.0; Nix npmDepsHash pins the closure. Pinned CLI showConfig default cache is /tmp/jest_rs. New fixed-root jest-cache uses the temp-prefix v1 preset and passes the exact candidate to jest --config {} --cacheDirectory {path} --clearCache from its parent; the empty CLI config avoids loading project JS config. Core golden and model discovery tests passed. Focused real Nix container test passed 1/1: exact measured cache deleted, available bytes increased, deletion Oracle clean, decoy outside candidate preserved. Full just check passed before the additional model test; that focused model test passed after. Gradle remains planned: official Gradle directory-layout docs describe retention-based automatic cleanup after builds, not an exact whole ~/.gradle/caches command. Maven remains planned: official dependency:purge-local-repository is scoped to a project dependency graph, not the measured whole ~/.m2/repository. Neither has a verified exact-path whole-repository cleaner; no unsafe profile shipped. No host deletion.

2026-10-20 integrated pinned Jest 30.2.0 cleaner in c8ade1b. Focused Jest E2E passed exact measured-cache deletion with positive space delta, deletion oracle clean and decoy preserved; final nix develop -c just e2e passed 119/119; QA and MPED reviews GO. Gradle and Maven remain planned because their documented cleanup does not bind a whole measured cache/repository. No host deletion. Task stays In Progress for those named profiles.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
