---
id: TASK-172
title: >-
  Cargo reclaim cost: each fingerprint is its own part with a mountinfo reread
  and profile statx before it
status: To Do
assignee: []
created_date: '2026-10-25 01:14'
labels:
  - hardening
  - performance
dependencies: []
references:
  - crates/diskgc-core/src/cargo_target/reclaim.rs
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, read-only; not measured). Before each part locks_unchanged calls profile_dir (mountinfo reread + statx of every profile entry), and every fingerprint is a part; plus a /proc rescan every 500 ms. On a large per-unit target on a busy host the 3 shared fresh attempts (TASK-144) can run out part-way after all fingerprints are gone: little space freed and a full rebuild next time. Measure locks_held vs part count on a large target under walk load; consider one part for all of a profile's fingerprints and one mountinfo read per target.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 locks_held and part count measured on a realistic large target under host-shaped load; design changed or the cost accepted with numbers
<!-- AC:END -->
