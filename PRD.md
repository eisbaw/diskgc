# diskgc: PRD

Status: draft, round 9. The human decides when this is done.

Diagram: `docs/diskgc-design.svg` (interface plus an *example* tier list; the
tier list itself is config).

## Tentative-vs-Committed

`firm`

The shape, interface and protocols below are decided. The Cargo kind still
ships only after its race tests pass (see What good looks like); if they
expose a flaw, the Cargo protocol is revised, not the rest. The backlog must
therefore schedule the Cargo race tests first, before features that build on
them. They need the container harness and `LinuxHost`, not `ModelHost`,
so the container harness is on the critical path; the host model can
follow. (The Nix-daemon GC and Docker `until` checks were run on 2026-10-15;
results are under Weak assumptions & risks.)

## Essence / problem

Machines that build software unattended run out of disk, and when they do,
all work stops. On a development laptop, overnight agents fill the disk with
Cargo `target/` dirs, Nix store paths and Docker images, and in the morning
nothing is done. On persistent CI runners the same caches accumulate between
jobs.

Almost all of that space is *rebuildable cache*. The problem is not "find big
files"; it is: **keep enough space available that no job hits ENOSPC, by
deleting the cache that is cheapest to lose and not in use as far as each
profile and kind can tell, and fail loudly and early when that is not possible.**
v1 is about filesystem *bytes*: inode exhaustion and quotas are out of scope.

diskgc is one binary. `ensure` brings one filesystem up to a free-space goal,
once. `watch` is a loop around the same core with two thresholds. What may be
deleted, and how, is described by **profiles**: named, composable rules
for recognising one kind of cruft (pytest temp dirs under `/tmp`, Cargo
targets, Docker build cache, ...), each with its removal action. A library
of built-in profiles ships with diskgc; users add and override their own.
The YAML config's tiers select profiles with expressions such as
`(pytest-tmp || nix-shell-rc) && idle-3d` and arrange them in escalating
groups. Safety checks (in use, locks, mounts, ownership) are code and
apply whatever the profile.

## Users / actors

- **Laptop owner** running several projects and overnight coding agents on
  NixOS. Runs `watch` as a service under their own user; agents call `ensure`
  before each task.
- **CI runner fleet operator.** Persistent runners, one job at a time. Calls
  `ensure` from the pre-job hook; no service needed.
- **Agent harness** (a script). Calls `ensure --json` and branches on exit code
  and the JSON result: retry later, skip the task, alert.
- **Config author.** Writes the YAML; usually one of the above.

## Interface

```
diskgc ensure --min-free SIZE [--target-free SIZE] [--timeout DUR]
              [--tally-budget DUR] [--no-index]
              [--dry-run] [--json] [--config FILE] [--allow-root] PATH
diskgc watch  --min-free SIZE --target-free SIZE [--interval DUR]
              [--config FILE] [--allow-root] PATH
diskgc watch  --tally-only [--config FILE] PATH
diskgc check  [--config FILE]
diskgc survey [--select EXPR] [--json] [--config FILE] PATH...
diskgc profiles list | show NAME | test   [--config FILE]
```

`ensure` and `watch` also take `--select EXPR`: use one tier selecting
`EXPR` instead of the config's groups (still with the config's user
profiles and `protect` list, if any). `--roots DIR` (repeatable, one
directory each) supplies roots for `roots: required` profiles. Handy in CI:
`diskgc ensure --min-free 50G --select 'cargo-target && idle-1d' --roots
"$CI_BUILDS_DIR" "$CI_BUILDS_DIR"` (the first is the profile's root, the
second the `PATH` whose filesystem must have the space). `--roots` is only
valid together with `--select`; without it, roots come from the config's
tiers.

### `ensure`

- `PATH` is required: the directory the next job will write to. Its
  filesystem is the one diskgc measures and frees. No default to the current
  directory; a hook launched from `/tmp` must not silently free the wrong
  disk. The resolved mount point is printed in every report.
- `--min-free` (required): if available space is at least this, do nothing
  and exit 0, without taking the lock.
- `--target-free` (default: same as `--min-free`): once triggered, keep
  reclaiming until this much is available. Setting it higher is the
  hysteresis: a fleet of CI jobs then does one larger cleanup instead of a
  small one per job. The exit code depends only on `--min-free`; missing the
  target is reported (`target_met: false`) but is not a failure.
- `--timeout` (default `10m`, `0` = none): deadline for the *whole* call,
  including waiting for another reclaim to finish. At the deadline diskgc
  starts no new work and terminates a running child (SIGTERM, SIGKILL after
  10 s) without waiting further for it to exit, so the worst-case runtime
  is `--timeout` + 10 s, plus at most one in-flight `unlinkat` (a single
  syscall; deleting a huge file can take a moment). A CI hook never hangs.
- `--dry-run`: print, in order, every candidate diskgc would consider, whether
  it is eligible or the skip reason, its estimated size and the operation it
  would run (JSON: the `plan` array); delete nothing.
  Does not take the lock. Always prints, even if the goal is already met.
  With a `random` group, the printed order is for the printed seed only.
  Sizes are estimates; Docker and Nix tiers can only say what they would run.
  Exits 0 (2 on usage errors, 124 at the deadline, 130/143 when
  interrupted); JSON `status` is `dry_run`.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors.

### `watch`

- One process per filesystem. On NixOS, one instance of a systemd template
  unit per watched path.
- `--target-free` is required here (unlike `ensure`): a watcher without two
  thresholds would reclaim a sliver at every poll. The error message says so.
- Every `--interval` (default `10s`): one `statvfs`. At or above
  `--min-free`: no deletions; only the background tally keeps the index
  fresh, within its I/O budget (see The tally).
- `--tally-only`: maintain the index and its snapshot, never reclaim. For CI
  runners, where `ensure` in the pre-job hook then finds a warm index.
  `--min-free` / `--target-free` are not used in this mode.
- Below `--min-free`: run the same core as `ensure`, goal `--target-free`.
  No overall deadline; each action is bounded by its tier's `timeout`. The
  wait for the host lock must stay interruptible by SIGTERM.
- Back-off: if a reclaim ends still below `--min-free` because everything
  left is in use (`exhausted`), do not start another until available space
  has dropped by a further `max(1 GiB, 1% of the filesystem)` or 15 minutes
  have passed. The same applies when it ended with misconfiguration errors.
  Only *backend* errors (a Docker or Nix daemon unreachable) shorten the
  wait to 1 minute: backends often recover quickly, misconfigurations do
  not. One log line per
  back-off, not per poll. These values are fixed in code.
- On SIGTERM/SIGINT: stop as described under Signals, exit 0.

### `survey`

Read-only categorisation: for each `PATH`, list which profiles match what
(candidate count, total size, age range, how many are in use), plus the
*unmatched* top-level entries of each `PATH` grouped by name family, so the
next profile worth writing is visible. A name family is the name with runs
of digits replaced by `#` and a trailing random-looking suffix after `.`,
`-` or `_` (five or more letters and digits) replaced by `*`, e.g.
`nix-shell.8raHjU` → `nix-shell.*`, `nix-shell-1144586-3257819220` →
`nix-shell-#-#`. This grouping is a hint for humans, not part of the JSON
contract. Without `--select`, all profiles (built-in and user)
are applied; each `PATH` also serves as the root for `roots: required`
profiles, so `diskgc survey ~/src` finds Cargo targets there. Takes no lock and deletes nothing. This is the manual `/tmp`
survey (ncdu export plus grouping) turned into a command.

### `profiles`

`list`: name, category, kind, description of every profile (built-in and
user, marking overrides). `show NAME`: the full YAML, as diskgc understands
it. `test`: run every profile's `examples` against an in-memory model; exit
1 on any failure.

### `check`

Static validation only (see Validation): load the config, print the
resolved config path and the groups and tiers as diskgc understood them,
exit 0 or 2. It needs nothing from the target machine, so the NixOS module
runs it at build time (also when cross-building) and a bad config fails
`nixos-rebuild`, not the service at 3am. Machine-specific problems (a
missing root, a symlinked path) can only be found at run time.

### Exit codes (`ensure`)

The first matching row wins:

| Code | Meaning |
|---|---|
| 2 | Usage, config or provisioning error. Nothing was deleted. |
| 130 / 143 | Interrupted by SIGINT / SIGTERM. |
| 75 | Deadline reached while waiting for another reclaim, and a fresh sample is still below `--min-free` (EX_TEMPFAIL). Nothing was deleted; retry later. If the other holder freed enough meanwhile, the result is 0. |
| 0 | Final sample of available space ≥ `--min-free`. |
| 124 | Deadline reached during reclaim, below `--min-free`. |
| 3 | A tier failed (command error, permission error, daemon gone) or was excluded as misconfigured, below `--min-free`. Something may have been deleted; the report says what. |
| 1 | Every eligible candidate was processed without errors, still below `--min-free`. The report lists what was skipped and why. |

The mapping is explicit in code and covered by tests. A Rust panic (101) is a
bug.

### Output

- stdout carries only the result. Human mode: silent when nothing needed doing
  (like `cp`); otherwise a short summary. `--json`: always one object.
- stderr carries progress logs: one line per action with tier, candidate,
  observed change in available space, duration. Output of child commands (Docker,
  Nix, command actions) goes to stderr, prefixed with the tier and profile
  name.
- `watch` logs to stderr only; journald collects it.

JSON result (field names are part of the contract):

```json
{
  "status": "met | exhausted | error | usage_error | lock_timeout | deadline | interrupted | dry_run",
  "mount": "/", "path": "/home/mark/src/app", "seed": null,
  "index": {"source": "live | snapshot | sync", "age_s": 0, "units": 0, "untallied": 0},
  "min_free_bytes": 0, "target_free_bytes": 0, "target_met": false,
  "avail_before_bytes": 0, "avail_after_bytes": 0,
  "plan":     [{"group": "", "tier": "", "profile": "", "candidate": "", "eligible": true, "reason": null, "estimated_bytes": 0, "operation": ""}],
  "actions":  [{"tier": "", "profile": "", "candidate": "", "avail_delta_bytes": 0, "duration_ms": 0, "count": 1}],
  "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | not_idle | changed | empty | kept | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_store | missing | unsafe_path | contains_ipc | contains_mount | contains_special | protected | excluded | not_owner | has_not | stale"}],
  "held_by_deleted_files": [{"pid": 0, "command": "", "bytes": 0}],
  "errors":   [{"tier": "", "profile": "", "candidate": "", "message": ""}]
}
```

- `seed` is `null` when no group uses `random`.
- `index` says where the order came from: the live index of a running
  `watch`, a snapshot, or a synchronous tally, how old it is, and how many
  candidates were not yet tallied. `stale` skips are units that were gone
  or replaced when re-checked.
- `plan` is filled only by `--dry-run` (empty otherwise); `reason` uses the
  skip-reason enum; a bucket is one plan entry with a member `count`;
  `estimated_bytes` is `null` for Docker and Nix without a reported size
  and for candidates not yet tallied.
