---
id: TASK-122
title: >-
  Classify the liveness contract of every shipped built-in source (TASK-112
  admission rule)
status: In Progress
assignee: []
created_date: '2026-10-23 16:33'
updated_date: '2026-10-24 18:02'
labels:
  - profiles
  - design
dependencies:
  - TASK-112
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: TASK-112's design (docs/safety.md Producer liveness) makes a liveness contract class (sound, producer-equivalent, verified heuristic, tolerant, delegated, none) a precondition for every built-in source; the rule's liveness table (docs/reference.md Liveness contracts) covers only the profiles TASK-112 decided. The shipped development-catalog built-ins predate the rule. Known gap: claude-code-tmp documents that an active session can hold no scratch fd (reference.md note), i.e. class none or heuristic without a recorded decision. Also review chromium-tmp, playwright-tmp, node-compile-cache, renode-tmp, zephyr-tmp, jest-cache, xdg-cache-entry-v2, the version/extension profiles and the cache/command profiles. For each: name the producer, the class with evidence (tool docs or a container probe), the residual, and either a liveness-table row or withdrawal before release. If two or more /tmp profiles want the pytest protocol's temp-root-users rule, promote it to a generic where predicate (none exists before that second user).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Every built-in source in builtin.yaml has a row in docs/reference.md Liveness contracts with its class and residual
- [x] #2 A test keeps the liveness table and builtin.yaml in sync (every source has exactly one row)
- [ ] #3 Every row of class none is withdrawn from the built-in library or explicitly recorded as a Mark decision
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-24 from TASK-112 steps 0-3: the admission rule now has a mechanism. where.liveness_contract exists (only nix-shell judged; pytest/node-project/python-project refused at load until TASK-112 steps 5/6). containers-image-tmp is withdrawn from builtin.yaml (e7c0686; re-admission TASK-125). When classifying shipped built-ins, a contract-less /tmp or cache source cannot be given a contract other than nix-shell yet. Union rule: in a selection union, every admitted source with a contract that recognises the candidate must permit it (veto).

2026-10-24 TASK-112 step 4 (557db0e): classification row for nix-shell-rc is in docs/reference.md Liveness contracts: producer legacy nix-shell + nix develop (Nix 2.34.9 container-pinned, 2.34.8 host); contract liveness_contract: nix-shell (Owner PID), min_idle_for 1h; class sound under Owner PID's preconditions; residual: a consumer outliving the shell that keeps only a path string (the 1h floor narrows it); verdict Shipped. Known imprecision (not a safety gap): a pid that is a live non-leader thread id is producer_unknown instead of in_use (TASK-131). For the sync test (AC #2): builtin.yaml now has one contract-bearing /tmp source; golden.rs frozen definition already asserts its contract and floor.

2026-10-24 TASK-112 step 5 (master dcfd410): classification row for pytest-tmp is in docs/reference.md Liveness contracts: producer pytest 9.1.1 (lifecycle pinned by pytest_lifecycle_is_pinned) + pytest-xdist 3.8.0/execnet 2.1.2; contract liveness_contract: pytest, min_idle_for 1h, checkouts: allow, one candidate per session; class verified heuristic (lock rows sound; rank row producer-equivalent or stricter; unused-root row heuristic, removes sessions above the rank pytest keeps); residuals in safety.md pytest sessions; verdict Shipped. builtin.yaml now has two contract-bearing /tmp sources (nix-shell-rc, pytest-tmp); golden.rs frozen definitions assert contract and floor for both. Host fact for the sync test / classification of others: on this laptop 6047 of 6051 eligible pytest sessions are decided by the heuristic row 11 (each nix-shell root holds one session), so pytest-tmp's value rests on that row. TASK-122's 'temp-root-users rule as a generic predicate if a second /tmp profile wants it': the rule now exists only inside the pytest contract (liveness::pytest root-use rows + Host::proc_root_lookup); promote it when a second user appears (none yet).

