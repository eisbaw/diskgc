---
id: TASK-74
title: 'Predicate: application not running (Chromium-family SingletonLock)'
status: Done
assignee: []
created_date: '2026-10-18 20:53'
updated_date: '2026-10-20 03:53'
labels:
  - home
  - wave-4
  - irreversible
dependencies:
  - TASK-8
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the in-use check does not protect a running browser's disk cache. A running Brave held no fd into its Cache or Code Cache, so diskgc could delete a live browser's cache. Chromium's SingletonLock symlink in the profile config directory is the guard. The predicate holds only when that lock is safely confirmed absent; any present lock, including a stale-looking PID, keeps the cache because PID namespaces prevent a reliable liveness conclusion. Unreadable lock state fails closed. Used by Chromium-family cache profiles. Source: read-only home survey 2026-10-18.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Verified against a running and a stopped Chromium-family browser (container, or read-only on a real machine)
- [x] #2 docs/safety.md states why the in-use check is not enough here
- [x] #3 An absent SingletonLock makes the predicate true; every present, stale-looking, foreign or unreadable lock makes it false (model tests)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Read-only real-host Brave dry-run 2026-10-20: two cache candidates both skipped in_use because .config/BraveSoftware/Brave-Browser/SingletonLock named live local PID 4071596. New loader rejects a present null/blank app_not_running so user overrides cannot silently disable the guard; model test passes. Real-kernel live/stopped PID container E2E is in progress; no live host deletion.

2026-10-20 shipped in 98365a7: model live/stale/foreign/unreadable lock tests; real-kernel Brave live/stale cache E2E (Nix and Debian focused) and full Nix just e2e 104/104; command action final lock recheck regression proved red when removed; QA and MPED GO. Safety docs state same-PID-namespace requirement and startup race.

2026-10-20 superseded by 7cd329e: same-host dead-PID inference was unsafe across PID namespaces. Current implementation retains caches for every present SingletonLock, including stale-looking locks; only confirmed absence permits the profile. Prior 98365a7 note records historical behavior, not current contract. Model and container tests cover present versus absent locks.
<!-- SECTION:NOTES:END -->