- `avail_delta_bytes` is signed (see Reclamation core).
- `count` is 1 for a single candidate. Selections that match very many
  candidates (e.g. under `/tmp`) aggregate: one `skipped` entry per reason with `count`
  and up to 20 `examples` (`candidate` is then empty), and after 1,000
  `actions` a single summary entry with `count`.
- `held_by_deleted_files` lists open-but-deleted files on the requested
  filesystem (visible processes only); always present, possibly empty.
- On a usage error, fields that could not be determined (`mount`, `path`,
  sizes) are `null`; `errors` says why.

### Signals

On SIGINT/SIGTERM: do not start another candidate; send SIGTERM to the
running child's process group, SIGKILL after 10 s; release locks; print the
report; exit 130/143 (`watch`: 0).

Every child inherits the host lock's file descriptor (close-on-exec cleared
on that one fd only; Cargo lock fds keep it), so `flock` stays held until the
last child, and its children, exit. A SIGKILLed diskgc therefore does not
leave a child process deleting without the lock; the cost is that an
orphaned child stalls other reclaims until it ends (they hit their deadline
and exit 75). diskgc releases the host lock only by `close()`, never
`flock(LOCK_UN)`, which would release it for every inherited copy too.

Limits, stated plainly: the guarantee is best-effort for command actions
(tools such as `sudo` or `ssh` close inherited fds), and it does not cover
work done inside daemons: a Nix GC or Docker prune keeps running in
`nix-daemon` / `dockerd` after its client dies. Overlapping prunes waste work
but do no harm. (`PR_SET_PDEATHSIG` was rejected: it fires on parent
*thread* death and does not reach grandchildren.)

### Sizes and durations

- `K M G T` and `KiB MiB GiB TiB` are powers of 1024; `KB MB GB TB` are powers
  of 1000. A bare number is an error.
- `N%` is N percent of the filesystem size (`f_blocks × f_frsize`).
- Durations: integer plus `s m h d w`. No months; `m` is always minutes.
- `--target-free` below `--min-free`, or either above the filesystem size, is
  a usage error (exit 2).

## Profiles

diskgc's knowledge of *what* can be deleted and *how* lives in **profiles**:
named, composable rules for recognising one kind of cruft ("pytest temp dirs
left under `/tmp`", "Cargo target dirs", "Docker build cache"). The config's
tiers then only say *which* profiles apply and *when* (see Configuration).

Surveying a real `/tmp` showed why: 143,190 entries and 45 GiB, of which
22.8 GiB were pytest temp dirs inside leftover `nix-shell` temp dirs and
98,000 entries were `nix-shell` / `nix develop` rc dirs. Each such pattern is
one profile.

### What a profile is

```yaml
- name: pytest-tmp
  category: test
  description: pytest temp dirs in /tmp, also inside nix-shell and mktemp dirs
  find:                              # makes this a *source* profile
    roots: [/tmp, /var/tmp]
    paths: ["pytest-of-*", "nix-shell.*/pytest-of-*", "tmp.*/pytest-of-*"]
    type: dir
  action: delete
  examples:                          # checked by `diskgc profiles test` and CI
    match:    [/tmp/pytest-of-mark, /tmp/nix-shell.AbC123/pytest-of-mark]
    no_match: [/tmp/pytest-of-mark/pytest-3, /home/mark/pytest-of-mark]
  verified_with: pytest 8.3          # built-in profiles only
```

- **Source profiles** have `find`: roots plus path patterns relative to them
  (each pattern segment is one directory level; `**` is not allowed, so
  discovery never walks deeper than the patterns say), and the candidate
  type. `find` names candidates *positively*; nothing that no source
  profile found is ever a candidate.
- **Roots are fixed or required.** A profile either has fixed roots
  (`/tmp`, `/var/tmp`) or declares `roots: required` (machine-specific
  locations such as `cargo-target`, `pytest-cache`). Only the latter take
  roots from a tier (`roots:`) or from `--roots`; giving roots to a
  fixed-root profile is a load-time error. A selection can therefore never
  be pointed at a place its profile was not written for.
- **Filter profiles** have only `where` (predicates, below). They narrow a
  source's candidates; they never find anything.
- **`action`** says how a source profile's candidates are removed. For the
  code-backed kinds the kind fixes the action; a profile of such a kind
  cannot name another one.
- **`kind`** (default `fs`): `fs` (a file or directory), or one of the
  code-backed kinds `cargo-target`, `docker-build-cache`, `docker-images`,
  `nix-store` (see Built-in kinds).
- **`examples`**: paths that must and must not match. Every built-in profile
  has them; they run as model tests.

### Predicates (`where`)

Deliberately few; `find` already covers names, types and locations, and
ownership is an always-on check, not a predicate.

| Predicate | Meaning | Kinds | Cost |
|---|---|---|---|
| `idle_for` | not used or changed for this long, as the kind measures it (below) | `fs`, `cargo-target`, `docker-build-cache` | tree walk (`fs`); a few `stat`s (`cargo-target`); none (Docker) |
| `has` / `has_not` | contains / does not contain a named file (e.g. `CACHEDIR.TAG`) | `fs` | one `stat` |

A predicate on a kind that does not support it is a load-time error. Two
`idle_for` in one term combine to the longest (the intersection).

`idle_for` per kind (the report always names the measure used):

| Kind | `idle_for` means |
|---|---|
| `fs` | newest mtime or ctime in the candidate's tree (atime is ignored: under `relatime` any reader, such as a nightly scan, refreshes it; observed on this machine) |
| `cargo-target` | last build: newest mtime of the profile dirs and their `.fingerprint/`, `deps/`, `build/`, `incremental/` |
| `docker-build-cache` | last use, as recorded by Docker (`--filter until=`) |

### Composing profiles

A tier selects candidates with an expression over profile names:

```
expr   := term ( '||' term )*
term   := source ( '&&' factor )*
factor := filter | '!' filter | '!' source
```

Examples: `pytest-tmp && idle-10d`, `(pytest-tmp || nix-shell-rc) && idle-3d`
(written with parentheses, it distributes: each source gets the filters),
`tmp-entry && idle-2d && !idle-30d` (idle between 2 and 30 days),
`tmp-entry && idle-2d && !claude-code-tmp`.

- **One source per term.** Each `&&`-term is exactly one source profile plus
  filters. Intersecting two source profiles that find at different depths
  matches only by accident, so it is not allowed.
- **`||` is a union**, and all source profiles in one expression must share
  one action (and one kind). Then `a || b` and `b || a` select and remove
  exactly the same, and no rule about which action "wins" is needed.
  Different actions belong in different tiers.
- **`!` only narrows.** `!filter` is a path-local complement (`!idle-30d`).
  `!source` removes every candidate that **overlaps** a match of that source
  profile: is it, lies inside it, or contains it. So
  `tmp-entry && !claude-code-tmp` excludes `/tmp/claude-1000` as a whole,
  because it contains Claude Code session dirs, even though the two
  profiles find at different depths. The negated profile's `find` is
  enumerated for this (bounded by its patterns, like any `find`); excluded
  candidates are reported as `excluded`. Any
  `where` it has is ignored for the overlap test: `!` protects everything
  the profile finds, not only its currently eligible part. A negated
  profile with `roots: required` takes the tier's roots.
- **`protect:`** (top level, below) is the same overlap exclusion applied to
  every tier at once, for things that must never go in any tier.
- A negated profile must be of kind `fs` or `cargo-target`, and `!profile`
  is only allowed in `fs` / `cargo-target` expressions (e.g.
  `tmp-entry && !cargo-target` is legal; nothing with Docker or Nix is).
  Negated profiles do not count towards "one kind and one action per
  expression".
- **Non-`fs` kinds** allow only `NAME` (`docker-images`, `nix-store`) or
  `NAME` / `NAME && idle-*` (`docker-build-cache`): they have no
  per-candidate enumeration, so unions and negation have nothing to act on.
- **Filters are evaluated from the index** where it already knows the
  answer, and re-evaluated at deletion time (see The reclaim walk). A
  candidate that fails a filter is reported as skipped with that filter's
  reason
  (`not_idle`, `has_not`). The set-algebra properties hold for the set of
  *eligible* candidates.

### Protection and nesting

- **`protect:`** (top level of the config) lists profiles whose matches are
  never deleted by any tier, by **overlap**: a candidate that is a protected
  match, lies inside one, or contains one is skipped (`protected`). Example:
  `protect: [claude-code-tmp]` keeps `/tmp/claude-1000` from being deleted
  as a whole by `tmp-entry`, even though `tmp-entry` finds it at a
  different depth. Built-in default: empty; the example config protects
  `claude-code-tmp`.
- **Nested candidates.** A candidate is skipped (`contains_special`) if its
  tree contains a `.diskgc-keep` file anywhere, or anything that a profile
  with an action other than plain `delete` would match (any profile in the
  library, selected or not). For this test, roots are ignored: a path
  inside the candidate "would match" a special profile if it matches the
  profile's recogniser, i.e. the trailing segments of its `paths` patterns
  plus its `has` markers, or, for `cargo-target`, a directory with
  `CACHEDIR.TAG` and a known Cargo lock file. The tree walk already visits
  every entry, so this costs pattern matches, not extra I/O. A plain `delete` of an
  enclosing directory can therefore never bypass a special action (a
  `cargo-target` lock protocol, a `go clean`), whichever tier runs first.
  Candidates are otherwise processed independently; one deleted inside an
  earlier-deleted parent is simply `missing`.

### Actions

| Action | Use |
|---|---|
| `delete` | diskgc's own deletion: fd-relative, never follows symlinks, never crosses mounts. Option `make_writable: true` adds owner write permission to directories, *during* the delete walk (after the last in-use recheck), for read-only trees such as Go's module cache or files copied from the Nix store. Opt-in per profile, since a read-only directory can be a deliberate protection. |
| `command` | an argv list run per candidate, e.g. `[go, clean, -modcache]` or `[bazel, clean, --expunge]`. Trusted, arbitrary command execution (see Command actions). |
| kind actions | `cargo-target` (lock protocol), Docker prune, Nix GC: fixed by the kind, see Built-in kinds. |

### Built-in profile library

Shipped inside the binary as YAML data (not code), listed with
`diskgc profiles list`, shown with `diskgc profiles show NAME`. A built-in
profile is only added with `examples`, model tests, and a `verified_with`
line naming the tool version it was checked against on a real system.
Initial library (to grow by surveying real machines):

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | `pytest-of-*` in `/tmp`, `/var/tmp`, and one level down (nix-shell / mktemp dirs) | `delete` |
| test | `pytest-cache` | `.pytest_cache` (roots required) | `delete` |
| nix | `nix-shell-rc` | `/tmp/nix-shell-*-*`, `/tmp/nix-develop-*-*` (rc dirs left by every shell start) | `delete` |
| nix | `nix-shell-tmpdir` | `/tmp/nix-shell.*` (a shell's `TMPDIR`; in use while a process has it in its environment) | `delete` |
| nix | `nix-garbage` | dead store paths | Nix GC |
| rust | `cargo-target` | Cargo `target/` dirs (roots required) | lock protocol |
| containers | `docker-build-cache` | BuildKit cache | prune |
| containers | `docker-dangling` | untagged images | prune |
| containers | `docker-unreferenced` | images no container uses (removes tagged images too) | prune |
| ai agents | `claude-code-tmp` | `/tmp/claude-{uid}/*/*` session dirs | `delete` |
| browsers | `chromium-tmp` | `.org.chromium.Chromium.*` and similar automation profiles in `/tmp` | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner |
| generic | `tmp-entry` | any top-level entry of `/tmp`, `/var/tmp` | `delete` |
| generic | `mktemp-dir` | `/tmp/tmp.*` | `delete` |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `jest-cache`, `playwright-tmp`, `node-compile-cache` | jest's cache dir; `playwright*` profiles in `/tmp`; `node-compile-cache` in `/tmp` and nix-shell temp dirs (seen here) | `delete` |
| c / c++ | `sccache` | sccache's local cache dir (roots required) | `delete`, only while no `sccache` server process is running (in-use check on the server's open files); never stops the server |
| c / c++ | `cmake-build` | directories containing `CMakeCache.txt` (roots required) | `delete` |
| jvm | `gradle-caches`, `maven-repo` | `~/.gradle/caches`, `~/.m2/repository` (roots required) | `delete` (Gradle daemons are an in-use process; re-download cost) |
| embedded | `zephyr-tmp` | `/tmp/zephyr-*` scratch and `zephyr-*-run.*` dirs (seen here: 1,800+ entries) | `delete` |
| embedded | `west-build` | Zephyr `build/` dirs (`CMakeCache.txt` plus `zephyr/`; roots required) | `delete` |
| embedded | `renode-tmp` | `renode-*` in `/tmp` and nix-shell temp dirs (seen here) | `delete` |
| embedded | `platformio-cache` | PlatformIO's `.cache` (roots required) | `delete` |
| filters | `idle-1h`, `idle-1d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d`, `has-cachedir-tag` | (no `find`) | none |

Profiles that find at any depth under a project root (`pytest-cache`,
`cmake-build`, `west-build`) cannot use `**`; they either list depths
explicitly or take one root per project. Settling that is part of
verifying them.

Every row above is a v1 *target*: a profile ships only once it has its
`examples`, a model test and a `verified_with` from a real system; until
then it is listed as planned, not shipped. Other AI agents (Codex, Cursor,
Aider, ...) and further build systems come later, the same way; until
then, user profiles cover them.

### User profiles and overrides

- The config's `profiles:` list adds profiles. A user profile with the name
  of a built-in one must say `override: true`; otherwise it is a load-time
  error, so nobody shadows a built-in by accident.
- `diskgc profiles test` runs every profile's `examples` (built-in and user)
  against an in-memory model, so a user profile is checked before it ever
  touches a disk.
- `check` warns about a user profile whose last path segment is only
  wildcards outside `/tmp` and `/var/tmp` (e.g. `roots: [/home/mark]`,
  `paths: ["*"]`).

## Configuration (YAML)

Lookup: if `--config` or `$DISKGC_CONFIG` is given, that file and only that
file is used; missing or unreadable is exit 2, never a fallback, so a typo
cannot activate some other policy. Only when neither is given:
`$XDG_CONFIG_HOME/diskgc/config.yaml` (default `~/.config/...`), then
`/etc/diskgc/config.yaml`. None found: exit 2, listing the paths tried. The
binary ships built-in *profiles* but **no built-in tiers**; an example config
ships as `docs/example.yaml`, and the NixOS module provides a default.

The config contains `version`, optional user `profiles`, optional
`protect`, optional `owners` (only with `--allow-root`), and the tier
groups. Thresholds, intervals and timeouts are command-line arguments, so the
same file serves `watch` on a laptop and `ensure` on a runner.

```yaml
version: 1

profiles:                          # optional: your own, or overrides
  - name: zephyr-run
    category: build
    find: {roots: [/tmp], paths: ["zephyr-*-run.*"], type: dir}
    action: delete
    examples: {match: [/tmp/zephyr-h5e5-run.jTm9zGlH], no_match: [/tmp/zephyr-hal]}

protect: [claude-code-tmp]         # never deleted by any tier (by overlap)

groups:
  - name: stale                    # nobody will miss these
    ranking: random                # ordered (default) | random
    tiers:
      - name: test-and-shell-cruft
        select: (pytest-tmp || nix-shell-rc || zephyr-run) && idle-3d
      - name: cargo-stale
        select: cargo-target && idle-7d
        roots: [/home/mark/src, /home/mark/topics]
      - name: docker-cache-stale
        select: docker-build-cache && idle-7d
      - name: nix-dead
        select: nix-garbage

  - name: pressure                 # costs rebuild or download time
    tiers:
      - name: docker-dangling
        select: docker-dangling
      - name: tmp-old
        select: tmp-entry && idle-2d
      - name: cargo-recent
        select: cargo-target && idle-1h
        roots: [/home/mark/src, /home/mark/topics]
```

Rules:

- **Groups run in list order. Always.** Escalation (stale before pressure) is
  group order; nothing reorders groups.
- **`ranking` decides tier order inside a group:**
  - `ordered` (default): list order.
  - `random`: shuffled on each reclaim. On a CI fleet, runners then lose
    different caches first, so the fleet as a whole stays warmer. The seed is
    in the report and can be fixed with `DISKGC_SEED` for reproducible tests.
- Within a tier, units are taken in the tier's `order` (`oldest` by
  default, or `best-fit`; see The index), across all profiles the tier's
  expression matched; candidates not yet in the index come after the
  queue, oldest first.