2026-10-24 implementation (master 808a8e8 = 1f43325 + review rounds d9086a3, 808a8e8; fast-forwarded from 3abbaae).

DATA: builtin.yaml liveness_class (required on built-in sources, refused on filters/user profiles) and releases: ["0.1"] (explicit catalog membership; absent/null/[] = dev library only; refused on class none; duplicates refused). Library::proposed_release_01 is derived from releases (scope guard: fs+delete or cargo-target, admissible class, >=1 source); the 0.1 catalog is unchanged (cargo-target + 7 idle filters). profiles show YAML/JSON emit liveness_class and releases (additive JSON). golden.rs sync test: every built-in source in exactly one Liveness contracts row, same class, residuals non-empty, verdict/class pairs (Shipped needs admissible class, verified heuristic needs "admitted by Mark, DATE"; Decision pending only verified heuristic; Proposed withdrawal only none), release only for Shipped. Table now 6 columns (Profile | Producer (verified) | Contract (evidence) | Class | Residuals | Verdict).

CLASSIFICATION (docs/reference.md Liveness contracts is the single home):
- sound, Shipped: nix-shell-rc (Owner PID); brave/chrome/chromium/edge-browser-cache (app_not_running).
- verified heuristic, Shipped: pytest-tmp (admitted by Mark 2026-10-23).
- delegated, Shipped: cargo-target (lock protocol; 0.1); docker-build-cache, docker-dangling, docker-unreferenced (daemon prune); pip-cache, uv-cache, go-build-cache, npm-cache, pnpm-store, yarn-cache, ccache, platformio-cache, jest-cache (own cleaner).
- tolerant, Shipped: pytest-cache (pytest 9.0.3 cacheprovider source: get->default, set warns on OSError), python-pycache (importlib/PEP 3147), node-compile-cache (probe below).
- verified heuristic, DECISION PENDING (Mark): mypy-cache, nix-user-cache, go-mod-cache, bun-cache (a run in flight fails once; nix: gone upstream + collected store path lost; go: module version no longer served lost; bun: symlink backend, backend unprobed); cmake/meson/autotools/west-build-anchored (residuals: configure choices only in build dir; HAND-PLACED FILES IN A BUILD DIR = user work, vector cmake-build-anchored-hand-placed-file-residual; dormant IDE); cursor-agent-versions, codex-releases, claude-code-versions, cursor-extensions (launcher by full path; superseded extension lazy load; exe evidence vector claude-code-versions-running-old-version-exe-in-use). Missing vectors are named in each row's verdict.
- none, PROPOSED WITHDRAWALS (not removed; orchestrator/Mark decide; follow-up commit removes them and moves each counterexample vector to a user probe profile like containers-image-age; diskgc-e2e release_suite EXCLUDED must shrink with it):
  * claude-code-tmp: in use only, no floor. Host: all 18 live Claude Code 2.1.280 sessions hold <session>/tasks open (undocumented, version-dependent; TASK-68 on 2026-10-18 saw idle sessions holding nothing). Resumed sessions lose scratchpad/task output (can be user work). Vector claude-code-tmp-live-session-without-held-file-eligible.
  * chromium-tmp: shared org.chromium.Chromium prefix of every Chromium/Electron app; host 312 dirs (245 component-updater url fetcher, 5 unpacker, 62 unnamed) + 8 files; uses not enumerable. Vector chromium-tmp-live-updater-scratch-eligible.
  * playwright-tmp: host live driver (5 d) + headless chromium: profile dir held, its playwright-artifacts-qHgpwa (same second) held by nobody; glob playwright* is open-ended. Vector playwright-tmp-live-artifacts-eligible.
  * zephyr-tmp: producer is the user's own script (zephyr_nixified/scripts/run-h5e5.sh: mktemp -d /tmp/zephyr-h5e5-run.XXXXXXXX, kept on failure with log paths printed) plus hand-made dirs without .git (zephyr-h5-clock-patch, zephyr-usart-patch.*). Vector zephyr-tmp-kept-failure-diagnostics-eligible.
  * renode-tmp: Renode 1.17.0 TemporaryFilesManager (renode-infrastructure f3d5d7b7d): renode-<pid>, files created then closed, own cleanup removes dead-pid dirs but never *-crash nor under --keep-temporary-files; the glob deletes crash reports (29 on host) and live dirs. Route: new name over renode-<pid> with an Owner PID contract. Vector renode-tmp-live-emulation-eligible.
  * xdg-cache-entry-v2: any app; residuals not nameable. Vector xdg-cache-entry-v2-unknown-app-state-eligible.

