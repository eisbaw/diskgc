---
id: TASK-349
title: >-
  nix-shell-rc: a read-only directory inside an rc dir fails the delete with
  EACCES (exit 3, candidate left partly deleted)
status: To Do
assignee: []
created_date: '2026-11-05T07:04:00Z'
labels:
  - dogfood
  - profile
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Dogfood 2026-11-05, same run (a4-rc-real.err line ~7300): /tmp/nix-shell-3406072-3109443335 (25 entries, newest change 67 d ago) contains task…/primary/raw/artifacts with mode dr-xr-xr-x (owned by the user; typical of files copied out of the Nix store); unlinkat of its entries -> EACCES; the run continued but ended status error, exit 3; the candidate is left partly deleted. nix-shell-rc has make_writable: false. Decide: should nix-shell-rc (and other /tmp profiles whose producers copy Nix store trees) use make_writable (u+w on the user's own directories only, as the cargo/module-cache profiles do)? And: is a partly deleted candidate + exit 3 the documented outcome for EACCES inside a candidate (safety.md, reference.md exit codes)? Model vector with a read-only subdir for nix-shell-rc.
<!-- SECTION:DESCRIPTION:END -->