- A tier's optional `roots:` supplies the roots of the `roots: required`
  profiles in its expression (e.g. `cargo-target`); such a profile without
  roots is a load-time error, and so is `roots:` on a tier whose profiles
  all have fixed roots.
- `owners:` (user names) is required with `--allow-root` and forbidden
  otherwise; without root, diskgc only ever touches files of the user it
  runs as. Names must resolve at load; `root` and system users (uid below
  1000) are rejected.
- All paths must be absolute. `~` is rejected: it would mean `/root` for one
  caller and `/home/mark` for another. The only placeholder is `{uid}` (the
  effective user's id, e.g. in `/tmp/claude-{uid}`).
- Static validation is complete at load: YAML, schema, unknown keys,
  profile names, expression grammar (one source per term, `!` only
  narrows), one kind and one action per expression, predicates supported by
  the kind, roots fixed vs required, `override:` on shadowed built-ins,
  profile `examples`, and `command` executables (resolved to absolute paths
  at load and shown by `check`). Any error → exit 2, nothing
  deleted. Machine-specific checks run before the first deletion of each
  reclaim (see Validation). `version` is mandatory. Tier names are unique
  across all groups.
- Disabling a tier = commenting it out. There is no `enabled` field.

### Protection marker

A file named `.diskgc-keep` in a filesystem candidate, anywhere inside it,
or in any directory between a profile root and the candidate, excludes it
(reported as `kept`).
Docker images and Nix store paths have no such marker; protect them with
Docker's and Nix's own means (running containers, GC roots). A `command`
action can still delete anything its author tells it to.

### Common tier fields

| Field | Meaning |
|---|---|
| `name` | Unique; used in logs and reports. |
| `select` | Profile expression (see Composing profiles). |
| `roots` | Roots for the `roots: required` profiles in `select`. |
| `order` | `oldest` (default) or `best-fit`, see The index. |
| `timeout` | Per-action limit (default `10m`); a child is killed after it. |

## Built-in kinds

### Filesystem candidates (`fs`): safety checks that always apply

Every filesystem candidate, whichever profile found it, goes through these
checks. No profile or tier can switch them off.

- **Ownership:** only candidates owned by the user diskgc runs as, or,
  under `--allow-root`, by a user in the config's `owners:` list (see
  Configuration); others are skipped (`not_owner`).
- **Mounts first:** `mountinfo` is read before anything else; a candidate
  that is a mount point or contains one (a user's FUSE or sshfs mount in
  `/tmp` is common) is skipped (`contains_mount`) without being `stat`ed, so
  a dead mount cannot hang the scan.
