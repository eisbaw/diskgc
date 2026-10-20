---
id: TASK-78
title: >-
  Profiles: chromium-family-cache and electron-app-cache (gated on app not
  running)
status: To Do
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-20 03:05'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Read-only host dry runs 2026-10-20 on browser candidate patch: Brave 2 candidates, 0 eligible, both in_use via live PID 4071596 SingletonLock; Chrome 0 candidates; Chromium 0; Edge 2 eligible, allocated estimates 139857920 and 54329344 bytes, exactly matching du -sB1 for both. Brave ineligible cache directories had 1237856419 and 548114278 apparent bytes by du -sb; no deletion. All four built-in match sets and live-lock refusal now have model golden coverage, with real-kernel E2E pending.

Version provenance for shipped browser definitions: `brave --version` reported Brave Browser 154.1.96.61. Host profile `Last Version` files reported Google Chrome 132.0.6834.110, Chromium 145.0.7632.159, Microsoft Edge 132.0.2957.127; only Brave executable is installed now. Chrome/Chromium 2026-10-20 dry runs found no cache candidates, so their real-host positive match is unverified; model fixtures cover positive layouts. Edge has two positive real-host candidates.

Profile DoD mutation trial: `browser_cache_builtins_have_golden_sets_and_refuse_live_locks` failed when Brave Default/Cache was widened to Cache* (Cache.backup became eligible), when type dir became any (Profile 2/Cache regular file became eligible), and when app_not_running was removed (live lock caches became eligible). Restored original YAML after each mutation; baseline focused model test passed. The table-driven fixture exercises all four shipped Chromium-family definitions.

2026-10-20 four Chromium-family browser profiles shipped in 98365a7 after full nix develop -c just e2e 104/104 and QA/MPED GO. Focused Nix and Debian Brave live/stale lock container tests passed; only named Cache and Code Cache directories are reclaimed. TASK78 stays To Do: surveyed Cursor/teams-for-linux Electron roots had no usable SingletonLock, Firefox cache2 needs a different liveness guard, and Chrome/Chromium positive host matches remain unverified. Running with a host home bind-mounted into a separate PID namespace is outside the guard contract (docs/safety.md).

2026-10-20 safety correction committed as 7cd329e: the shipped Chromium-family app_not_running guard now treats every present SingletonLock entry as in_use, regardless of PID text; only a safely confirmed absent lock permits deletion. Real private-PID-namespace Firefox proof showed a live lock PID invisible to host /proc. Brave real-kernel E2E now covers live, stale and absent lock phases; full nix develop -c just e2e passed 114/114 with parallel QA and MPED GO. Firefox and Electron profiles remain unshipped: normal Firefox exit left a lock symlink, and no reliable app-root lock was verified for Electron apps.
<!-- SECTION:NOTES:END -->
