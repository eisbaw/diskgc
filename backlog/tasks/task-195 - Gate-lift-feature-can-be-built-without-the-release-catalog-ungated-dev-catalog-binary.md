---
id: TASK-195
title: >-
  Gate-lift feature can be built without the release catalog (ungated
  dev-catalog binary)
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 09:55'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-c (755a066, 15cb544): diskgc_linux::sandbox::SANDBOX_GATE + const assert in crates/diskgc/src/main.rs (SANDBOX_GATE || proposed-release-01-catalog); cargo check -p diskgc --features diskgc-linux/release-without-sandbox-gate fails E0080 naming the catalog feature. Flake check release-gate-needs-catalog (in just e2e / e2e-full): gate-lift alone must fail on the assert message, the release pair must compile; mutation-checked (assert neutralised -> check fails). sandbox_gate_flag_is_enabled_nowhere allows exactly the check's two lines. Scope: guards the shipped diskgc binary, not --all-features test builds of other crates or diskgc-e2e (safety.md). Awaiting merge.

Merged to master as 9f317cb on 2026-10-25T09:54Z.
<!-- SECTION:NOTES:END -->
