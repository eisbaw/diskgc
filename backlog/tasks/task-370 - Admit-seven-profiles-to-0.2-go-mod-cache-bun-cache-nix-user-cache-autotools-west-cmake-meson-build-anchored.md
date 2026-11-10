---
id: TASK-370
title: >-
  Admit seven profiles to 0.2: go-mod-cache, bun-cache, nix-user-cache,
  autotools-, west-, cmake-, meson-build-anchored
status: Done
assignee: []
created_date: '2026-11-07T17:30:00Z'
updated_date: '2026-11-10T15:27:00Z'
labels:
  - admission
  - release
  - '0.2'
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-07 answers: runtime caches 'go-mod, bun, nix-user'; build outputs 'autotools, west'; TASK-367 option 1 (general nested-build guard) then cmake/meson. Evidence: TASK-352 (fbf47d34's predecessors 4afba051/cc38ce9d), TASK-354 (fbf47d34). Do as TASK-359/360: 0.2 labels, kind/action rules (go-mod-cache uses make_writable delete; check whether 0.2's rules accept it), docs (library rows, liveness verdicts, release notes 'what 0.2 adds' + Known limits per profile in user wording), release-suite runs through the gate-lifted package (reuse runtime_cache_suite/build_output_suite; note west is full-suite only), candidate-catalog list updated, bite. Not admitted: huggingface-hub-repos, electron-download-cache, node-gyp-headers, cargo-registry-src, buildroot-, yocto-, bazel-output-base-anchored.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-07: implementer in tmp/worktrees/t370 (branch t370-admit-seven), together with TASK-367 (guard first).

2026-11-10 merged to master in ce284125 (local, not pushed). Guard consolidated in fbed4618; gates: just check exit 0, just e2e exit 0 on fbed4618; Codex round 6 found no single-fault bypass; docs follow-up af720377; hardening TASK-372, TASK-373.
<!-- SECTION:NOTES:END -->