- **Order:** from the index (the tier's `order`). Discovery of
  100,000+ entries costs one `stat` each and happens in the background
  tally, not in the reclaim walk.
- **In use (skip):**
  - a visible process has an open file descriptor, working directory, root
    or memory mapping on a file in the candidate, or an environment variable
    whose value is a path inside it (e.g. `TMPDIR` of a dormant `nix develop`
    shell) (`in_use`). Files are matched by **device and inode**, not by
    path text: the walk records the candidate's inodes, and
    `/proc/<pid>/fd/*` is `stat`ed, `maps` read by its dev:inode columns,
    `cwd`/`root` `stat`ed. Path text would be wrong for processes in other
    mount namespaces (PrivateTmp, flatpak, containers). Environment values
    are paths and are compared as paths;
  - it is a directory locked with `flock` by someone else (the convention
    systemd-tmpfiles also honours) (`locked`);
  - it is, or contains, a socket or FIFO (`contains_ipc`): those belong to
    running programs (agents, X11, tmux, ssh).
- **Deletion, close to the check** (for the `delete` action):
  1. Take an exclusive `flock` on the candidate (directories), as
     systemd-tmpfiles does, so cooperating tools see it is being removed.
  2. Under that lock, re-walk the candidate to collect its current inodes
     (files created since the first walk included), then re-scan `/proc`
     for them immediately before deleting; any hit: release, skip.
  3. Delete with the usual path rules (fd-relative, no symlinks, no mount
     crossing).
  4. Release.

  Remaining gap, stated plainly: a process can still open a file in the
  candidate, or create one, between step 2 and the deletion. A file created
  mid-delete makes the delete partial (`ENOTEMPTY`); that is reported as an
  error for the candidate, never retried silently.
- **Visibility:** without root, other users' `/proc/<pid>/fd` cannot be
  read, so their use of *your* files is not seen. Fine for one-user
  machines and runners; weaker on shared hosts.
- Entries of systemd's `PrivateTmp` (`systemd-private-*`) are never
  candidates.
- **Hard links:** removing a name that is hard-linked elsewhere frees
  nothing; the signed `avail_delta` shows that.
- **Report size:** a selection can match 100,000+ candidates, so reports
  aggregate: `skipped` holds one entry per profile and reason with a `count`
  and up to 20 example candidates; `actions` holds up to 1,000 entries, then
  a single summary entry with a `count`.
- **Space held by deleted files.** A file deleted while still open keeps its
  space until the process closes it; deleting more cannot help. Every report
  lists such files on the requested filesystem, found via `/proc/*/fd`
  (`held_by_deleted_files`: pid, command, bytes), so a stuck process is
  visible. diskgc never kills processes.
- If `/tmp` is a tmpfs, profiles under `/tmp` only help a request for that
  tmpfs (filesystem matching), which is correct: it frees memory, not disk.

### `cargo-target`: Cargo `target/` dirs

The filesystem checks above apply too (ownership, mounts, in-use process
checks, protection, nesting), with two exceptions where Cargo's own rules
replace them: candidates are ordered by last build, not by own mtime, and
the reclaim protocol below replaces the generic "flock the candidate,
recheck, delete" steps.


- **Lock files.** Cargo's lock set has grown: besides `.cargo-lock`, Cargo
  1.96 also creates and takes `.cargo-build-lock` (taken *before*
  `.cargo-lock`), and `.cargo-artifact-lock` appears in some profile
  directories on this machine (which Cargo mode creates it is unknown; see
  risks).
  diskgc has an explicit **allowlist of known Cargo lock file names**. A
  profile directory containing any other file matching `.cargo-*lock*` is an
  unknown layout: the target is skipped (`unsupported_layout`) until diskgc
  learns it. Lock files are never deleted or moved.
- **Discovery:** walk `roots` (depth limit 6, fixed in code) for directories
  that contain a valid `CACHEDIR.TAG` and at least one *profile directory*:
  a directory holding a known Cargo lock file, at `<target>/<profile>/` or
  `<target>/<triple>/<profile>/`. Do not descend into a found target. Never
  follow symlinks, never cross mount points (see Path traversal). Refuse
  targets on NFS (`unsupported_fs`): Cargo does not lock there.
- **Last build:** newest mtime of the profile directories and their
  `.fingerprint/`, `deps/`, `build/`, `incremental/` subdirectories. A few
  `stat` calls, no tree walk. This is the target's `newest_change` in the
  index.
- **Eligible** only if (when the tier's expression uses an `idle-*` filter)
  the last build is older than its `idle_for`, and some profile directory
  holds more than its lock files (otherwise `empty`: an
  emptied target is not re-listed on every run).
- **In use (skip)**, checked by locks and by process heuristics:
  - any known lock file cannot be `flock`ed without waiting (`locked`);
  - a process the caller can see has its executable or working directory
    inside the target (`in_use`; e.g. a test binary still running);
  - a process the caller can see whose executable name, after normalising
    Nix wrapper names (`.cargo-wrapped` → `cargo`), starts with `cargo`
    has its working directory inside the target's project directory (the
    target's parent), or has `CARGO_TARGET_DIR` pointing at the target in
    `/proc/<pid>/environ` (`in_use`). Cargo holds *no* lock while the
    program runs under `cargo run` / `cargo test` (measured, see risks), so
    for the whole run of such a binary these process checks are the only
    protection: the executable check covers the binary, and this check
    covers the `cargo` parent, including the moment between compilation
    and launch.

  These are heuristics for the caller's own processes, not proof. The PRD
  does not claim Cargo targets are "provably" unused, only that diskgc never
  deletes while Cargo holds a lock, and skips targets with visible activity.
- **Reclaim protocol: delete inside locked profile directories only.**
  1. Take every known lock file of the target, exclusively, without
     waiting. Any failure: release all, skip (`locked`). (A Cargo that holds
     `.cargo-build-lock` while waiting for `.cargo-lock` makes this fail,
     which is the intended outcome.)
  2. List the lock files again and re-read the last build time. A new lock
     file (a profile created since discovery) or a newer build: release,
     skip (`changed`). Repeat the process checks now that the locks are
     held; a hit: release, skip (`in_use`).
  3. In each locked profile directory: delete `.fingerprint/` first (so a
     half-deleted profile always forces a rebuild), then every other entry
     except the lock files.
  4. Release the locks.

  Nothing outside the locked profile directories is touched: not
  `CACHEDIR.TAG`, not the triple directories themselves, and not top-level
  entries such as `target/doc`, `target/tmp` (written by running tests
  without a lock) or `target/criterion` (benchmark baselines, which are data,
  not cache). Because lock files never move, a Cargo that was blocked on a
  lock and a Cargo that starts afterwards both lock the *same* inodes and
  stay mutually exclusive. A Cargo started during step 3 prints its usual
  `Blocking waiting for file lock` and continues after step 4.

### `docker-build-cache`

`docker builder prune --all --force --filter until=<idle_for>` (the
`idle_for` of the tier's `idle-*` filter, if any). Without
`--all`, Docker removes only *dangling* build cache, not all unused cache
older than the filter. Docker's build cache `until` is based on last use
(verified on Docker 29.9: entries used 5 s earlier survived `until=30s`;
after 45 s idle they were removed). One action prunes everything matching;
there is no per-candidate stop.

### `docker-images`

Profile `docker-dangling` runs `docker image prune --force`;
`docker-unreferenced` runs `docker image prune --all --force`, which also
removes tagged images that no container references.
No age option: Docker's image `until` filter is never "last used". With
the containerd image store (the default on Docker 29) it is the local pull
time (verified: an image pulled 20 s earlier, built months ago, was removed
by `until=10s`); with the classic store it is the image's build date (per
Docker's documentation, not tested). Either way an image pulled long ago but
run hourly would look "old".

### `nix-store`

`nix-store --gc --max-freed <remaining need>`. Nix decides liveness (GC
roots, temporary roots, runtime roots of running processes). `--max-freed`
means "stop after at least this much"; the real effect on available space is
measured, not assumed. Never removes generations or profiles.

Store selection: `NIX_REMOTE` or the `store` setting can point `nix-store`
at a different store than `/nix/store`. Before running GC, diskgc asks Nix,
with the exact environment it will pass to `nix-store`, which store is in
effect (`nix --extra-experimental-features nix-command store info
--json`, whose `url` field names the store; it does not report a
directory). Supported: `url` is `daemon`, or `local` without `root=`,
`store=` or `real=` parameters, and `builtins.storeDir` is `/nix/store`;
the filesystem of `/nix/store` is the one matched. Anything else: the tier is excluded and reported as an error
(`unsupported_store`), so a useless Nix tier is noticed.

### Command actions

```yaml
- name: go-build-cache
  category: caches
  find: {roots: [/home/mark/.cache], paths: [go-build], type: dir}
  action:
    command: [env, "GOCACHE={path}", go, clean, -cache]
    timeout: 5m
```

- **This is trusted, arbitrary command execution.** diskgc provides the
  filesystem safety checks *before* the command runs (ownership, mounts,
  in use, locks), but cannot limit what the command deletes; the profile
  author owns that. Prefer the tool's own cleaner (`pip cache purge`,
  `go clean -cache`), which knows its own locking, over `rm`.
- **The candidate path does not tell the tool what to clean.** Most
  cleaners pick their cache from their own config or environment, not from
  the working directory, so the command must name the location explicitly
  (`pip --cache-dir {path} cache purge`, `env GOCACHE={path} go clean
  -cache`). Otherwise diskgc measures one directory and the command purges
  another. The built-in profiles do this.
- `{path}` is replaced by the candidate's path as a single argument (or
  inside one argument, as in `GOCACHE={path}`), never through a shell. Use
  `[sh, -c, "..."]` explicitly if a shell is wanted.
- **Gap, stated plainly:** the tool receives a path *string* and resolves it
  again itself, following symlinks; diskgc's `openat2` guarantees end at the
  `exec`. Where the tool allows it, prefer `.` with the candidate as working
  directory (entered via `fchdir`, so it is the checked directory).
- `argv[0]` is resolved to an absolute path at load (from the `PATH` diskgc
  runs with) and shown by `check` and in reports, so it is clear which
  binary a built-in `command` profile will run.
- Working directory: the candidate (opened as described under Path
  traversal and entered with `fchdir`) if it is a directory, else its
  parent; `cwd:` can name `candidate` or `parent` explicitly.
- Environment: `DISKGC_PATH`, `DISKGC_NEED_BYTES`, `DISKGC_MOUNT`,
  `DISKGC_PROFILE`.
- Runs in its own process group; killed after `timeout`. Like every child
  (see Signals), it inherits the host lock.
- Non-zero exit is an error in the report (and exit 3 if the goal is not
  met), never just a log line.

## Gathering and reclaiming

diskgc separates **knowing what could be freed** from **freeing it**. When a
disk is nearly full is the worst moment to walk 45 GiB of `/tmp` to find out
what is big: that walk alone took ncdu over 7 minutes on this machine. So:

1. A **tally** builds and maintains an **index** of what the profiles match,
   with sizes, ages and a last-checked time, slowly and in the background.
2. A **reclaim walk** takes units from a **priority queue** over that index
   and acts on them, revalidating each one cheaply first.

**The index is a cache, never an authority.** Every unit is revalidated
before anything is done to it, and all safety checks (ownership, mounts,
protection, nesting, in use, locks, Cargo's protocol) run at deletion time,
exactly as before. A stale, partial, corrupt or missing index can make
diskgc slower or pick a worse candidate first; it can never make it delete
something the checks would refuse.

### The index

The index is a set of trees, one per filesystem, of **units**: things the
profiles found, with what the tally learned about them. A unit has medium
granularity: coarse enough that the index stays small, fine enough that it
is cheap to check whether it still holds.

```
Unit {
  identity: single (mount, inode, birth time) | bucket (mount, parent, name family)
  path
  profiles                      # every source profile that found it (a set)
  members                       # bucket only: count (names are re-read, not stored)
  est_bytes, files              # allocated blocks, each inode counted once
  newest_change                 # newest mtime/ctime seen (for idle-* filters)
  flags                         # keep file, special content, sockets, mounts (as last seen)
  checked_at, tallied_at
  cursor                        # resumable walk position while a big tree is being tallied
}
```

- **Granularity:** one unit per candidate (one `pytest-of-*` dir, one Cargo
  target, one top-level `/tmp` entry). Small candidates are merged per
  (profile set, parent directory, name family) into one **bucket**, so
  98,000 tiny `nix-shell-*-*` rc dirs become a handful of units. The
  bucket threshold is derived, not stored: the smallest power of two from
  64 MiB up that keeps the index within its bound (default 100,000 units,
  roughly 20 MB of memory).
- **Identity** is (mount, inode, birth time from `statx`), falling back to
  (mount, inode, parent identity) where the filesystem reports no birth
  time. ctime and size are *not* identity: they change whenever something
  inside changes, and only mean "refresh the estimate". A path that several
  profiles match is one unit with a set of profiles, so it is counted once.
- **Trees and queue:** units are stored in a per-mount path trie, which also
  answers the overlap questions (`protect`, `!profile`, nesting) without
  touching the disk. The **priority queue** is a derived view: for each
  tier, the units its expression selects on that filesystem, ordered by
  group, then tier, then the tier's `order` (below). Changing tiers rebuilds
  the queue, not the index.
- **Order within a tier** (`order:` on a tier):
  - `oldest` (default): least recently changed first, as before. Keeps the
    LRU gradient even in tiers without an `idle-*` filter, and makes the
    smallest possible overshoot likely.
  - `best-fit`: the smallest unit whose estimate covers the remaining need,
    otherwise the largest. Frees "something sizable" in one step when the
    disk is nearly full, with little overshoot. Buckets count as many
    deletions and rank after single units of the same size.
- **Freshness** has two levels. A **re-check** is cheap (`statx` the unit
  and its parent: still there, same identity); a **re-tally** walks the
  tree to refresh size and `newest_change`. Directory mtimes do not reveal
  growth deep inside a tree, so re-tallies are scheduled by age, not
  triggered by mtimes alone.

### The tally

- **Background (in `watch`):** the tally runs forever in **mini-batches**
  with a **duty cycle**: after a batch that took *t*, it sleeps 19 × *t*, so
  it uses about 5 % of one disk's time whatever the device (on a cold HDD a
  `statx` can take 10 ms, on NVMe microseconds). It also runs at idle I/O
  priority and `nice 19`; note that only the BFQ scheduler honours idle I/O
  priority, so on NVMe (scheduler `none`) the duty cycle is the real limit.
  Big trees are tallied across many batches via their `cursor`; a cursor
  survives entries vanishing under it (`ENOENT` ends that branch).
- **Order of work:** discovery (list profile roots for new and vanished
  candidates), re-checks, re-tallies, with aging so that none starves the
  others on a busy `/tmp`.
- **Pausing:** before each batch the tally tests whether anyone holds the
  host lock (`/run/diskgc/lock`, a non-blocking test); if so, a reclaim is
  running (in this or another process) and the tally waits. The tally never
  speeds up under pressure; when space is short, the reclaim walk uses what
  is already known.
- **Synchronous mode:** without a usable index (e.g. `ensure` on a runner
  that runs no `watch`, or `--select` / `--roots` naming roots no index
  covers), the same engine runs first at full speed for a bounded time
  (`--tally-budget`, default the smaller of 30 s and a quarter of
  `--timeout`; `0` means no limit), **before** taking the host lock (it is read-only), then the
  reclaim walk uses the partial index.
- **Snapshot:** `watch` writes its index atomically (temporary file +
  rename) every 5 minutes and on exit, to
  `$XDG_STATE_HOME/diskgc/index-<fs>` (mode 0600, it lists paths), with a
  format version and a hash of what shapes the index (profiles and roots,
  not tiers). `<fs>` is the filesystem's UUID where it has one, otherwise
  its device number (`st_dev`); not `f_fsid`, which is 0 on tmpfs on older
  kernels, and not the mount path. A single writer is enforced with a
  separate lock file, `index-<fs>.lock`, that is never renamed (the
  snapshot itself is replaced by rename, so a lock on it would not hold):
  a second `watch` for the same filesystem and user fails at start (exit 2)
  instead of racing. `ensure`
  and `survey` use the snapshot if version and hash match and it is younger
  than an hour; otherwise they tally synchronously. A snapshot another user
  wrote is unreadable (0600), so that user's `ensure` tallies synchronously,
  and says so in its report. A mismatched or corrupt snapshot is ignored
  with a log line, never an error. `--no-index` forces synchronous mode.
- **Docker and Nix units** carry the reclaimable size the tools report
  (`docker buildx du`, `docker system df`), or no size if unknown.

### The reclaim walk

```
reclaim(min, goal, filesystem, deadline):
  if avail(filesystem) >= min: return met            # no lock taken
  index := live index (watch) | fresh snapshot | synchronous tally (budget)
  lock /run/diskgc/lock, waiting until deadline
    on timeout: return met if avail >= min else lock_timeout
  if avail(filesystem) >= min: return met            # someone else freed it
  preflight all tiers (environmental validation)      # before any deletion
  queue := units on this filesystem selected by a tier,
           by group, tier rank, tier order (oldest | best-fit)
  while avail < goal and queue not empty:
    unit := queue.pop()
    re-check unit (statx: same identity; bucket: one readdir of the parent,
      members still match the family) and re-evaluate its selection now
      (filters, `!profile`, `protect`, nesting):
      gone or moved: record skip (stale), continue
      changed estimate (cheap re-check only, no walk): note it, keep going;
        the background tally refreshes it later
    for candidate in unit (a bucket's members oldest first; one /proc scan
                           per bucket, not per member):
      if not_owner / protected / excluded / kept / contains_special /
         contains_mount / not idle / has_not / in use / changed / empty:
        record skip; continue                         # checks at deletion time
      action(candidate, need = goal - avail)
      record avail_delta
      if avail >= goal: break
  if avail < goal:                                    # always, any index source
    one discovery pass over the selected profiles for candidates not in the
      queue (created since the index was built), oldest first, same checks
  return met if avail >= min else (error if any errors else exhausted)
```

Every loop step, child wait and native deletion step checks the deadline.
`watch` calls it with `min = --min-free`, `goal = --target-free`; `ensure`
with its two flags. With a warm index, the first deletion starts within
seconds of crossing `--min-free`: re-checking a unit costs a few `statx`
calls, not a walk. `exhausted` is only reported after the final discovery
pass, so an index that missed something can delay success but never fake
exhaustion. `stale` means the unit was gone or replaced at re-check;
`changed` is Cargo's "a lock file or build appeared after the locks were
taken".

- **Available space** = `f_bavail × f_frsize` from `statvfs`: what an
  unprivileged writer can use, never the root reserve. No `syncfs` (it waits
  for unrelated I/O and cannot be cancelled). Some filesystems free blocks
  asynchronously, so if a sample after an action shows less than expected,
  diskgc re-samples a few times over at most 2 s before moving on.
- **`avail_delta`** is the observed net change in available space around an
  action. It is signed: concurrent writers can make it negative, unrelated
  deletions can inflate it. It is reported as an observation, never as
  "bytes freed by this action". Success is judged only by the final sample.
- **Filesystem matching** happens before any tier runs: filesystem
  candidates (including Cargo targets) by device and mount (see Path
  traversal); Docker by its data
  root (`DockerRootDir`, or the containerd root when the containerd image
  store is in use) and a local, not remote, Docker context; Nix by the store
  it will actually collect (see `nix-store`). A tier whose backend is
  elsewhere is skipped and reported (`other_fs`, `remote_backend`):
  deleting there would hurt a cache and free nothing here. A Nix store diskgc
  cannot handle is different: `unsupported_store` is a misconfiguration and
  reported as an error (see Validation).

- **Overshoot:** for filesystem candidates (including Cargo), at most one
  candidate past the goal. Docker and Nix actions are whole operations with no such bound.
- **Tallies** count allocated blocks and each inode once (Cargo hard-links
  binaries from `deps/`), and are still estimates. They decide the order of
  work, never whether a deletion is safe; reports use the statvfs delta.

### Path traversal

- All directory work is fd-relative. To reach a configured root or `path`:
  1. In `/proc/self/mountinfo`, pick the deepest mount point that is a
     path prefix of the configured path (after unescaping `\040` and
     friends; where mounts are stacked, the topmost one).
  2. Open that mount point with `O_PATH | O_DIRECTORY | O_NOFOLLOW` and
     confirm with `statx` that it is that mount: `STATX_ATTR_MOUNT_ROOT` set
     and `stx_mnt_id` equal to the mountinfo ID.
  3. Resolve the rest of the path relative to it with
     `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`; if
     the path *is* the mount point, the rest is `.`.

  Example: `/home/mark/.cache/pip` with `/home` as its own mount opens
  `/home`, then resolves `mark/.cache/pip`. A `/home -> /var/home` symlink
  on the root filesystem fails in step 3 (`ELOOP`) and becomes
  `unsafe_path`. This refuses symlinks and *any* mount crossing below the
  root, including a bind mount of the same filesystem, which a device
  comparison would miss. (`statx` on the full path is not used to find the
  mount: it follows symlinks in the middle of the path.)
- Deletion walks with `openat2(... RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS)`
  and `unlinkat`, never following a symlink and never entering another mount.
- `statx` results are trusted only when the attribute's bit is set in
  `stx_attributes_mask` (e.g. `STATX_ATTR_MOUNT_ROOT`). If the kernel cannot
  answer a required check, the candidate is skipped (`unsupported_fs`),
  never assumed safe.
- Minimum kernel: Linux 5.8 (`openat2`, `STATX_ATTR_MOUNT_ROOT`, mount ids).
  Older kernels: exit 2 at startup.

### Validation

- **Static** (`check`, and every startup): YAML syntax, schema, unknown keys,
  `version`, unique names, absolute paths, no `~`, size and duration syntax.
  Needs nothing from the machine, so it runs in a Nix build sandbox and when
  cross-building. Failure: exit 2, nothing deleted.
- **Environmental** (at the start of every reclaim, for *all* tiers, before
  the first deletion): roots and paths exist, contain no symlinks, resolve
  to a device and mount; Docker and Nix backends reachable and local.
  - A missing path, another filesystem or a remote Docker context is
    normal: that tier is skipped with its reason (`missing`, `other_fs`,
    `remote_backend`). A path created later is picked
    up on a later reclaim.
  - A misconfiguration (`unsafe_path`, `unsupported_store`) excludes that
    tier and is reported as an error (exit 3 if the goal is then not met),
    so a broken tier is visible rather than silently empty. The other tiers
    still run: refusing all cleanup because one tier is broken would let
    the disk fill.
  - A daemon that is down (Docker, Nix) is a *backend* error: the tier is
    skipped and reported, and `watch` retries soon (see Back-off).

### Locking

- One reclaim per host at a time, via `flock` on the fixed path
  `/run/diskgc/lock`, opened read-only. Not configurable: two configs naming
  two lock files would silently break mutual exclusion.
- The lock file is provisioned, not created by diskgc. tmpfiles.d drop-in
  (the NixOS module ships it; CI installs the same file, since `/run` is
  emptied on reboot):

  ```
  d /run/diskgc      0755 root root -
  f /run/diskgc/lock 0644 root root -
  ```

  Missing → exit 2, printing those two lines.
- Any local user who can read the file can hold the lock and stall reclaims;
  `ensure` then exits 75 at its deadline. Inherent to `flock`; documented.
- A caller that waited re-reads available space first; often the previous
  holder already freed enough.

### Privileges

- diskgc refuses to run as root unless `--allow-root` is given. The laptop
  service runs as the owner (`User=`), CI as the runner user.
- Docker requires the `docker` group (which is root-equivalent; that is
  Docker's model, not ours). Nix GC goes through the Nix daemon as a normal
  user: verified on Nix 2.34.8 for deleting a specific dead path
  (`nix-store --delete`); `--gc --max-freed` uses the same daemon operation
  but was not run, to avoid deleting the machine's real garbage.
- With `--allow-root`, the config file and every directory above it must be
  owned by root and not writable by others (like sshd's StrictModes), since
  `command` actions in profiles execute commands from it.

## Hysteresis, everywhere it applies

| Where | Mechanism |
|---|---|
| `watch` | `--min-free` triggers, `--target-free` (required) is the goal. |
| `ensure` | Same pair; `--target-free` defaults to `--min-free` (no surprise deletions), CI hooks set it higher. |
| Candidate eligibility | `idle-*` filters (`idle_for`, measured per kind): recently used caches are not eligible. |
| Nothing left to free | `watch` back-off until space drops further or 15 minutes pass. |
| Within a reclaim | Stop as soon as the goal is met; re-check after every candidate. |

Not included: per-tier cooldowns. The index snapshot could now carry
them, but it is a cache that may be missing or discarded, and a cooldown
that silently disappears is worse than none; the cost (a no-op Nix GC per
CI job when nothing is dead) is accepted until measured otherwise.

## Prior art and why it is not enough

| Tool | What it does | Gap |
|---|---|---|
| Nix `min-free` / `max-free` | GC with hysteresis during Nix builds | Only triggers on Nix builds; ignores Cargo and Docker writers. Complementary. |
| `nix.gc.automatic` | Timed Nix GC | Time-based, not pressure-based. |
| Docker `builder.gc` policy | Bounds BuildKit cache size | Docker only; size-based, not free-space-based. |
| `cargo-sweep`, `cargo-clean-all` | Age-based Cargo target cleanup | No lock awareness, no free-space goal, no other tiers. |
| `systemd-tmpfiles` age cleanup | Age-based deletion (here: `/tmp` after 10 days, `/var/tmp` after 30); skips paths protected by a BSD `flock` | No free-space trigger, no open-file or socket checks, no policy across ecosystems, no Cargo-specific lock handling. diskgc's filesystem checks honour the same `flock` convention. |

diskgc's contribution is the coordinator: one free-space goal, an ordered
policy across ecosystems, composable profiles, and in-use checks that
apply whatever the profile.

## Deployment

- One Rust binary (see Implementation), Linux 5.8 or newer.
- NixOS module: renders the YAML, ships the tmpfiles rule for the lock, runs
  `check` at build time, and starts one `diskgc watch` instance per
  configured path as the configured user.
- CI runners: binary, YAML, the tmpfiles drop-in, and
  `diskgc ensure --min-free 50G --target-free 80G "$CI_BUILDS_DIR"` in the
  pre-job hook.

## Implementation

- **Rust everywhere**: the product, the test harness, the host-model
  generator, test helpers (e.g. the "pause before run" Cargo runner used by
  race test 8). No shell scripts in product or tests; `just` recipes only
  call Nix, Cargo and Podman.
- **Nix flake** is the single entry point:
  - Build with **crane** (`buildDepsOnly` shares one dependency build across
    package, clippy and tests), with source filtering so that editing docs
    does not rebuild or rerun anything.
  - Product toolchain from **rust-overlay**, pinned by `rust-toolchain.toml`
    (not rustup, whose self-contained linker and nightlies are broken on
    this machine).
  - *Cargo under test* is separate from the build toolchain: dated,
    pinned rust-overlay toolchains (oldest supported stable and one dated
    nightly, never "latest"), installed in the test image under
    `/opt/cargo-under-test/{stable,nightly}` together with a C toolchain.
    A smoke build per toolchain runs first, so a broken toolchain fails as
    such and not as a diskgc race failure.
  - Outputs: `packages.default` (the binary), `nixosModules.default`,
    `devShells.default`, `checks` (fast checks and evaluation of the NixOS
    module), and the container images for the e2e tests.
- **Cargo workspace:**

| Crate | Role |
|---|---|
| `diskgc-core` | Config schema; the **index** types and the queue derivation (the planner's "snapshot of facts"); the **planner** (pure: index in, plan, skip reasons, exit-code precedence and report out); the **tally** engine and the **executor** (runs lock → re-check → delete → re-sample), both over the `Host` port, which also does snapshot I/O; the `Host` trait. No syscalls of its own. |
| `diskgc-linux` | `LinuxHost`, the real `Host`. A library, because the contract tests use it. |
| `diskgc-model` | `ModelHost`, a `Host` backed by a host model (below), its YAML schema, and the model-test harness. Test-only. |
| `diskgc-modelgen` | Generates host-model *values* (parameterised fixtures and `proptest` strategies); YAML is only how they are saved. Test-only. |
| `diskgc-e2e` | Destructive tests that need a real kernel: the differential contract suite, `materialize` (below), the Cargo race tests. **Excluded from the workspace** (its own Cargo project depending on the workspace crates by path), so no `cargo` command in the workspace compiles it; built only into the e2e test image. |
| `diskgc` | The binary: CLI and output; installs signal delivery (e.g. `signalfd`) into `LinuxHost`, which reports signals to the executor as port events; wires `diskgc-core` to `LinuxHost`. |

### The `Host` port

- **Two shapes of operation.** Files and locks are syscall-shaped
  (`openat2`, `statx`, `flock`, `unlinkat`, `statvfs`, `mountinfo`, `/proc`
  reads), so the safety logic (flags, ordering, rechecks) lives in the core
  where the model can test it. Docker and Nix are domain-shaped:
  `run(argv, env, deadline)`.
- **The tally's pacing is part of the port too:** batch timing, the duty
  cycle's sleeps and the host-lock test go through the port and the virtual
  clock, so the background tally and its pausing are model-testable, with
  operation counts as the measure of I/O.
- **Time is part of the port.** The clock and every blocking operation
  (waiting for the host lock, waiting for a child, the SIGTERM → 10 s →
  SIGKILL sequence, the re-sampling window) are port operations with an
  explicit deadline. `ModelHost` keeps a virtual clock that advances on
  those calls, and delivers signals as events, so exit codes 75, 124 and
  130/143 are model-testable without real sleeping.
- **`ModelHost` checks how it is called**, not only what is asked: an
  `openat2` without `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
  RESOLVE_NO_XDEV`, or a lock released with `LOCK_UN` instead of `close`,
  fails the test. Otherwise "no symlink followed" would test the model, not
  diskgc.

## Testing strategy

**No test ever runs diskgc against the developer's live system.** Tests run
against a modelled host (in-process) or inside a rootless test container.

### 1. Planner tests: plain data

The planner is a pure function, so most of the PRD's rules (eligibility,
ordering, skip reasons, the exit-code precedence table, the dry-run plan,
the JSON report) are tested as data in, data out, with no host at all.

### 2. Model tests: a YAML host model instead of a real disk

All of diskgc's access to the machine goes through `Host`, so tests
substitute `ModelHost`, which answers from a *host model*. Nothing is
intercepted at the kernel level: no FUSE, no `LD_PRELOAD`.

The model is designed as data, not as a second Linux:

- **Inode graph**, not a path map: files and directories are inodes with
  names in directories, so hard links, renames and "a symlink swapped in
  between validation and `fchdir`" are expressible.
- **Derived, not duplicated**: available space is computed (filesystem size
  − allocated blocks, each inode counted once − `other_used`), so deleting
  something in the model frees space in the model.
- **Grown on demand**: categories are added when a core test needs them. The
  full list in view: mounts (ids, parents, types including nfs/btrfs, bind
  and stacked mounts), trees, locks (holders and waiters), processes (exe,
  cwd, root, environ, uid, open file descriptors including deleted files,
  memory mappings), kernel capabilities (`statx` masks, `openat2`),
  scripted `docker` / `nix-store`, clock, delayed freeing.
- **Strict loading**: unknown keys are errors (`deny_unknown_fields`
  throughout), references must resolve (a lock held by a pid that does not
  exist is an error), and a JSON Schema is generated for editor checking.
- **Events on the call trace.** A race is an event between two `Host`
  calls, addressed by the call, not by a named point in diskgc's code:
  `at: {op: flock, path: <…>, nth: 1, when: after}`. No hooks in the
  product, nothing that breaks when code is refactored, and property tests
  can place an event after every call *k* in turn, covering every
  interleaving of a small scenario. The recorded trace of calls is itself
  something tests assert on.

Sketch of a model test:

```yaml
host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /,     fs: ext4, dev: "254:1", size: 1000G, other_used: 980G}
  - {id: 2, at: /home, fs: ext4, dev: "254:2", size: 900G,  other_used: 880G, parent: 1}
tree:
  /home/mark/src/app/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug:
      .cargo-lock: {}
      deps: {size: 8G, mtime: 2026-10-01T00:00:00Z}
locks:
  - {path: /home/mark/src/app/target/debug/.cargo-lock, holder: 4242, mode: exclusive}
processes:
  - {pid: 4242, exe: /nix/store/x-cargo/bin/cargo, cwd: /home/mark/src/app}
events:
  - at: {op: flock, path: /home/mark/src/app/target/debug/.cargo-lock, nth: 1, when: after}
    create: /home/mark/src/app/target/release/.cargo-lock
expect:
  exit: 1
  skipped: [{candidate: /home/mark/src/app/target, reason: locked}]
```

Fixtures live in `tests/models/<area>/<case>.yaml`, one named test per file
(`datatest-stable`); JSON reports are snapshot-tested (`insta`, with
durations, timestamps and seed redacted), since the JSON fields are part of
the contract.

### 3. Property tests

`diskgc-modelgen` generates random host models (`proptest`) and checks the
invariants of What bad looks like, stated exactly:

- no Cargo lock file ever deleted or moved; nothing outside a candidate's
  scope deleted; no symlink followed; no mount crossed (all checked on the
  call trace);
- **the index is never an authority:** with a randomly stale, partial or
  corrupted index (sizes, ages, flags, missing or extra units), every
  deletion still passes the deletion-time checks, and nothing is deleted
  that a run with a correct index would refuse; only the order and the
  number of `stale` skips may differ;
- the exit code follows the full precedence table for the final state;
- on an unchanged system with the same seed, the real run's actions are a
  prefix of the eligible entries of the dry-run plan (filesystem
  candidates, including Cargo);
- selection semantics: for random profile sets and expressions, the
  *eligible* candidates (after lazy filters, `!` and `protect` overlap
  exclusion, and `contains_special`) equal the result computed by a simple
  reference implementation of those rules; `!` and `protect` never add a
  candidate; `a || b` and `b || a` give the same result; every eligible
  candidate was found by some source profile.

A failing case shrinks to a minimal model, printed as YAML; developers save
it under `crates/diskgc-model/tests/regressions/`, where it is replayed from
then on. (Nix builds have a read-only source tree, so nothing writes there
automatically; proptest's own seed files are disabled.) Regression files pin
the host-model schema: when `host_model` changes, they are migrated, and a
test fails if any regression file has an old version.

### 4. Differential contract tests

A model is only useful if it behaves like the kernel, and a hand-written
contract suite only covers the cases someone thought of. So the contract is
differential: `materialize(model)` builds a host model for real on a scratch
tmpfs inside a test container (directories, `fallocate`d files, bind mounts,
stacked mounts, symlinks, helper processes holding locks), and the same
scenarios (fixtures and generated ones) run on `ModelHost` and on
`LinuxHost`. Traces and outcomes must match. Model fields that cannot be
built for real in a rootless container (clock, kernel capabilities, exact
`statvfs` numbers, filesystem types other than tmpfs, NFS, scripted
Docker/Nix, delayed freeing) are marked *model-only* in the schema; tests
that use them depend on the model being right, and say so.

### 5. End-to-end tests in containers

End-to-end tests run the real binary in **rootless Podman containers**,
built from images that Nix produces (`dockerTools`). Containers run
everywhere, need no KVM, start fast, and isolate the test's filesystem,
processes and mounts from the developer's system.

- **Cargo race tests** 1–9 with the pinned Cargo-under-test toolchains
  (installed in the image under `/opt/cargo-under-test/{stable,nightly}`
  with a C toolchain), on fixture projects with no registry dependencies (or
  vendored ones), `CARGO_NET_OFFLINE=true` and a temporary `CARGO_HOME`.
  Races are deterministic, not timing-based: a helper holding `.cargo-lock`
  makes Cargo take `.cargo-build-lock` and block; a `build.rs` waits on a
  control file.
- **Mounts that a rootless container can create** in its own mount
  namespace: tmpfs, bind mounts (including a bind mount of the same
  filesystem), stacked mounts. This covers the path-traversal rules
  (symlink refusal, no mount crossing).
- **Docker**: a real Docker daemon in its own container (`docker:dind` under
  rootless Podman with `--privileged`, which stays inside the user
  namespace; verified working on this machine), images preloaded with
  `docker load` since tests run offline; BuildKit cache created by
  `FROM scratch` builds.
- **Nix GC**: a container with its own Nix store and `nix-daemon`; garbage
  is created inside it and the test asserts on what GC removes.
- **Filesystem profiles** (`pytest-tmp`, `nix-shell-rc`, `tmp-entry`, ...)
  on the scratch tmpfs mounted *at* the container's `/tmp`
  (so interlock layer 3 holds): entries held open, used as working
  directory, mapped, named in a process's `TMPDIR`, `flock`ed, containing a
  socket or FIFO, containing a mount, owned by another user (needs subuid
  ranges), a file created mid-delete, and a deleted-but-open file that must
  show up in `held_by_deleted_files`; plus a root with 100,000+ entries to
  check that ordering stays cheap and the report stays aggregated.
- Mounts and the other-owner cases need `CAP_SYS_ADMIN` inside the
  container's user namespace (rootless Podman grants it there) and subuid
  ranges for the test user.
- **Tally behaviour:** the duty cycle and the operation counts are
  asserted in model tests (through the `Host` port; `/proc/<pid>/io` reads
  close to zero on tmpfs and overlay, so it proves nothing in a
  container); in the container, a warm-index reclaim starts deleting
  within seconds on a 100,000-entry `/tmp`; a stale snapshot (wrong config
  hash or too old) is ignored and `ensure` falls back to a
  synchronous tally within `--tally-budget`.
- **Signals, SIGKILL, deadlines, the CLI-hook use of `ensure`, `watch`
  running as a plain process** (no systemd in the container).

The NixOS module is checked by evaluation in `nix flake check` (the
generated unit, tmpfiles rules and YAML are asserted on), not by booting a
system.

**Not covered end-to-end (for now), and why:** real block filesystems (ext4
or xfs as separate mounts, btrfs) and NFS cannot be mounted in a rootless
container; behaviour on specific kernel versions (the Linux 5.8 minimum,
`statx` capability fallbacks) follows whatever kernel the test host runs;
systemd and tmpfiles are not run for real. These cases rest on model tests
with model-only fields. A VM suite can be added later if that proves too
thin (see Weak assumptions & risks).

Why not FUSE: it has its own `flock`, `statx` and mount-id semantics, needs
privileges, and is a large extra moving part.

Container tests cannot run inside the Nix build sandbox, so they run from
`just e2e` (Nix builds the images and the test binary; Podman runs them),
not as `nix flake check` checks. They lose Nix's result caching.

### Safety interlock for destructive tests

An environment variable is not enough (it leaks through `export`, direnv,
CI settings). Three layers:

1. **Build-time**: destructive tests live in `diskgc-e2e`, which is
   excluded from the workspace; `cargo test`, `cargo test --workspace` and
   `just check` never compile it. It is built only into the test image.
2. **Run-time**: the test binary refuses to run unless a marker file baked
   into the test image exists (`/.diskgc-test-sandbox`, holding a fixed
   constant also compiled into the binary) and `systemd-detect-virt
   --container` reports a container. (A kernel command-line token would not
   work: a container sees the host's `/proc/cmdline`.)
3. **Per operation**: every test asserts that the paths it touches are on
   the scratch tmpfs the harness mounted for it (checked by mount id), and
   aborts otherwise.

### Commands

| `just` recipe | What | When |
|---|---|---|
| `check` | fmt, clippy, unit, planner, model tests, property tests (few cases), in the dev shell | constantly; seconds |
| `e2e` | `check` plus all container tests (builds the images with Nix, runs them with Podman) | **before every commit** (the gate); budget under 5 minutes, split the suite if it grows past that |
| `e2e-one NAME` | one container test, keeping the container for inspection on failure | while working on one |
| `soak` | property tests with many cases | before a release, nightly CI |
| `ci` | `nix flake check -L` plus `just e2e` | CI |

Requirements on a machine running `just e2e`: Nix and rootless Podman.
CI runners that are themselves containers need nested Podman (privileged
runner containers). Coverage of unit and model tests via `cargo llvm-cov`.

## What good looks like (observable)

- With reclaimable candidates available, no job on a watched host fails for
  lack of filesystem bytes unless writes outpace the reserve (see risks).
  Inode exhaustion and quotas are not covered.
- `ensure` exits 0 only if its final sample of available space is at least
  `--min-free`, and exits 0 without taking the lock when space is already
  there.
- **Cargo race tests pass**, each with real Cargo processes, run against
  every supported Cargo version (at least the oldest supported stable and
  one dated, pinned nightly):
  1. a build holding the lock: its target is skipped;
  2. a build that starts during deletion: it waits, then succeeds;
  3. one build blocked on the lock plus another started right after the
     reclaim: they never run concurrently and both succeed;
  4. a target whose test binary is still running: skipped;
  5. a new profile (`--release`, `--target`) started between discovery and
     locking: skipped as `changed`, its build succeeds;
  6. diskgc SIGKILLed during deletion at each point of a seeded list of
     positions in its call trace: the next `cargo build` always succeeds;
  7. a `cargo check` paused after taking `.cargo-build-lock` and before
     `.cargo-lock`, plus a concurrent `cargo build`: the target is skipped,
     no lock file is ever unlinked, both commands succeed;
  8. the compile-to-run handoff: `cargo run` / `cargo test` on an old,
     up-to-date target, paused deterministically between compilation and
     execution (a `runner` wrapper that waits): the target is skipped as
     `in_use`, the binary runs;
  9. a profile directory containing an unknown `.cargo-*lock*` file:
     skipped as `unsupported_layout`.
- With available space steady between the two thresholds, `watch` makes
  zero deletions, and its tally keeps its duty cycle (asserted in model
  tests through the `Host` port).
- With a warm index, the first deletion starts within seconds of available
  space crossing `--min-free`, even on a `/tmp` with 100,000+ entries (no
  tree walk before acting).
- The index stays within its bound (default 100,000 units) on such a
  `/tmp`, by bucketing small candidates.
- Every action produces one log line and one JSON `actions` entry.
- A deliberately misconfigured profile (e.g. a symlinked root) shows up as
  an error in the report, not as a silently empty tier.
- Every built-in profile's `examples` pass, and each built-in profile has a
  model test on a fixture taken from a real survey (anonymised).
- An invalid config exits 2 and deletes nothing; `check` reports the same
  error; `--json` reports it as `usage_error`.
- On an unchanged system and for the same index (or with `--no-index` and
  `--tally-budget 0`, so the synchronous tally completes), the
  real run's actions are a prefix of the *eligible*
  entries of the `--dry-run` plan (`ordered` groups, or `random` with the
  same seed; filesystem candidates, including Cargo).
- `ensure` never runs longer than `--timeout` + 10 s.
- Every invariant in What bad looks like has model tests and is checked by
  the property tests; the differential contract tests agree between
  `ModelHost` and `LinuxHost`; the container e2e suite passes for every supported
  Cargo version; `just e2e` stays under its time budget.
- No test touches the developer's live system.

## What bad looks like (must never happen)

These apply to diskgc's own actions (`delete` and the kind actions). A
`command` action can do anything its author writes; diskgc only guarantees
which candidates it runs for, where, and when.

- Anything deleted that no source profile found and no tier selected.
- Any Cargo lock file (`.cargo-lock`, `.cargo-build-lock`,
  `.cargo-artifact-lock`, or a later addition) deleted or moved.
- A target deleted while a Cargo process holds any of its locks, or two
  Cargo builds running in one target dir because of diskgc.
- Anything outside locked profile directories deleted by the `cargo-target`
  kind.
- A filesystem candidate deleted while a visible process has it open, uses
  it as working directory or maps it; a socket or FIFO deleted; a candidate
  of another user deleted without an explicit `owners:` list.
- A `!` or `protect` entry that *adds* a candidate, a candidate deleted
  that overlaps a negated or protected profile's match, or an eligible set
  that differs from the selection rules.
- A Docker volume or container, a Nix GC root, profile or generation removed.
- Deletion following a symlink or crossing into another mount.
- A `command` action run for a candidate that failed the safety checks, or
  in a working directory other than the validated candidate (or parent).
- `ensure` exiting 0 when the space is not there, or running past its
  deadline plus the 10 s termination grace.
- A static config error discovered only after something was deleted, or a
  tier's environmental check run only after other tiers deleted.

## Scope

In scope (v1): `ensure`, `watch`, `check`, `survey`, `profiles`; profiles
with `find` / `where` / `action` and the selection grammar; kinds `fs`,
`cargo-target`, `docker-build-cache`, `docker-images`, `nix-store`; the
initial built-in profile library; user profiles and overrides; groups with
`ordered` / `random` ranking and per-tier `order`; the index, background and
synchronous tally, snapshot, `watch --tally-only`, `--tally-budget`,
`--no-index`; `--json`; NixOS module; plain binary + YAML for CI.

## Non-goals

- Disk usage analysis (use `ncdu`, `nix-du`). `survey` reports only what
  profiles match and the unmatched top-level name families; it is no
  general browser or visualiser.
- Authoritative persisted state: no usage database, no cooldown
  timestamps. The index snapshot is a cache; deleting it only costs a
  synchronous tally.
- Copy-on-write filesystems with snapshots (Btrfs, ZFS), where deleting may
  free nothing. Warn on them.
- Event-driven watching (inotify/fanotify).
- Coordination across hosts.
- Removing Nix generations/profiles, Docker volumes/containers. (A user
  profile's `command` *could* run `nix-collect-garbage --delete-older-than`;
  no built-in profile does.)
- Cargo top-level entries outside profile directories (`target/doc`,
  `target/tmp`, `target/criterion`). A user profile can cover them if
  someone wants that.
- Inode exhaustion and user/project quotas: `ensure` reports bytes only.
- Running as root by default; Podman as the Docker backend for the Docker
  tiers (Podman is used only to run the e2e tests); rootless Docker; non-Linux; Linux
  older than 5.8.
- FUSE- or `LD_PRELOAD`-based filesystem simulation (see Testing strategy).
- A VM test suite, for now (real block filesystems, NFS, kernel versions,
  booted NixOS module). Revisit if model-only coverage proves too thin.

## Key decisions

| Decision | Chosen | Why |
|---|---|---|
| What vs when | Profiles (what and how: `find`, `where`, `action`) separate from tiers (which and when: `select` expressions in ordered groups) | The same cruft knowledge serves every machine; only the policy differs. |
| Profile composition | `\|\|` of terms, each term one source profile `&&` filters, `!filter` or `!profile`; one kind and one action per expression | Composable by name like the human asked; `a \|\| b` = `b \|\| a`; nothing is ever deleted that no source profile found. |
| Roots | Profiles have fixed roots or `roots: required`; only the latter take tier / `--roots` roots | A selection can never be re-pointed at a place its profile was not written for. |
| Profile library | YAML data inside the binary, each built-in with `examples`, model tests and `verified_with`; user profiles add, `override: true` to replace | Knowledge grows without code changes; no accidental shadowing. |
| Safety vs profiles | Safety checks are code, always on for every filesystem candidate; profiles cannot disable them | A wrong profile can at worst select the wrong *unused* files, never ones in use. |
| Age filter naming | One `idle_for` predicate, measured per kind and named in reports (supersedes round 4's per-type age names) | Composable `idle-*` filters across kinds; the report keeps the measure honest. |
| Ordering | Groups in list order; `ordered` or `random` tiers inside a group; candidates oldest first | Random spreads cache loss across a fleet without ever running pressure tiers before stale ones. |
| Thresholds | Command-line arguments | Same config for laptop and CI; smaller frozen schema. |
| Cargo deletion | Take all known lock files, delete inside locked profile dirs only, keep every lock file; unknown lock files → skip | Blocked and new Cargo processes stay on the same lock inodes; unlocked areas and unknown layouts are never touched. |
| Cargo in-use | Locks + `/proc` heuristics (exe/cwd in target, `cargo*` processes in the project or with `CARGO_TARGET_DIR`) + `idle_for` | Covers running binaries and the compile-to-run handoff; a heuristic, stated as such. |
| Path handling | fd-relative `openat2` from the mount root with `RESOLVE_BENEATH \| NO_SYMLINKS \| NO_XDEV` | Catches symlinks and bind mounts, which device comparison misses. |
| Special removal | `command` action on a profile (argv, `{path}`, working directory = the checked candidate) | Some cruft needs the tool's own cleaner; the candidate still gets the always-on safety checks first. |
| Protection | `!profile` per tier and a global `protect:` list, both by overlap (is, inside, contains); `!` only narrows | Per-tier exclusion where it is local, a global list where it must hold everywhere; overlap catches parents of protected dirs. |
| Nesting | Skip a candidate whose tree contains `.diskgc-keep` or anything a non-`delete` profile matches | A plain delete of a parent must never bypass a special action. |
| Gathering vs acting | Background tally builds an index of medium-grained units (one per candidate, small ones bucketed); reclaim pops a priority queue over it and revalidates each unit; synchronous tally with a time budget when no fresh index exists | No lengthy survey when the disk is already full; steady low I/O instead of bursts. |
| State | The index (in memory in `watch`, snapshot file for `ensure` / `survey`) is a cache, never an authority; all safety checks run at deletion time | A stale or missing index costs speed, never safety; wiped runners lose nothing that matters. |
| Order within a tier | `oldest` by default; opt-in `best-fit` (smallest unit covering the need, else largest) | Keeps the LRU gradient and small overshoot by default; `best-fit` frees something sizable in one step when asked. |
| Success measure | Final statvfs sample; per-action signed `avail_delta` as an observation | Tallies are wrong with hard links and Nix dedup; concurrent writers make per-action attribution impossible. |
| Concurrency | Fixed, provisioned `/run/diskgc/lock` | One lock inode per host, guaranteed. |
| Privileges | Non-root by default | Removes the "root runs commands from YAML" hazard. |
| `--target-free` default | `ensure`: equal to `--min-free`; `watch`: required | Asking for 50G must not delete 70G unless asked; a watcher needs two thresholds. |
| Language and build | Rust only; Nix flake; Cargo workspace | One toolchain for product and tests; `nix flake check` is the full gate. |
| Test seam | Pure planner + executor over a `Host` port; `LinuxHost` and YAML-driven `ModelHost` | Rules tested as data; weird filesystems and races tested in-process and deterministically; no FUSE or syscall interception. |
| Race tests in the model | Events addressed by position in the `Host` call trace | No hooks in the product; every interleaving of a small scenario can be enumerated. |
| Model fidelity | Differential: `materialize` the model for real in a test container, compare traces and outcomes | A hand-written contract suite only covers cases someone thought of. |
| Destructive-test safety | Crate outside the workspace + image marker file and container check + scratch-mount check | An environment variable leaks too easily. |
| E2E environment | Rootless Podman containers, never the live system; no VM suite for now | Runs on any machine and CI runner without KVM, fast; real block filesystems, NFS and kernel versions are left to the model. |

## Open questions

None blocking. Items to settle during implementation are listed as risks
below, each with the check that resolves it.

## Weak assumptions & risks

- **Cargo lock coverage (load-bearing, partly verified).** Probed on
  2026-10-15 by sampling `/proc/locks` during real Cargo commands:
  - Cargo 1.90 (stable) holds an exclusive `flock` on `<profile>/.cargo-lock`
    during a build.
  - Cargo 1.96 (nightly) holds `.cargo-build-lock` *and* `.cargo-lock`
    during a build; a second build waits on `.cargo-build-lock` first;
    `cargo check` waits behind a running build.
  - **Neither version holds any lock while the program runs** under `cargo
    run` or `cargo test`: the locks are free for the whole runtime of the
    binary, not just a moment. Running binaries are therefore protected
    only by the `/proc` process checks (executable or working directory
    inside the target, and the `cargo` parent process), which makes them
    essential, not a nicety.
  - `.cargo-artifact-lock` exists in one of this machine's targets but was
    not created by plain `build` / `run` / `test` on either version; which
    mode creates it, and the separate `build-dir` layout, are not verified.

  The allowlist plus `unsupported_layout` keeps unknown versions safe
  (skipped) rather than unsafe; the race tests, run per Cargo version,
  decide what is supported.
- **Half-deleted incremental state.** Deleting `.fingerprint/` first should
  force a full rebuild, but rustc loading a partly deleted `incremental/`
  session might still fail. Resolved by race test 6.
- **Last-build heuristic.** Directory mtimes change on real rebuilds but not
  on no-op builds, so an up-to-date, actively used target can look idle. The
  lock and process checks still protect visible running work.
- **Process heuristics, not proof.** Only the caller's own processes are
  visible (fewer with `hidepid`). A Cargo started from outside the project
  directory without `CARGO_TARGET_DIR` (e.g. `--manifest-path`, or a
  `.cargo/config.toml` `target-dir`) is not recognised in the
  compile-to-run window. Fine when diskgc runs as the owner of the targets;
  weaker under `--allow-root` on multi-user hosts. A strict guarantee would
  need Cargo itself to hold a lock until the run ends.
- **Burst writes.** Thresholds cannot protect against a write burst. The gap
  below `--min-free` must exceed write rate × (interval + slowest tier).
  Documented, not enforced.
- **Nix GC.** Can take minutes on a large store, holding the host lock
  meanwhile (hence the deadline). With `auto-optimise-store`, freed space can
  lag. Verified on Nix 2.34.8 (2026-10-15): an untrusted, non-root user can
  delete dead store paths through the daemon (`nix-store --delete` on a
  freshly added, unrooted path). The store in effect can be read without
  relying on the user's config by passing the feature flag per call:
  `nix --extra-experimental-features nix-command store info --json` (here:
  `"url":"daemon"`). `builtins.storeDir` is not enough: it still says
  `/nix/store` when `NIX_REMOTE` points at another store. Not verified:
  Lix and other Nix variants.
- **Docker.** Verified on Docker 29.9 (in a disposable `docker:dind`
  container, since this machine has Podman, not Docker): build cache `until`
  is last use; the containerd image store is the default
  (`driver-type io.containerd.snapshotter.v1`). Inside dind, containerd
  lives under `DockerRootDir` (`/var/lib/docker/containerd`); on a host
  with a system containerd, images live in `/var/lib/containerd` instead.
  Still to settle: how diskgc reliably finds the containerd root for
  filesystem matching. Remote contexts or builders are skipped, not
  guessed. Podman's `docker` shim is not supported (non-goal).
- **`command` actions.** Arbitrary commands, and the tool re-resolves the
  path it is given. Mitigated by the always-on safety checks before the
  command, non-root default, argv execution, `fchdir` working directory,
  `argv[0]` resolved and shown at load, the inherited host lock and the
  StrictModes check under `--allow-root`. Still the most dangerous surface.
- **No real block filesystems, NFS or kernel versions in e2e.** Without a
  VM suite, NFS refusal, the btrfs warning, xfs/ext4 mount handling, the
  Linux 5.8 minimum and `statx` fallbacks are tested only against the model
  (model-only fields). A wrong assumption there reaches users first. The
  NixOS module is evaluated, not booted.
- **Built-in profiles are only as good as the surveys behind them.** The
  initial library comes from one machine; paths and naming differ between
  tool versions and distributions. Mitigation: `verified_with`, `examples`,
  `survey` showing the unmatched remainder, and "more precise, never
  broader" for existing names.
- **`claude-code-tmp`** deletes finished Claude Code session dirs, which
  hold task output files. The in-use checks protect running sessions; a
  finished session's outputs are gone once its dir is deleted. The
  example config therefore protects it (`protect: [claude-code-tmp]`);
  selecting it is a deliberate choice, best in a pressure tier.
- **Expression complexity.** Users can write unreadable selections.
  `check` prints each tier's expression normalised, with the profiles it
  resolves to; `survey --select` shows what it would match before any
  deletion.
- **Index rot.** Files come and go faster than the tally re-checks them.
  Revalidation before acting and the deletion-time checks keep this a
  speed problem, not a safety problem; `stale` skips in reports show when
  the tally is falling behind.
- **Tally I/O.** Even throttled, a continuous tally of millions of files
  costs I/O and keeps metadata in the page cache. The duty cycle, idle I/O
  priority (honoured by BFQ only) and the pause during reclaim limit it;
  the duty cycle is asserted in model tests, not assumed.
- **Snapshot privacy.** The snapshot lists paths of the user's files; it is
  written mode 0600 in the user's state directory and contains no file
  contents.
- **Lock stalling.** Any local user can hold `/run/diskgc/lock`, and an
  orphaned child keeps holding it until it exits.
- **Asynchronous freeing.** Without `syncfs`, a filesystem that frees blocks
  in the background can make diskgc delete one candidate more than needed.
  Accepted: it is bounded by the overshoot rule.

## Irreversibility map

Hard to walk back (deep review, tests before features):
- **The deletion path:** the Cargo lock-in-place protocol and its lock-file
  allowlist, fd-relative
  deletion with symlink and mount rules, the always-on filesystem safety
  checks, `command` execution, filesystem matching, and the selection
  semantics (positivity, `!` only narrows).
- **CLI contract used by CI and harnesses:** `ensure` flags, exit codes and
  their precedence, JSON field names and enums, `.diskgc-keep`, the lock path
  `/run/diskgc/lock`.
- **YAML schema `version: 1`:** `groups`, `ranking`, `select` grammar and
  semantics, the profile schema (`find`, `where`, `action`, `kind`,
  `examples`, `override`), predicate names, and the *names* of built-in
  profiles and kinds (configs refer to them; a rename breaks them).
- **The built-in profiles' meaning**: a built-in profile may become more
  precise, never broader, without a new name. Broadening what an existing
  name deletes would silently widen every config that uses it.

Tentative (change freely):
- Example groups and tiers; default timeouts; back-off constants; discovery
  depth; human-readable output; log line format.
- The `Host` trait and the crate split: internal, free to change.
- The index structure, unit granularity, tally pacing and the snapshot
  format: a cache, discarded on any version mismatch, free to change.
- The YAML host-model schema (`host_model: 1`): test-only, but pinned by
  the regression files; changing it means migrating them.
