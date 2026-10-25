---
id: TASK-195
title: >-
  Gate-lift feature can be built without the release catalog (ungated
  dev-catalog binary)
status: To Do
assignee: []
created_date: '2026-10-25 05:59'
labels:
  - safety
  - release
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). diskgc-linux/release-without-sandbox-gate is independent of proposed-release-01-catalog; only a text test on flake.nix couples them, so an ad-hoc cargo build yields an ungated binary with the full development catalog. Fix: export a SANDBOX_GATE const and a const assert in main.rs tying the two features.
<!-- SECTION:DESCRIPTION:END -->
