---
id: TASK-146
title: >-
  Docs vs 0.1 binary: the reference config example and its built-ins are
  refused, with jargon errors
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 00:54'
labels:
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (laptop owner journey). A laptop owner copies the configuration example in docs/reference.md (Configuration file) into ~/.config/diskgc/config.yaml with their roots: the 0.1 release binary refuses it, one error per run:
- 'proposed 0.1 catalog refuses operator-authored profile `claude-sessions` (including overrides)'
- 'proposed 0.1 catalog refuses group `stale` tier `tmp-nix-shells`: invalid selection `profile:nix-shell-rc`: `profile:nix-shell-rc`: no profile or filter is named `nix-shell-rc`' (same for pytest-tmp), while reference.md's library and liveness table list them as Shipped.
Also: reference.md (protect) says 'The example config protects claude-sessions'; the packaged share/doc/diskgc/examples/example.yaml has no protect and no user profile.
Pane dumps: tmp/walks/2026-10-24/20-laptop-config-from-reference.txt, tmp/walks/2026-10-24/21-laptop-config-iterate.txt, tmp/walks/2026-10-24/14-ci-example-config.txt; the config used: tmp/walks/2026-10-24/laptop-from-reference.yaml (copied into the walk home).

Fix: (1) messages in user words: 'not available in diskgc 0.1 (0.1 ships cargo-target and the idle filters; see the release notes)', no 'proposed'/'operator-authored'; 'no profile named X' must not be said of a documented built-in that the catalog excludes (TASK-139 covers the --select path); (2) docs: mark in the config example and the library which parts are not in 0.1, or give a 0.1 example; make the protect paragraph match example.yaml. Coordinate with TASK-65 (/tmp catalog), which changes what 0.1 admits.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 docs-setup branch (908fe3f, walk fix 5th commit): withheld built-ins say 'is a built-in that diskgc 0.1 does not ship' (tiers, protect, profile_policies, --select: also covers TASK-139); user profiles listed at once; config refusals collected in one message with the 0.1 scope said once. reference.md: 0.1 config example + 'Development build only' label; regression test documented_config_examples_load_under_the_catalog_they_claim. Walked: tmp/walks/2026-10-25-docs-setup/01..08 (0.1 example, dev example under 0.1 and dev builds, tier fragment, packaged example, --select).

Merged to master as d2c0554 on 2026-10-25T00:54Z (branch docs-setup, rebased; just check exit 0). Orchestrator accepted two deviations: JSON cause named 'provisioning' (fault class, matches exit-2 wording); 'check' warns on a missing supplied root with exit 0 (check is machine-independent; build-time checks), while ensure exits 2.
<!-- SECTION:NOTES:END -->