PROBES (read-only on host data; files only in the session scratchpad; rename used instead of deletion):
- Node 24.21.0: scratchpad ncc-probe; NODE_COMPILE_CACHE=<s>/c1 node run.js (cold, exit 0), again with NODE_DEBUG_NATIVE=COMPILE_CACHE (hits), run.js vanish (script mv's the cache dir mid-run: write "failed. no such file or directory", exit 0), NODE_COMPILE_CACHE=<s>/afile/sub (exit 0).
- Nix 2.34.8: XDG_CACHE_HOME=<s>/xc2; python3 -m http.server 18765 --bind 127.0.0.1 serving t.tgz; nix eval --impure --raw --expr 'builtins.fetchTarball "http://127.0.0.1:18765/t.tgz"' cold (downloaded), warm (cached), then mv xc2/nix/tarball-cache-v2 aside with fetcher-cache-v4.sqlite kept: downloaded again, same store path, exit 0. file:// is re-unpacked every call, so it does not exercise the cached path.
- Host observations: per-process fds via /proc/<pid>/fd for Claude Code and Playwright; ls of /tmp name families; renode source grep; pytest 9.0.3 and mypy 1.9.0 sources in /nix/store.

POLICY QUESTIONS for Mark (not decided here): (a) admit the 12 Decision pending sources, each with its named residual; in particular whether "hand-placed file in a build dir" gets a dated exception like dependency dirs, and whether "tolerant" may include "one run in flight fails" (if yes, the four caches return to tolerant with a rule edit in safety.md); (b) the 6 proposed withdrawals.

REVIEWS: mped-architect round 1 NO-GO (four tolerant rows were heuristics; release scalar not extensible; plus non-blockers) -> fixed d9086a3; round 2 GO with two nits -> fixed 808a8e8. qa-test-runner GO on 1f43325 (independent gate 1043/0/1; 7 mutations of data/table/loader all caught; CLI read-only profiles show checked; 8 vectors run).
GATES: just check exit 0, 1043 passed 0 failed 1 ignored, on each of the 3 commits (git notes). e2e NOT run (batch-end policy); architect: not a merge blocker, but blocks closing this task/batch.

Decisions 2026-10-24 by mark-emulator (Mark delegated questions; Mark may veto; listed in the orchestrator's report): (1) WITHDRAW the six class-none built-ins: claude-code-tmp, chromium-tmp, playwright-tmp, zephyr-tmp, renode-tmp, xdg-cache-entry-v2 (move their vectors to user probe profiles as done for containers-image-age; shrink release_suite EXCLUDED). (2) ADMIT the twelve verified heuristics, dated 2026-10-24 'admitted by mark-emulator for Mark': mypy-cache, nix-user-cache, go-mod-cache, bun-cache, cmake/meson/autotools/west build-anchored, cursor-agent-versions, codex-releases, claude-code-versions, cursor-extensions; add each row's missing residual vectors. (3) Hand-placed files in a build directory: same DATED EXCEPTION as hand-edited dependency files (2026-10-24), remedy .diskgc-keep/protect. (4) Do NOT widen 'tolerant'; the four caches stay verified heuristic. (5) File re-admission tasks: renode-<pid> with an owner-pid contract; a narrower Chromium component-updater profile.
<!-- SECTION:NOTES:END -->
