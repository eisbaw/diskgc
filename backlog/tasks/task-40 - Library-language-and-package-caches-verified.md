---
id: TASK-40
title: 'Library: language and package caches, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 11:12'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
