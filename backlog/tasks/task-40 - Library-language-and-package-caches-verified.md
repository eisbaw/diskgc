---
id: TASK-40
title: 'Library: language and package caches, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-18 12:02'
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
- [ ] #3 Profiles whose tool cannot be verified offline in the harness (e.g. Gradle, Maven) stay planned, not shipped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/). Missing language/package profiles (sizes on this machine): bun-cache (~/.cache/.bun, 3.6G; bun pm cache rm), go-mod-cache (~/go/pkg/mod, 2.6G, read-only tree; go clean -modcache or delete with make_writable), cargo-registry (~/.cargo/registry/src and cache, ~1.7G), python-venv (.venv with pyvenv.cfg, 5.5G), node-modules (node_modules with .package-lock.json, 3.9G), mypy/ruff caches (CACHEDIR.TAG, 1.0G), lean lake-build (.lake/build, 1.3G), browser-cache (~/.cache/BraveSoftware etc., 1.8G), huggingface-cache (~/.cache/huggingface hub models/datasets, 1.05G; pressure tier), xdg-cache-entry (generic ~/.cache/* with a long idle filter). Proposed definitions in catalog format are in the dogfood report.

From task-68: none of jest's, PlatformIO's, Gradle's or Maven's caches exists on the laptop (dry runs found 0). jest's default cacheDirectory is $TMPDIR/jest_<x>, so jest-cache wants fixed roots (/tmp and TMPDIR containers, task-69), not roots: required. Command-action rows (pip, go-build, npm, ccache, uv, pnpm, yarn) are written in crates/diskgc-core/src/fs_kind/spike-profiles.yaml and load (static rules), never run. has-cachedir-tag as a probe (any dir <= 3 deep below $HOME and $HOME/topics holding CACHEDIR.TAG, 38 s): 185 eligible, 7.85 GiB: uv's .venv (37), .pytest_cache (56), .ruff_cache (47), .mypy_cache (19), per-project .cargo/registry and ~/.cargo/registry (1.7 GiB), fontconfig (in use). A broad "rebuildable" signal, but deleting a .venv costs a re-sync; decide whether it is a filter for pressure tiers only.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
