---
id: TASK-359
title: >-
  Admit five project-tree profiles into the 0.2 catalog (pytest-cache,
  node-compile-cache, mypy-cache, python-pycache, python-venv)
status: Done
assignee: []
created_date: '2026-11-05T19:17:00Z'
updated_date: '2026-11-06T00:13:00Z'
labels:
  - admission
  - release
  - '0.2'
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-05: 'admit all' after TASK-350's evidence (node-modules held back pending his reply: its successor TASK-357). Conditions accepted with 'admit all': mypy-cache (ratify the 2026-10-24 mark-emulator admission; write phase untested), python-pycache (Git-tracked .pyc residual; TASK-358 open as an optional successor), python-venv (rule-2 narrowing; activated-shell-after-cd and hand-installed-package residuals). Do as TASK-335 did for nix-garbage: add the 0.2 label to these five in builtin.yaml, liveness/library rows and release-notes draft (what 0.2 adds, known limits = each profile's named residuals), release-candidate --help/profiles list, docs-vs-binary, release suites exercising them through the gate-lifted package where possible (TASK-350 harness; note the 30-day floor needs the clock offset), bite (remove a label -> tests fail).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-05 orchestrator: implementer in tmp/worktrees/tadmit (branch t-admit-project-tree).

Merged de9f2af13b457e011eb56041cc62256a211d1224 (2026-11-06): pytest-cache, node-compile-cache, mypy-cache, python-pycache, python-venv, node-modules in the 0.2 catalog. Open for Mark: the npm install --no-save residual (packages not in the manifest are not reinstalled) is named for node-modules and marked unconfirmed (TASK-357). Busy-host test flake: TASK-363.
<!-- SECTION:NOTES:END -->
