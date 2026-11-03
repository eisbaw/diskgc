---
id: TASK-78
title: >-
  Profiles: chromium-family-cache and electron-app-cache (gated on app not
  running)
status: Done
assignee: []
created_date: 2026-10-18 20:54
updated_date: '2026-11-03T00:01:00Z'
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

2026-10-20 disposable Debian trixie container (Firefox ESR 153.5.0esr) lock evidence: a live headless default profile created ~/.cache/mozilla/firefox/<id>.default-esr/cache2 and ~/.mozilla/firefox/<id>.default-esr/.parentlock. For an explicit profile, live Firefox held a POSIX fcntl exclusive write lock on .parentlock: another process's F_SETLK returned EAGAIN (11). After SIGTERM, .parentlock persisted and F_SETLK succeeded; the old 'lock' symlink disappeared. With .parentlock mode 0444, same-user Firefox still ran using a 'lock' symlink (fallback), target without '+'. A same-user process holding F_SETLK caused a second Firefox launch to print 'Firefox is already running' and exit. Mozilla nsProfileLock.cpp confirms the fcntl-first/symlink-fallback protocol and that failure to create the symlink can be ignored after fcntl succeeds. Sound Firefox cache2 deletion therefore needs an O_WRONLY, no-follow .parentlock FD with nonblocking POSIX F_SETLK held through every deletion, plus an absent fallback 'lock' entry and exact cache-profile mapping. A mere symlink absence or host /proc PID scan is insufficient; UID, inode/path replacement, root privilege asymmetry, and fallback races need explicit coverage. Electron's app.requestSingleInstanceLock() is optional per its API and surveyed Cursor/teams-for-linux roots have no SingletonLock, so neither Electron cache profile has a demonstrated app-wide liveness guard. No new profile shipped.

2026-10-20 Firefox ESR investigation: Disposable Debian ESR 153.5.0esr used ~/.mozilla/firefox/<name>/.parentlock; pinned Nix ESR 153.5.0esr used ~/.config/mozilla/firefox/<name>/.parentlock. Both used same-named ~/.cache/mozilla/firefox/<name>/cache2. A held Linux OFD write lock conflicts with the live POSIX .parentlock lock. Focused pinned ESR container E2E passed live-cache retention, fallback-lock retention, stopped-cache deletion, and site-data preservation. ModelHost also verified exact-one legacy/XDG binding and ambiguity refusal. BLOCKER: Firefox can switch to fallback lock symlink when .parentlock becomes unreadable. The ModelHost event at unlinkat(before), after the final fallback-entry check, created lock while reclaim still completed and deleted cache2. Same-user permission change can trigger this real Firefox branch; repeated checks cannot atomically exclude it. A sound protocol needs a durable guard that excludes BOTH .parentlock locking and fallback lock creation throughout every unlink, or a supported Firefox cache-clearing API that runs in the browser. Do not ship firefox-cache2 or the custom firefox_profile_lock predicate until this race is closed. Electron remains blocked on app-specific reliable lock evidence.

2026-11-02: Mark: 'yes faster' (one review round, model-level blockers only, merge to dev library). In progress in tmp/worktrees/t78.

2026-11-03: merged teams-for-linux-electron-cache and figma-linux-electron-cache to the development library. Follow-ups: container tests with the real apps (lock exists while running; deletion once stopped); the shutdown-order gap (Electron drops SingletonLock before sessions close) for the browser profiles too; Firefox cache2 lock race; Claude desktop when installed.
<!-- SECTION:NOTES:END -->
