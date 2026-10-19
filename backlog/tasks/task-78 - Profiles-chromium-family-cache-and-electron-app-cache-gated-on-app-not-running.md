---
id: TASK-78
title: >-
  Profiles: chromium-family-cache and electron-app-cache (gated on app not
  running)
status: To Do
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-18 23:16'
labels:
  - safety
  - wave-4
  - home
dependencies:
  - TASK-18
  - TASK-74
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: browser and Electron caches (Brave Default/{Cache, Code Cache} 1.8 GiB; Edge; Firefox cache2; ~/.config/{Cursor, teams-for-linux, ...}/{Cache, CachedData, CachedExtensionVSIXs} 0.3 GiB) are regenerable, but the in-use check does not protect them (see the app-not-running predicate task). Only the named cache subdirs of each profile, never a browser profile dir; Service Worker CacheStorage and WebStorage are site data, not cruft. Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Profiles match only named cache subdirs per browser profile (examples incl. negative ones for profile data and Service Worker storage)
- [ ] #2 Gated on the app-not-running predicate: a running browser makes its cache ineligible (test)
- [ ] #3 A read-only dry run here lists sizes matching du
<!-- AC:END -->
