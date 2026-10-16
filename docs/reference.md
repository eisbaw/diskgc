# diskgc reference: CLI, configuration, profiles

Normative for everything a user or a script sees: commands, flags, exit
codes, output, the configuration file, the profile schema and the selection
grammar. Deletion safety is specified in [safety.md](safety.md), the index
and the reclaim walk in [architecture.md](architecture.md).

## Commands

```
diskgc ensure --min-free SIZE [--target-free SIZE] [--timeout DUR]
              [--tally-budget DUR] [--no-index] [--select EXPR] [--roots DIR]...
              [--dry-run] [--json] [--config FILE] [--allow-root] PATH
diskgc watch  --min-free SIZE --target-free SIZE [--interval DUR]
              [--select EXPR] [--roots DIR]... [--config FILE] [--allow-root] PATH
diskgc watch  --tally-only [--config FILE] PATH
diskgc check  [--config FILE]
diskgc survey [--select EXPR] [--roots DIR]... [--list] [--json] [--config FILE] PATH...
diskgc profiles list | show NAME | test   [--config FILE]
```

`--select EXPR` replaces the config's groups with one tier selecting `EXPR`
(the config's user profiles and `protect` list still apply). With
`--select`, a config file is optional: if none is found, `EXPR` is evaluated
against the built-in profiles alone. Without `--select`, a missing config is
exit 2 as described under Configuration file. `--roots DIR`
(repeatable, one directory each) supplies roots for `roots: required`
profiles in that selection, and is only valid together with `--select`.
Example for a CI runner:

```
diskgc ensure --min-free 50G \
  --select 'profile:cargo-target && filter:idle-1d' \
  --roots "$CI_BUILDS_DIR" \
  "$CI_BUILDS_DIR"
```

The `--roots` value is where the profile looks; the final `PATH` is the
directory whose filesystem must have the space.

### `ensure`

- `PATH` is required: the directory the next job will write to. Its
  filesystem is the one diskgc measures and frees. There is no default to
  the current directory, so a hook launched from `/tmp` cannot silently free
  the wrong disk. The resolved mount point is printed in every report.
- `--min-free` (required): if available space is at least this, do nothing
  and exit 0, without taking the lock.
- `--target-free` (default: same as `--min-free`): once triggered, keep
  reclaiming until this much is available. Setting it higher is the
  hysteresis: a fleet of CI jobs then does one larger cleanup instead of a
  small one per job. The exit code depends only on `--min-free`; missing the
  target is reported (`target_met: false`) but is not a failure.
- `--timeout` (default `10m`, `0` = none): a **cooperative deadline** for
  the whole call, including waiting for another reclaim. At the deadline
  diskgc starts no new work and terminates a running child (SIGTERM, SIGKILL
  after 10 s). diskgc checks the deadline between operations; a single
  filesystem operation that blocks (a hung network mount, a slow disk)
  cannot be interrupted and can overrun it. See [safety.md](safety.md#deadlines).
- `--tally-budget` (default: the smaller of 30 s and a quarter of
  `--timeout`; `0` = no limit) and `--no-index`: see
  [architecture.md](architecture.md#the-tally).
- `--dry-run`: print, in order, every candidate diskgc would consider,
  whether it is eligible or the skip reason, its estimated size and the
  operation it would run (JSON: the `plan` array); delete nothing. Takes no
  lock and always prints, even if the goal is already met. With a `random`
  group, the order is for the printed seed only. Sizes are estimates; Docker
  and Nix tiers can only say what they would run. Exits 0 (2 on usage
  errors, 124 at the deadline, 130/143 when interrupted); JSON `status` is
  `dry_run`.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors.

### `watch`

- One process per filesystem, under any supervisor (e.g. one instance of a
  systemd template unit per watched path; systemd is not required).
- `--target-free` is required here: a watcher without two thresholds would
  reclaim a sliver at every poll. The error message says so.
- Every `--interval` (default `10s`): one `statvfs`. At or above
  `--min-free`: no deletions; only the background tally keeps the index
  fresh.
- Below `--min-free`: run the reclaim walk with goal `--target-free`. No
  overall deadline; each action is bounded by its tier's `timeout`. The wait
  for the host lock stays interruptible by SIGTERM.
- `--tally-only`: maintain the index and its snapshot, never reclaim (for
  CI runners, so that `ensure` in the pre-job hook finds a warm index).
  `--min-free` / `--target-free` are not used in this mode.
- **Back-off:** if a reclaim ends below `--min-free` because everything left
  is in use (`exhausted`) or with misconfiguration errors, the next one
  starts only after available space has dropped by a further
  `max(1 GiB, 1% of the filesystem)` or 15 minutes have passed. Backend
  errors (a Docker or Nix daemon unreachable) shorten the wait to 1 minute,
  since backends often recover quickly. One log line per back-off. These
  values are fixed in code.
- On SIGTERM/SIGINT: stop as described under Signals, exit 0.

### `survey`

Read-only: takes no lock and deletes nothing. For each `PATH`:

- per profile: candidate count, total estimated size, age range, how many
  are in use;
- with `--list` (and always in `--json`): the **candidates themselves**,
  one line each, with size, age and in-use state, so a human reviews
  concrete paths, not globs. Output beyond 1,000 candidates per profile is
  truncated, and the report says how many were left out;
- the *unmatched* top-level entries grouped by name family, so the next
  profile worth writing is visible. A name family is the name with runs of
  digits replaced by `#` and a trailing random-looking suffix after `.`, `-`
  or `_` (five or more letters and digits) replaced by `*`, e.g.
  `nix-shell.8raHjU` → `nix-shell.*`, `nix-shell-1144586-3257819220` →
  `nix-shell-#-#`. The grouping is a hint for humans, not part of the JSON
  contract.

Without `--select`, all profiles (built-in and user) are applied; each
`PATH` also serves as the root for `roots: required` profiles, so
`diskgc survey ~/src` finds Cargo targets there. `survey` uses a fresh
snapshot when one exists, otherwise it tallies.

### `profiles` command

`list`: name, category, kind, description of every profile (built-in and
user, marking overrides). `show NAME`: the full YAML as diskgc understands
it. `test`: run every profile's `examples` against an in-memory model; exit
1 on any failure.

### `check`

Static validation only, machine-independent: load the config, print the
resolved config path and each tier's expression normalised with the
profiles it resolves to, exit 0 or 2. It needs nothing from the deployment
machine (no `PATH` lookups, no filesystem access beyond reading the config),
so packaging and configuration tools run it before deploying (the NixOS
module at build time, also when cross-building). Everything that depends on
the machine is checked at run time; see
[safety.md](safety.md#validation).

## Exit codes (`ensure`)

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

The mapping is explicit in code and covered by tests. A Rust panic (101) is
a bug.

## Output

- stdout carries only the result. Human mode: silent when nothing needed
  doing (like `cp`); otherwise a short summary. `--json`: always one object.
- stderr carries progress logs: one line per action with tier, profile,
  candidate, observed change in available space, duration. Output of child
  commands (Docker, Nix, command actions) goes to stderr, prefixed with the
  tier and profile name.
- `watch` logs to stderr only; a supervisor such as journald collects it.

JSON result (field names are part of the contract):

```json
{
  "status": "met | exhausted | error | usage_error | lock_timeout | deadline | interrupted | dry_run",
  "mount": "/", "path": "/home/mark/src/app", "seed": null,
  "index": {"source": "live | snapshot | sync", "age_s": 0, "units": 0, "untallied": 0, "coverage": "complete | partial"},
  "min_free_bytes": 0, "target_free_bytes": 0, "target_met": false,
  "avail_before_bytes": 0, "avail_after_bytes": 0,
  "plan":     [{"group": "", "tier": "", "profile": "", "candidate": "", "eligible": true, "reason": null, "estimated_bytes": 0, "operation": ""}],
  "actions":  [{"tier": "", "profile": "", "candidate": "", "avail_delta_bytes": 0, "duration_ms": 0, "count": 1}],
  "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | not_idle | changed | empty | kept | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_store | unsupported_backend | missing | unsafe_path | contains_ipc | contains_mount | contains_special | protected | excluded | not_owner | has_not | stale"}],
  "held_by_deleted_files": [{"pid": 0, "command": "", "bytes": 0}],
  "overruns": [{"operation": "", "path": "", "over_ms": 0}],
  "errors":   [{"tier": "", "profile": "", "candidate": "", "message": ""}]
}
```

- `seed` is `null` when no group uses `random`.
- `index` says where the order came from (the live index of a running
  `watch`, a snapshot, or a synchronous tally), its age, how many candidates
  were not yet tallied, and whether the index covers everything it should
  (`partial` when it hit its memory budget).
- `plan` is filled only by `--dry-run`; `reason` uses the skip-reason enum; a
  bucket is one plan entry with a member `count`; `estimated_bytes` is
  `null` for Docker and Nix without a reported size and for candidates not
  yet tallied.
- `avail_delta_bytes` is signed: the observed net change in available space
  around an action, never "bytes freed by this action" (see
  [architecture.md](architecture.md#measuring-space)).
- `count` is 1 for a single candidate. Selections that match very many
  candidates aggregate: one `skipped` entry per profile and reason with
  `count` and up to 20 `examples` (`candidate` is then empty), and after
  1,000 `actions` a single summary entry with `count`.
- `held_by_deleted_files` lists open-but-deleted files on the requested
  filesystem (visible processes only); always present, possibly empty. Their
  space returns only when the process closes them; diskgc never kills
  processes.
- `overruns` lists operations that blocked past the deadline (see
  [safety.md](safety.md#deadlines)); always present, usually empty.
- On a usage error, fields that could not be determined (`mount`, `path`,
  sizes) are `null`; `errors` says why.

## Signals

On SIGINT/SIGTERM: do not start another candidate; send SIGTERM to the
running child's process group, SIGKILL after 10 s; release locks; print the
report; exit 130/143 (`watch`: 0). Lock inheritance by children is
specified in [safety.md](safety.md#host-lock).

## Sizes and durations

- `K M G T` and `KiB MiB GiB TiB` are powers of 1024; `KB MB GB TB` are
  powers of 1000. A bare number is an error.
- `N%` is N percent of the filesystem size (`f_blocks × f_frsize`).
- Durations: integer plus `s m h d w`. No months; `m` is always minutes.
- `--target-free` below `--min-free`, or either above the filesystem size,
  is a usage error (exit 2).

## Configuration file

Lookup: if `--config` or `$DISKGC_CONFIG` is given, that file and only that
file is used; missing or unreadable is exit 2, never a fallback, so a typo
cannot activate some other policy. Only when neither is given:
`$XDG_CONFIG_HOME/diskgc/config.yaml` (default `~/.config/...`), then
`/etc/diskgc/config.yaml`. None found: exit 2, listing the paths tried. The
binary ships built-in *profiles* but **no built-in tiers**; an example config
ships as `docs/example.yaml`, which packages install as the default.

The config holds `version`, optional user `profiles`, optional `protect`,
optional `owners` (only with `--allow-root`), and the tier `groups`.
Thresholds, intervals and timeouts are command-line arguments, so the same
file serves `watch` on a laptop and `ensure` on a runner.

```yaml
version: 1

profiles:                          # optional: your own, or overrides
  - name: zephyr-run
    category: build
    find: {roots: [/tmp], paths: ["zephyr-*-run.*"], type: dir}
    action: delete
    examples: {match: [/tmp/zephyr-h5e5-run.jTm9zGlH], no_match: [/tmp/zephyr-hal]}

protect:                           # never deleted by any tier (by overlap)
  - claude-code-tmp                # fixed-root profile: bare name
  - {profile: cargo-target, roots: [/home/mark/src/important]}

groups:
  - name: stale                    # nobody will miss these
    ranking: random                # ordered (default) | random
    tiers:
      - name: test-and-shell-cruft
        select: (profile:pytest-tmp || profile:nix-shell-rc || profile:zephyr-run) && filter:idle-3d
      - name: cargo-stale
        select: profile:cargo-target && filter:idle-7d
        roots: [/home/mark/src, /home/mark/topics]
      - name: docker-cache-stale
        select: profile:docker-build-cache && filter:idle-7d
      - name: nix-dead
        select: profile:nix-garbage

  - name: pressure                 # costs rebuild or download time
    tiers:
      - name: docker-dangling
        select: profile:docker-dangling
      - name: tmp-old
        select: profile:tmp-entry && filter:idle-2d
      - name: cargo-recent
        select: profile:cargo-target && filter:idle-1h
        roots: [/home/mark/src, /home/mark/topics]
```

Rules:

- **Groups run in list order. Always.** Escalation (stale before pressure)
  is group order; nothing reorders groups.
- **`ranking` decides tier order inside a group:** `ordered` (default, list
  order) or `random` (shuffled on each reclaim, so runners in a CI fleet
  lose different caches first and the fleet as a whole stays warmer; the
  seed is in the report and can be fixed with `DISKGC_SEED`).
- **Within a tier**, candidates are taken in the tier's `order` (`oldest`
  by default, or `best-fit`; see
  [architecture.md](architecture.md#order-within-a-tier)).
- **Roots:** a tier's `roots:` applies only to the `roots: required`
  profiles in its expression; fixed-root profiles in the same expression
  keep their own roots. A `roots: required` profile without roots, or
  `roots:` on a tier with no `roots: required` profile, is a load-time
  error.
- **`owners:`** (user names) is required with `--allow-root` and forbidden
  otherwise. Names are resolved at run time (environmental validation);
  `root` and system users (uid below 1000) are rejected there.
- **Paths** must be absolute. `~` is rejected: it would mean `/root` for one
  caller and `/home/mark` for another. The only placeholder is `{uid}` (the
  effective user's id, e.g. in `/tmp/claude-{uid}`).
- **Validation:** the config is validated completely at load, before
  anything is deleted; the full list of static and run-time checks is in
  [safety.md](safety.md#validation). `version` is mandatory; tier names are
  unique across all groups.
- Disabling a tier = commenting it out. There is no `enabled` field.

### Tier fields

| Field | Meaning |
|---|---|
| `name` | Unique; used in logs and reports. |
| `select` | Profile expression (see Selection grammar). |
| `roots` | Roots for the `roots: required` profiles in `select`. |
| `order` | `oldest` (default) or `best-fit`. |
| `timeout` | Per-action limit (default `10m`); a child is killed after it. |

### `protect`

Profiles whose matches no tier ever deletes, by **overlap**: a candidate
that is a protected match, lies inside one, or contains one is skipped
(`protected`). Entries are bare names for fixed-root profiles, or
`{profile: NAME, roots: [...]}` for `roots: required` profiles; a bare
`roots: required` name is a load-time error, so protection never depends on
which tier happens to supply roots. Only kinds `fs` and `cargo-target` can be
protected (Docker images and Nix paths are protected by Docker's and Nix's
own means: running containers, GC roots). The example config protects
`claude-code-tmp`.

### Protection marker

A file named `.diskgc-keep` in a filesystem candidate, anywhere inside it,
or in any directory between a profile root and the candidate, excludes it
(reported as `kept`).

## Profiles

diskgc's knowledge of *what* can be deleted and *how* lives in **profiles**:
named, composable rules for recognising one kind of cruft. Tiers say which
profiles apply and when.

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
  type. `find` names candidates *positively*; nothing that no source profile
  found is ever a candidate.
- **Roots are fixed or required.** A profile either has fixed roots (`/tmp`,
  `/var/tmp`) or declares `roots: required` (machine-specific locations such
  as `cargo-target`, `pytest-cache`), which only a tier, `--roots` or a
  `protect` entry can supply.
- **Filter profiles** have only `where` (predicates). They narrow a source's
  candidates; they never find anything.
- **`action`** says how a source profile's candidates are removed. For the
  code-backed kinds the kind fixes the action.
- **`kind`** (default `fs`): `fs` (a file or directory), or a code-backed
  kind: `cargo-target`, `docker-build-cache`, `docker-images`, `nix-store`.
- **`examples`**: paths that must and must not match. They check that the
  patterns do what their author meant; they do not prove that the matched
  files are disposable (a human decides that, from `survey --list`).
  Structural recognition (markers, Cargo layouts) is covered by fixtures in
  the test suite, not by path examples.

### Predicates (`where`)

| Predicate | Meaning | Kinds |
|---|---|---|
| `idle_for` | not used or changed for this long, as the kind measures it (below) | `fs`, `cargo-target`, `docker-build-cache` |
| `has` / `has_not` | contains / does not contain a named file (e.g. `CACHEDIR.TAG`) | `fs` |

A predicate on a kind that does not support it is a load-time error. Two
`idle_for` in one term combine to the longest. Ownership is not a
predicate: it is an always-on check ([safety.md](safety.md)).

| Kind | `idle_for` means |
|---|---|
| `fs` | newest mtime or ctime in the candidate's tree (atime is ignored: under `relatime` any reader, such as a nightly scan, refreshes it) |
| `cargo-target` | last build: newest mtime of the profile dirs and their `.fingerprint/`, `deps/`, `build/`, `incremental/` |
| `docker-build-cache` | last use, as recorded by Docker (`--filter until=`) |

The report always names the measure used.

### Selection grammar

A tier selects candidates with an expression over **qualified** names:
`profile:` for a source profile, `filter:` for a filter profile.

```
expr    := term | union
union   := uterm ( '||' uterm )+
uterm   := profile | '(' term ')'
term    := sources ( '&&' factor )*
sources := profile | '(' profile ( '||' profile )* ')'
factor  := filter | '!' filter | '!' profile
profile := 'profile:' NAME
filter  := 'filter:' NAME
```

Examples:

```
profile:pytest-tmp && filter:idle-10d
(profile:pytest-tmp || profile:nix-shell-rc) && filter:idle-3d
profile:tmp-entry && filter:idle-2d && !filter:idle-30d       # idle 2 to 30 days
profile:tmp-entry && filter:idle-2d && !profile:claude-code-tmp
```

- **Qualifiers are required.** A term without a `profile:` is rejected by
  the parser; a wrong qualifier (`filter:pytest-tmp`) is a load-time error
  naming what the name actually is. Fields that can only hold profiles
  (`protect`) and a profile's own `name:` take bare names.
- **No operator precedence to remember.** `||` and `&&` cannot be mixed at
  one level without parentheses: `profile:a || profile:b && filter:f` is a
  parse error, because under the usual precedence it would delete all of
  `a` unfiltered. A parenthesised group of profiles distributes:
  `(profile:a || profile:b) && filter:f` means
  `(profile:a && filter:f) || (profile:b && filter:f)`. Parentheses do not
  nest beyond that, filters cannot appear inside a profile group, and `!`
  applies to one name: `!(profile:a || profile:b)` is written
  `!profile:a && !profile:b`.
- **One source per term** (after distribution). Intersecting two source
  profiles that find at different depths matches only by accident.
- **`||` is a union**; all source profiles in one expression share one kind
  and one action, so `a || b` and `b || a` select and remove exactly the
  same. Different actions belong in different tiers.
- **`!` only narrows.** `!filter:…` is a path-local complement.
  `!profile:…` removes every candidate that **overlaps** a match of that
  profile (is it, lies inside it, or contains it), using everything the
  profile finds regardless of its `where`; excluded candidates are reported
  as `excluded`. A negated profile must be of kind `fs` or `cargo-target`,
  is only allowed in `fs` / `cargo-target` expressions, takes the tier's
  roots if it needs them, and does not count towards "one kind and one
  action".
- **Non-`fs` kinds** allow only `profile:NAME` (`docker-images`,
  `nix-store`) or `profile:NAME` / `profile:NAME && filter:idle-*`
  (`docker-build-cache`): they have no per-candidate enumeration.
- Filters are evaluated from the index where it already knows the answer,
  and always re-evaluated at deletion time. A candidate that fails a filter
  is reported as skipped with that filter's reason (`not_idle`, `has_not`).

### Actions

| Action | Use |
|---|---|
| `delete` | diskgc's own deletion ([safety.md](safety.md#deletion)). Option `make_writable: true` adds owner write permission to directories during the delete walk, after the last in-use recheck, for read-only trees such as Go's module cache or files copied from the Nix store. Opt-in per profile, since a read-only directory can be a deliberate protection. |
| `command` | an argv list run per candidate, e.g. `[env, "GOCACHE={path}", go, clean, -cache]`. Trusted, arbitrary command execution; never run as root. See [safety.md](safety.md#command-actions). |
| kind actions | Cargo lock protocol, Docker prune, Nix GC: fixed by the kind ([safety.md](safety.md#cargo-target)). |

Command action fields: `command` (argv; `{path}` is replaced by the
candidate path as one argument or inside one, never through a shell; use
`[sh, -c, "..."]` explicitly if a shell is wanted), `cwd` (`candidate`, the
default for directories, or `parent`), `timeout`. Environment passed:
`DISKGC_PATH`, `DISKGC_NEED_BYTES`, `DISKGC_MOUNT`, `DISKGC_PROFILE`. Most
cleaners pick their cache from their own config or environment, not from
the working directory, so the command must name the location explicitly
(`pip --cache-dir {path} cache purge`, `env GOCACHE={path} go clean
-cache`). A non-zero exit is an error in the report (exit 3 if the goal is
not met).

### Built-in profile library

Shipped inside the binary as YAML data, listed with `diskgc profiles list`.
A built-in profile ships only with `examples`, model tests, structural
fixtures where it relies on markers, and a `verified_with` line naming the
tool version it was checked against on a real system. Every row below is a
v1 *target*; until verified it is listed as planned, not shipped.

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | `pytest-of-*` in `/tmp`, `/var/tmp`, and one level down (nix-shell / mktemp dirs) | `delete` |
| test | `pytest-cache` | `.pytest_cache` (roots required) | `delete` |
| nix | `nix-shell-rc` | `/tmp/nix-shell-*-*`, `/tmp/nix-develop-*-*` (rc dirs left by every shell start) | `delete` |
| nix | `nix-shell-tmpdir` | `/tmp/nix-shell.*` (a shell's `TMPDIR`) | `delete` |
| nix | `nix-garbage` | dead store paths | Nix GC |
| rust | `cargo-target` | Cargo `target/` dirs (roots required) | lock protocol |
| containers | `docker-build-cache` | BuildKit cache of the default builder | prune |
| containers | `docker-dangling` | untagged images | prune |
| containers | `docker-unreferenced` | images no container uses (removes tagged images too) | prune |
| ai agents | `claude-code-tmp` | `/tmp/claude-{uid}/*/*` session dirs | `delete` |
| browsers | `chromium-tmp` | `.org.chromium.Chromium.*` and similar automation profiles in `/tmp` | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required) | `command` (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `jest-cache`, `playwright-tmp`, `node-compile-cache` | jest's cache dir; `playwright*` in `/tmp`; `node-compile-cache` in `/tmp` and nix-shell temp dirs | `delete` |
| c / c++ | `cmake-build` | directories containing `CMakeCache.txt` (roots required) | `delete` |
| jvm | `gradle-caches`, `maven-repo` | `~/.gradle/caches`, `~/.m2/repository` (roots required) | `delete` |
| embedded | `zephyr-tmp` | `/tmp/zephyr-*` scratch and `zephyr-*-run.*` dirs | `delete` |
| embedded | `west-build` | Zephyr `build/` dirs (`CMakeCache.txt` plus `zephyr/`; roots required) | `delete` |
| embedded | `renode-tmp` | `renode-*` in `/tmp` and nix-shell temp dirs | `delete` |
| embedded | `platformio-cache` | PlatformIO's `.cache` (roots required) | `delete` |
| generic | `tmp-entry` | any top-level entry of `/tmp`, `/var/tmp` | `delete` |
| generic | `mktemp-dir` | `/tmp/tmp.*` | `delete` |
| filters | `idle-1h`, `idle-1d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d`, `has-cachedir-tag` | (no `find`) | none |

- `sccache` is deferred: whether its server is using the cache cannot be
  established from open files (an idle server need not hold any); it needs
  a server-aware protocol first.
- Gradle and Maven caches are protected only by the generic open-file
  checks; a Gradle daemon that is idle may hold nothing open. Use them in
  pressure tiers.
- Profiles that find at any depth under a project root (`pytest-cache`,
  `cmake-build`, `west-build`) cannot use `**`; they list depths explicitly
  or take one root per project. Settled when they are verified.

**A built-in profile's matching set never changes under its name.** Any
change to what a built-in matches, narrower or broader, ships under a new
name (e.g. `claude-code-tmp-2`). An old name is never redefined; a new config
`version` may retire it (using a retired name is then a load-time error). Narrowing is not safe either: a narrower profile protects
less where it is used in `!profile:` or `protect`. Each built-in profile has
a golden match set over fixtures; any change to it fails CI unless the
profile was renamed.

### User profiles

- The config's `profiles:` list adds profiles. A user profile with the name
  of a built-in must say `override: true`; otherwise it is a load-time
  error, so nobody shadows a built-in by accident.
- `diskgc profiles test` runs every profile's `examples` against an
  in-memory model, so a profile is checked before it touches a disk.
- `check` warns about a user profile whose last path segment is only
  wildcards outside `/tmp` and `/var/tmp` (e.g. `roots: [/home/mark]`,
  `paths: ["*"]`).
- **Profiles are meant to be written by an AI and checked by the tool**,
  because globs are hard for humans to read and concrete paths are not:
  `diskgc survey --json` shows what no profile matches yet; an AI agent
  drafts a profile with `examples` taken from real paths it saw;
  `diskgc profiles test` checks the examples; `diskgc survey --list
  --select profile:<new>` lists every path the profile would match on this
  machine. The human reviews that list and decides. The examples only show
  that the patterns match what the author intended; whether those files are
  disposable is the human's judgement from the list.
