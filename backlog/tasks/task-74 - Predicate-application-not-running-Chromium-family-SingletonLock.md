---
id: TASK-74
title: 'Predicate: application not running (Chromium-family SingletonLock)'
status: To Do
assignee: []
created_date: '2026-10-18 20:53'
updated_date: '2026-10-18 23:16'
labels:
  - home
  - wave-4
  - irreversible
dependencies:
  - TASK-8
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the in-use check does not protect a running browser's disk cache. A running Brave held no fd into ~/.cache/BraveSoftware/*/Cache or Code Cache (only GPUCache/Dawn maps), so diskgc would delete a live browser's cache: no data loss, but the cache is confused until restart. Chromium's own guard is the SingletonLock symlink (hostname-pid) in the profile's config dir. Add a predicate that holds only when the application's lock is absent or names a dead pid on this host; fail closed when unreadable. Used by the browser and Electron cache profiles. Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A live SingletonLock (this host, live pid) makes the predicate false; a stale one (dead pid) true; unreadable or another host fails closed (model tests)
- [ ] #2 Verified against a running and a stopped Chromium-family browser (container, or read-only on a real machine)
- [ ] #3 docs/safety.md states why the in-use check is not enough here
<!-- AC:END -->
