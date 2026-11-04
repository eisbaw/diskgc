---
id: TASK-341
title: >-
  Nix GC e2e: prove --max-freed stops Nix (garbage > need) and read the bound
  from the executed argv
status: To Do
assignee: []
created_date: '2026-11-04T18:59:00Z'
labels:
  - nix
  - e2e
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of TASK-335 (2026-11-04), P2: nix_gc_suite keeps_every_kind_of_root has ~120 KiB of garbage against a ~1 MiB need and max_freed() reads diskgc's own log, so an unbounded GC would pass. gc_argv is pinned by a unit test and model vectors, so this is an e2e coverage gap. Do: a scenario with garbage well above the need, asserting collection stops within Nix's last-path overshoot, and read --max-freed from the executed nix-store argv (e.g. /proc of the child or a wrapper the image records), on the gate-lifted release binary. Also from the implementer's follow-ups: release-0.2 model-vector catalog with a nix-garbage vector; plan row checkouts: refuse for nix-store vs profiles show null; watch --tally-only with a nix-garbage tier on the package; 0.2 release criteria for the Nix backend (mutation scope, fuzz targets for its parsers, evidence key for the real multi-user GC).
<!-- SECTION:DESCRIPTION:END -->
