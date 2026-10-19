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
diskgc profiles list [--json] [--config FILE]
diskgc profiles show NAME [--json] [--config FILE]
diskgc profiles test [--config FILE]
diskgc --help | --version
```

`--select EXPR` replaces the config's groups with one tier selecting `EXPR`
(the config's user profiles and `protect` list still apply); reports name
that tier and its group `--select`. With `--select`, a config file is
optional: if none is found, `EXPR` is evaluated against the built-in
profiles alone. Without `--select`, a missing config is exit 2 as described
under Configuration file. `--roots DIR` (repeatable, one directory each)
supplies roots for `roots: required` profiles in that selection, and is
only valid together with `--select`. A root that cannot be reached safely
(`unsafe_path`: a symlink or a mount crossing on the way) is a
misconfiguration, reported once as an error that excludes the tier
([safety.md](safety.md#validation)). Example for a CI runner:

```
diskgc ensure --min-free 50G \
  --select 'profile:cargo-target && filter:idle-1d' \
  --roots "$CI_BUILDS_DIR" \
  "$CI_BUILDS_DIR"
```

The `--roots` value is where the profile looks; the final `PATH` is the
directory whose filesystem must have the space. A complete pre-job hook
built on this call, branching on the exit code and keeping the `--json`
report, is [ci-pre-job.sh](ci-pre-job.sh); the CI operator's journey test
runs it as written.

### `ensure`

- `PATH` is required: the directory the next job will write to, as an
  absolute path. Its filesystem is the one diskgc measures and frees. There
  is no default to the current directory, and a relative `PATH` is a usage
  error, so a hook launched from `/tmp` cannot silently free the wrong
  disk. It is reached like every configured path (no symlink on the way,
  [safety.md](safety.md#path-traversal)). The resolved mount point is
  printed in every report.
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
  [architecture.md](architecture.md#the-tally). Without an index (before
  task-29), `--no-index` changes nothing, and `--tally-budget` bounds the
  tally that orders the `--select` tier under the host lock: candidates it
  leaves untallied are taken after the tallied ones, in the order
  discovery found them, each judged in full under its own locks. A
  candidate the tally refuses (e.g. kept, another user's, too many entries) is
  skipped with that reason for the run and never locked; a refusal that
  stops being true later (a `.diskgc-keep` removed after the tally) is a
  missed reclaim until the next run, never an unsafe delete. The dry run
  judges every candidate fully and is not bounded by it.
- `--dry-run`: print, in order, every candidate diskgc would consider,
  whether it is eligible or the skip reason, its estimated size and the
  operation it would run (JSON: the `plan` array); delete nothing. The
  eligible candidates come first, in the order a reclaim would take them
  when its tally is not cut short (`--tally-budget 0`), then the skipped
  ones; a reclaim stops at the goal, the plan does not.
  Takes no lock (so it cannot tell `locked`: whether a target's locks are
  free is only tried by the reclaim) and always prints, even if the goal
  is already met. It deletes nothing, so the deletion's freshness bound
  does not apply: the plan of a `cargo-target` tier reuses a process scan
  for half a second, as a reclaim does, and that of an `fs` tier for up to
  a minute (half a second for a scan that blocks every candidate). With a `random` group, the order is for the printed seed
  only. Sizes are estimates; Docker and Nix tiers can only say what they
  would run. Exits 0 (2 on usage errors, 124 at the deadline, 130/143 when
  interrupted); JSON `status` is `dry_run`.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors. Any argument `--json` (or `--json=...`, a
  usage error) asks for it, wherever it stands, also where the parser takes
  it as another flag's value (`--config --json`). `--help` (or `-h`) wins
  over it: the help is printed as text, exit 0.

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

Read-only: takes no lock, measures nothing, deletes nothing. The library
is the built-in profiles plus the configuration file's user profiles,
found as described under Configuration file (its groups are not read).
Without a configuration file (none found by the search), the built-ins
alone, and no error. A given file (`--config`, `$DISKGC_CONFIG`) that
is missing, or any file that does not load, is exit 2 with the same
message `ensure` gives. The resolved configuration path is logged on
stderr (`diskgc: config PATH`).

- `list`: every profile, one row each, in one aligned table, the values
  of `ORIGIN`, `ROLE`, `KIND`, `ACTION` and `CHECKOUTS` spelled as in the
  JSON below so the table can be grepped: `NAME`,
  `ORIGIN` (`built_in` or `user`), `ROLE` (`source`: selected as
  `profile:NAME`; `filter`: `filter:NAME`), `CATEGORY`, `KIND`, `ACTION`
  (`delete`, `delete make_writable`, `command PROGRAM`, where `PROGRAM` is
  the one `env` runs for an `[env, NAME=value, ...]` command, or
  `cargo_lock_protocol`), `ROOTS` (the fixed roots, comma-separated, or
  `required`, which the JSON says as `find.roots_required: true`),
  `CHECKOUTS` (`refuse` or `allow`, so every profile that
  says `checkouts: allow` is marked), `DESCRIPTION`; `-` where a column
  does not apply (a filter has no kind, action, roots or checkouts).
  Sources come first, by name, then filters, the shortest `idle_for`
  first. Overrides are marked once `override: true` exists (task-17).
- `show NAME`: the profile as YAML in the schema of Profiles, as diskgc
  understood it: a comment line saying where it comes from, then the
  profile as a list item with every default spelled out (`kind`,
  `checkouts`, a command's resolved `cwd`, empty `no_match`). A kind's
  fixed action (`cargo-target`'s lock protocol) is a comment, since the
  schema takes no `action` there. Pasted under a config's `profiles:`,
  it loads as the same profile (a built-in's name needs `override: true`
  there, task-17). An unknown `NAME` is exit 2, naming the close names
  (those containing it or contained in it, then the fewest edits away,
  at most two) or, without any, pointing to `profiles list`.
- `test`: run every profile's `examples` against an in-memory model;
  exit 1 on any failure. Not in this build yet (task-17): exit 2.
- Exit codes of `list` and `show`: 0 when the list or the profile was
  printed; 2 for a usage error, a startup check that fails (as for every
  command but `--help`, `--version` and `check`), a configuration that is
  missing (when given) or does not load, or an unknown `NAME`. Nothing
  else. (`test` is exit 2 in this build.)

`--json` prints exactly one JSON object on stdout, for every exit code
(any argument `--json` asks for it, as for `ensure`). A profile is the
same object in `list` and `show`: every field of the schema, always
present, `null` where the profile's role or kind has none. Values that are
YAML identifiers keep their YAML spelling (kind names, `delete`,
`command`, `dir`); values only the JSON has are snake_case, like the
report's enum values (`status`, `reason`): `built_in`,
`cargo_lock_protocol`. (The report's `operation` is human-readable text,
not one of these.)
`config` is the configuration file read, `null` for none. `list` holds
the profiles in the order of the table, each the object `show` prints
under `profile`:

```json
{"config": "/home/mark/.config/diskgc/config.yaml", "profiles": []}
```

`show NAME`:

```json
{
  "config": null,
  "profile": {
    "name": "zephyr-run",
    "origin": "built_in | user",
    "role": "source | filter",
    "category": "build",
    "description": "Zephyr test runs left in /tmp",
    "kind": "fs",
    "find": {"roots": ["/tmp"], "roots_required": false, "paths": ["zephyr-*-run.*"], "type": "dir | file | any"},
    "where": {"idle_for": null, "has": null, "has_not": null},
    "action": {"type": "delete | command | cargo_lock_protocol", "make_writable": false, "command": null, "cwd": null},
    "checkouts": "refuse | allow",
    "examples": {"match": ["/tmp/zephyr-h5e5-run.jTm9zGlH"], "no_match": ["/tmp/zephyr-hal"]},
    "verified_with": null
  }
}
```

- `role` says how a selection names the profile: `source` as
  `profile:NAME`, `filter` as `filter:NAME`.
- `kind`, `find`, `action`, `checkouts` and `examples` are `null` for a
  filter; `examples` is also `null` for `cargo-target`, which takes none;
  `verified_with` is set on built-in sources only.
- `find.roots` is the list of fixed roots, or `null` when
  `find.roots_required` is `true` (`roots: required`: a tier or `--roots`
  supplies them); `find.paths` and `find.type` are `null` for
  `cargo-target`.
- `where` always has its three predicates, `null` when unused;
  `idle_for` is a duration (`3d`).
- `action` has every field: `make_writable` (a boolean) for `delete`;
  `command` (the argv) and `cwd` (`candidate`, `parent`, or `null` when
  it is decided per candidate: `type: any` without `cwd`) for `command`;
  neither for `cargo_lock_protocol`. Later kinds add their own `type`.
- On an error (exit 2): `{"status": "usage_error", "exit_code": 2,
  "error": "message"}`, the message as on stderr; `status` is the field
  `ensure`'s report has too. Every command but `ensure` fails with this
  object under `--json`.

The JSON of `profiles` is a pre-freeze addition, reviewed with the rest
at the contract freeze (task-26).

`diskgc --help` (or `-h`, anywhere on the command line: `diskgc profiles
list --help` is the same) prints the usage, then a short "Start here"
block so an agent can begin from the binary alone: a dry-run example
first, what `PATH` and `--roots` are, that `--min-free` is required also
with `--dry-run`, that `ensure` without `--dry-run` deletes, the `--json`
promise, and `ensure`'s exit codes, with the dry run's exit 0 whatever
the goal (read `target_met`). It ends with a Profiles
section generated from the built-in library at run time: one line per
built-in source profile (name, category, description, and `(roots
required: --roots DIR)` where it applies), the built-in filters on one
line, then where to learn more (user profiles are listed by `diskgc
profiles list`, details by `diskgc profiles show NAME`, machine-readable
with `--json`). It reads no configuration. `--help` and `--version` exit
0, and print text also when `--json` is given: help wins over `--json`.

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
| 3 | A tier failed (command error, permission error, daemon gone) or was excluded as misconfigured, below `--min-free`, or the final sample could not be taken. Something may have been deleted; the report says what. |
| 1 | Every eligible candidate was processed without errors, still below `--min-free`. The report lists what was skipped and why. |

The mapping is explicit in code and covered by tests. `ensure` exits with
no code but those in the table, or 101 when it panics (a bug, see task-88),
so a wrapper can use any other code below 126 for its own errors without a
collision (the shell reports 126 and 127 for a command it cannot run, and
128+N for one a signal ended; the example hook,
[ci-pre-job.sh](ci-pre-job.sh), uses 4 for a missing report). In a
reclaiming run, available space decides 0, 75, 124, 3 and 1 by a fresh
sample: a sample that could not be taken is a run error (3); 75 and 124
require a fresh sample below `--min-free`. The first sample, of `PATH`'s
filesystem before anything is done, is the exception: when it cannot be
taken nothing was measured or done, and it is exit 2, like an unusable
`PATH`. A dry run keeps its exits (0, 2, 124, 130/143) and reports a final
sample it could not take as `avail_after_bytes: null` with an `errors`
entry. The JSON `status` of each: 2 `usage_error` (also for a refusal to
run as root, a missing host lock file and, in development builds, the
pre-release sandbox gate, [safety.md](safety.md#pre-release-sandbox-gate)),
130 / 143 `interrupted`, 75 `lock_timeout`, 0 `met` (or `dry_run`), 124
`deadline`, 3 `error`, 1 `exhausted`.

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
  "plan":     [{"group": "", "tier": "", "profile": "", "candidate": "", "eligible": true, "reason": null, "estimated_bytes": 0, "operation": "", "checkouts": "refuse | allow"}],
  "actions":  [{"tier": "", "profile": "", "candidate": "", "avail_delta_bytes": 0, "duration_ms": 0, "count": 1}],
  "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | not_idle | changed | empty | kept | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_store | unsupported_backend | missing | unsafe_path | contains_ipc | contains_mount | contains_special | contains_checkout | protected | excluded | not_owner | has_not | future_time | stale | too_many_entries | unreadable"}],
  "held_by_deleted_files": [{"pid": 0, "command": "", "bytes": 0}],
  "overruns": [{"operation": "", "path": "", "over_ms": 0}],
  "errors":   [{"tier": "", "profile": "", "candidate": "", "message": ""}]
}
```

- `seed` is `null` when no group uses `random`.
- `index` says where the order came from (the live index of a running
  `watch`, a snapshot, or a synchronous tally), its age, how many candidates
  were not yet tallied, and whether the index covers everything it should
  (`partial` when it hit its memory budget). Builds without an index
  (before task-29) emit `null`.
- Paths are JSON strings; bytes of a path that are not UTF-8 are shown as
  U+FFFD (the text is for reading; diskgc itself compares paths byte for
  byte).
- `plan` is filled only by `--dry-run`; `reason` uses the skip-reason enum; a
  bucket is one plan entry with a member `count`; `estimated_bytes` is
  `null` for Docker and Nix without a reported size, for candidates not
  yet tallied, and for every skipped candidate (its checks stopped before
  its walk was complete, so a size would be a partial count); `checkouts`
  is the source profile's setting the candidate was judged under (`allow`:
  it may hold a version-control checkout, see
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate));
  `operation` is human-readable text (`cargo lock protocol`, a command's
  argv), not an enum: it is for reading, not for matching.
- `avail_delta_bytes` is signed: the observed net change in available space
  around an action, never "bytes freed by this action" (see
  [architecture.md](architecture.md#measuring-space)); `null` when a
  sample around the action failed. Anything diskgc did not measure is
  `null`, never 0: `avail_after_bytes` when the final sample failed (and
  then `target_met` too, and the run never counts as met). A candidate
  deleted part-way (its deletion stopped, `changed`, `in_use` or an error)
  is an action too, and also in `skipped` or `errors` with why it
  stopped.
- `count` is 1 for a single candidate. Selections that match very many
  candidates aggregate: one `skipped` entry per profile and reason with
  `count` and up to 20 `examples` (`candidate` is then empty), and after
  1,000 `actions` a single summary entry with `count`.
- `held_by_deleted_files` lists open-but-deleted files on the requested
  filesystem (visible processes only); always present, possibly empty. Their
  space returns only when the process closes them; diskgc never kills
  processes. It comes from the run's last process scan (one is made at the
  end if the run made none); a run that reclaimed nothing because the goal
  was met, or that ended before the reclaim (a usage error, a lock
  timeout), scans nothing and leaves it empty.
- `overruns` lists operations that blocked past the deadline (see
  [safety.md](safety.md#deadlines)); always present, usually empty.
- On a usage error, fields that could not be determined (`mount`, `path`,
  sizes, `target_met`) are `null`; `errors` says why. In `errors`, `tier`,
  `profile` and `candidate` are empty where they do not apply (a usage
  error, an error of the whole run).

## Signals

On SIGINT/SIGTERM: do not start another candidate; send SIGTERM to the
running child's process group, SIGKILL after 10 s; release locks; print the
report; exit 130/143 (`watch`: 0). Lock inheritance by children is
specified in [safety.md](safety.md#host-lock).

## Sizes and durations

- Sizes are an integer and a unit, case as written: `K M G T` and `KiB MiB
  GiB TiB` are powers of 1024; `KB MB GB TB` are powers of 1000. A bare
  number is an error, and so is any other unit (`g`, `Gb`, `B`, `1.5G`).
- `N%` is N percent of the filesystem size (`f_blocks × f_frsize`), rounded
  down; more than 100% is a usage error.
- Durations: integer plus `s m h d w`. No months; `m` is always minutes.
  A bare number is an error, except a bare `0` where a flag defines `0`
  (`--timeout 0`, `--tally-budget 0`; `0s` means the same).
- `--target-free` below `--min-free`, or either above the filesystem size,
  is a usage error (exit 2).

## Configuration file

Lookup: if `--config` or `$DISKGC_CONFIG` is given, that file and only that
file is used; missing or unreadable is exit 2, never a fallback, so a typo
cannot activate some other policy. An empty or relative value is exit 2
too (`DISKGC_CONFIG=$UNSET` must not fall back). Only when neither is given:
`$XDG_CONFIG_HOME/diskgc/config.yaml` (default `~/.config/...`; a relative
`$XDG_CONFIG_HOME` is ignored, as the XDG Base Directory specification
says), then `/etc/diskgc/config.yaml`; a file that exists but cannot be
read is exit 2, not a reason to try the next. None found: exit 2, listing
the paths tried. The configuration file may be a symlink (packaged
configurations often are); only its content is validated. The
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
own means: running containers, GC roots). Like `!profile:`, a protected
profile covers everything its `find` finds and ignores its `where`. The
example config protects `claude-code-tmp`.

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
  description: pytest test dirs in /tmp, also inside nix-shell and mktemp dirs
  find:                              # makes this a *source* profile
    roots: [/tmp, /var/tmp]
    paths: ["pytest-of-*/pytest-*/*", "nix-shell.*/pytest-of-*/pytest-*/*", "tmp.*/pytest-of-*/pytest-*/*"]
    type: dir
  checkouts: allow                   # default refuse (Built-in profile library)
  action: delete
  examples:                          # checked by `diskgc profiles test` and CI
    match:    [/tmp/pytest-of-mark/pytest-3/test_x0, /tmp/nix-shell.AbC123/pytest-of-mark/pytest-0/test_y0]
    no_match: [/tmp/pytest-of-mark/pytest-3, /home/mark/pytest-of-mark/pytest-3/test_x0]
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

Fields (an unknown key anywhere in a profile is a load-time error):

| Field | On | Meaning |
|---|---|---|
| `name` | every profile | A `NAME` (see Selection grammar), unique among built-in and user profiles. |
| `category`, `description` | every profile; required on built-ins | Free text, shown by `profiles list`. |
| `kind` | sources | Default `fs`. |
| `find.roots` | sources | A list of absolute directories, or `required`. |
| `find.paths` | kind `fs`, required | Patterns relative to the roots, segments separated by `/`; no segment is empty, `.` or `..`, or contains `**`. Each segment matches one name, compared as bytes: `*` matches any run of bytes (none included, and a leading `.`: `*` matches `.X11-unix`), every other byte itself; `?`, `[`, `]` and `\` are reserved (a load-time error), so a later glob syntax cannot change what an existing pattern matches. `{uid}` is replaced first. |
| `find.type` | kind `fs`, required | `dir` (a directory), `file` (a regular file) or `any` (a directory or a regular file). Symlinks, sockets, FIFOs and devices are never candidates, whatever the type: a symlink candidate could only be refused (`unsafe_path`), and `tmp-entry` would report every symlink in `/tmp`. |
| `where` | filters (at least one predicate); optional on sources, `has` / `has_not` only | See Predicates. A source's own predicates narrow it in every term; `!profile:` and `protect` ignore them. A source has no `idle_for`: when something is old enough is the tier's choice, written with a `filter:` (Key decisions: what vs when). |
| `action` | kind `fs`, required | See Actions. The code-backed kinds fix their action and take no `action`. |
| `examples` | kind `fs`, required | `{match: [...], no_match: [...]}`, absolute paths, at least one `match`; built-in and user profiles alike (User profiles). |
| `checkouts` | sources, any kind | `refuse` (the default, also when absent) or `allow`; any other value is a load-time error. Whether a candidate may hold a version-control checkout (`.git`, `.hg`, `.svn`) or lie inside a checkout's metadata; refused ones are `contains_checkout`. `allow` can delete a repository with uncommitted work: see [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate) (No source checkout) before writing it. |
| `verified_with` | built-in sources, required | The tool version the profile was checked against. |
| `override` | user profiles | See User profiles. |

`cargo-target` recognises Cargo target directories by their structure
([safety.md](safety.md#discovery-idleness-eligibility)), so its `find` has
only `roots`, and it takes no `examples`: `{kind: cargo-target, find: {roots:
required}}`.

### Predicates (`where`)

| Predicate | Meaning | Kinds |
|---|---|---|
| `idle_for` | not used or changed for this long, as the kind measures it (below) | `fs`, `cargo-target`, `docker-build-cache` |
| `has` / `has_not` | contains / does not contain a named file (e.g. `CACHEDIR.TAG`): a regular file of that name directly in the candidate directory (not followed; a file candidate contains nothing) | `fs` |

In YAML, `where: {idle_for: 3d}` (a duration, see Sizes and durations) or
`where: {has: CACHEDIR.TAG}` (one file name); each predicate at most once
per profile, all of a profile's predicates must hold. A predicate on a kind
that does not support it is a load-time error. Two `idle_for` in one term
(its `filter:`s) combine to the longest; a `!filter:` is a complement and
does not combine. When the measure cannot tell a candidate's age (the
kernel did not answer a timestamp, or the last use lies in the future),
the candidate passes neither `filter:` nor `!filter:` on `idle_for`: it is
skipped (`unsupported_fs`, `future_time`). Ownership is not a
predicate: it is an always-on check ([safety.md](safety.md)).

| Kind | `idle_for` means |
|---|---|
| `fs` | newest mtime or ctime in the candidate's tree (atime is ignored: under `relatime` any reader, such as a nightly scan, refreshes it) |
| `cargo-target` | last build: newest mtime or ctime of the profile dirs and everything in them, on every Cargo layout ([safety.md](safety.md#discovery-idleness-eligibility)) |
| `docker-build-cache` | last use, as recorded by Docker (`--filter until=`) |

The report always names the measure used.

### Selection grammar

A tier selects candidates with an expression over **qualified** names:
`profile:` for a source profile, `filter:` for a filter profile.

```
expr    := term | union
union   := uterm ( '||' uterm )+
uterm   := profile | '(' nterm ')'
term    := profile | nterm
nterm   := sources ( '&&' factor )+
sources := profile | '(' profile ( '||' profile )+ ')'
factor  := filter | '!' filter | '!' profile
profile := 'profile:' NAME
filter  := 'filter:' NAME
NAME    := [a-z0-9] [a-z0-9_-]*
```

Whitespace between tokens is ignored; a qualifier and its name are one
token (`profile:a`, not `profile: a`). Every profile name is a `NAME`:
lowercase letters, digits, `-` and `_`, starting with a letter or a digit.
Every parenthesis has a purpose: a group holds two or more profiles and is
followed by `&&`, and a parenthesised union member contains `&&`, so
parentheses are never optional.

Examples, with the normalised form that `check` prints (groups
distributed, the terms of a union that have factors in parentheses; it
parses back to the same expression):

| Expression | Normalised | Selects |
|---|---|---|
| `profile:pytest-tmp && filter:idle-10d` | `profile:pytest-tmp && filter:idle-10d` | pytest temp dirs idle for 10 days |
| `(profile:pytest-tmp \|\| profile:nix-shell-rc) && filter:idle-3d` | `(profile:pytest-tmp && filter:idle-3d) \|\| (profile:nix-shell-rc && filter:idle-3d)` | either, idle for 3 days |
| `profile:tmp-entry && filter:idle-2d && !filter:idle-30d` | `profile:tmp-entry && filter:idle-2d && !filter:idle-30d` | idle for 2 to 30 days |
| `profile:tmp-entry && filter:idle-2d && !profile:claude-code-tmp` | `profile:tmp-entry && filter:idle-2d && !profile:claude-code-tmp` | idle for 2 days, except what overlaps Claude Code's session dirs |
| `profile:pytest-tmp \|\| profile:nix-shell-rc` | `profile:pytest-tmp \|\| profile:nix-shell-rc` | either, at any age |
| `(profile:pytest-tmp && filter:idle-3d) \|\| profile:nix-shell-rc` | `(profile:pytest-tmp && filter:idle-3d) \|\| profile:nix-shell-rc` | pytest dirs idle for 3 days, nix-shell rc dirs at any age |
| `((profile:pytest-tmp \|\| profile:nix-shell-rc) && filter:idle-3d) \|\| (profile:tmp-entry && filter:idle-30d)` | `(profile:pytest-tmp && filter:idle-3d) \|\| (profile:nix-shell-rc && filter:idle-3d) \|\| (profile:tmp-entry && filter:idle-30d)` | the first two idle for 3 days, any `/tmp` entry idle for 30 |

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
  nest beyond that: the only nesting is a profile group at the start of a
  parenthesised union member (the last example above). Filters cannot
  appear inside a profile group, and `!` applies to one name:
  `!(profile:a || profile:b)` is written `!profile:a && !profile:b`.
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
  `nix-store`) or `profile:NAME`, optionally followed by `&&` filters whose
  predicates are all `idle_for` (`docker-build-cache`), as the whole expression (one term, never in a
  union): they have no per-candidate enumeration.
- Filters are evaluated from the index where it already knows the answer,
  and always re-evaluated at deletion time. A candidate that fails a filter
  is reported as skipped with that filter's reason (`not_idle`, `has_not`,
  or why its age is unknown: `unsupported_fs`, `future_time`).

Errors. A rejected expression is a load-time error (exit 2). The report
starts with `invalid selection`, the expression as written and, for a
syntax error, the column; then the message. The rules that depend on what a name is
report every broken rule, separated by `;`. Where a message suggests a
rewrite, the rewrite fixes that error without breaking another rule (if the
expression has other errors, they still need fixing); a suggestion that would
itself be rejected is left out (`filter:nix-garbage` below: `!profile:nix-garbage`
is rejected too). The messages, with the built-in profiles plus
`a`, `b` (kind `fs`, action `delete`) and `f` (`idle_for`):

| Expression | Message |
|---|---|
| `profile:a \|\| profile:b && filter:f` | `\|\|` and `&&` cannot be mixed without parentheses; write `(profile:a \|\| profile:b) && filter:f` or `profile:a \|\| (profile:b && filter:f)` |
| `profile:a && filter:f \|\| profile:b` | `\|\|` and `&&` cannot be mixed without parentheses; write `(profile:a && filter:f) \|\| profile:b` |
| `(profile:a \|\| profile:b) \|\| profile:tmp-entry` | a profile group must be followed by `&&`: parentheses group profiles only to share factors; write `profile:a \|\| profile:b \|\| profile:tmp-entry` |
| `((profile:a \|\| profile:b)) \|\| profile:tmp-entry` | a profile group must be followed by `&&`: parentheses group profiles only to share factors; write `profile:a \|\| profile:b \|\| profile:tmp-entry` |
| `(profile:a) && filter:f` | parentheses around one profile have no purpose: write `profile:a && filter:f` |
| `profile:tmp-entry && !(profile:a \|\| profile:b)` | `!` applies to one name, not a group: write `!profile:a && !profile:b` |
| `profile:tmp-entry && profile:a` | one source per term: `profile:a` cannot follow `&&`; to exclude its matches write `!profile:a`, to add them write a union with `\|\|` |
| `!profile:a && filter:f` | `!` only narrows: a term starts with a `profile:`, and `!` follows `&&` (`profile:… && !profile:…`) |
| `profile:tmp-entry && tag:old` | unknown qualifier `tag:`: names are written `profile:NAME` or `filter:NAME` |
| `profile:Tmp-entry` | `Tmp-entry` is not a valid name: a name is lowercase letters, digits, `-` and `_`, starting with a letter or a digit |
| `filter:pytest-tmp` | `filter:pytest-tmp`: `pytest-tmp` is a source profile of kind fs, not a filter; write `profile:pytest-tmp` |
| `profile:tmp-entry && !filter:claude-code-tmp` | `!filter:claude-code-tmp`: `claude-code-tmp` is a source profile of kind fs, not a filter; write `!profile:claude-code-tmp` |
| `profile:a && filter:nix-garbage` | `filter:nix-garbage`: `nix-garbage` is a source profile of kind nix-store, not a filter |
| `profile:tmp-entry && filter:nope` | `filter:nope`: no profile or filter is named `nope` |
| `profile:pytest-tmp \|\| profile:cargo-target` | `profile:pytest-tmp` is of kind fs but `profile:cargo-target` is of kind cargo-target: the sources of one expression share one kind; select them in separate tiers |
| `profile:pytest-tmp \|\| profile:pip-cache` | `profile:pytest-tmp` removes with `delete` but `profile:pip-cache` with `command ["pip", "--cache-dir", "{path}", "cache", "purge"], cwd: candidate`: the sources of one expression share one action; select them in separate tiers |
| `profile:cargo-target && filter:has-cachedir-tag` | `filter:has-cachedir-tag` uses `has`, which kind cargo-target does not support (`has` works on: fs) |
| `profile:nix-garbage \|\| profile:nix-garbage` | kind nix-store acts on everything it finds at once (no per-candidate enumeration), so its expression has one term, not a union of 2; select each in its own tier |
| `profile:nix-garbage && filter:idle-7d` | kind nix-store takes only `profile:NAME`, so `filter:idle-7d` cannot narrow it: it has no per-candidate enumeration |
| `profile:tmp-entry && !profile:docker-dangling` | `!profile:docker-dangling`: a negated profile must be of kind fs or cargo-target, and `docker-dangling` is of kind docker-images |

### Actions

| Action | Use |
|---|---|
| `delete` | diskgc's own deletion ([safety.md](safety.md#deletion)). Option `make_writable: true` adds owner write permission to directories during the delete walk, after the last in-use recheck, for read-only trees such as Go's module cache or files copied from the Nix store. Opt-in per profile, since a read-only directory can be a deliberate protection. |
| `command` | an argv list run per candidate, e.g. `[env, "GOCACHE={path}", go, clean, -cache]`. Trusted, arbitrary command execution; never run as root. See [safety.md](safety.md#command-actions). |
| kind actions | Cargo lock protocol, Docker prune, Nix GC: fixed by the kind ([safety.md](safety.md#cargo-target)). |

In YAML:

```yaml
action: delete
action: {delete: {make_writable: true}}
action: {command: [env, "GOCACHE={path}", go, clean, -cache], cwd: parent}
```

Command action fields: `command` (argv; `{path}` is replaced by the
candidate path as one argument or inside one, never through a shell; use
`[sh, -c, "..."]` explicitly if a shell is wanted) and `cwd` (`candidate` or
`parent`; the default is `candidate` for `type: dir`, `parent` for `type:
file`, and for `type: any` decided per candidate the same way; `cwd:
candidate` with `type: file` or `type: any` is a load-time error). The
program (`command[0]`) is a name or path without `{path}`: it is resolved
and shown before anything runs ([safety.md](safety.md#command-actions)),
so it is never the candidate. The tier's `timeout`
bounds it (Tier fields). Two sources have the same action only if the
action and every option are equal; the "one action" error shows a command
action as its quoted argv and `cwd`. Environment passed:
`DISKGC_PATH`, `DISKGC_NEED_BYTES`, `DISKGC_MOUNT`, `DISKGC_PROFILE`. Most
cleaners pick their cache from their own config or environment, not from
the working directory, so the command must name the location explicitly
(`pip --cache-dir {path} cache purge`, `env GOCACHE={path} go clean
-cache`). A non-zero exit is an error in the report (exit 3 if the goal is
not met).

### Built-in profile library

Shipped inside the binary as YAML data, listed with `diskgc profiles list`
and at the end of `diskgc --help`.
A built-in profile ships only with `examples`, model tests, structural
fixtures where it relies on markers, and a `verified_with` line naming the
tool version it was checked against on a real system. Every row below is a
v1 *target*; until verified it is listed as planned, not shipped.

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | pytest's test dirs `pytest-of-*/pytest-*/*` in `/tmp`, `/var/tmp`, and one level down (`nix-shell.*/pytest-of-*/pytest-*/*`, `tmp.*/...`); one candidate per test dir (below the list); `checkouts: allow`: pytest deletes these dirs itself (below the list), so a repository a test made there is a fixture | `delete` |
| test | `pytest-cache` | `.pytest_cache` (roots required) | `delete` |
| nix | `nix-shell-rc` | `/tmp/nix-shell-*-*`, `/tmp/nix-develop-*-*` (rc dirs left by every shell start) | `delete` |
| nix | `nix-shell-tmpdir` | `/tmp/nix-shell.*` (a shell's `TMPDIR`) | `delete` |
| nix | `nix-garbage` | dead store paths | Nix GC |
| rust | `cargo-target` | Cargo `target/` dirs (roots required); `checkouts: allow`: build scripts clone sources into a target (a vendored dependency in `build/<pkg>/out/`), which is Cargo's own output, reclaimed by its lock protocol | lock protocol |
| containers | `docker-build-cache` | BuildKit cache of the default builder | prune |
| containers | `docker-dangling` | untagged images | prune |
| containers | `docker-unreferenced` | images no container uses (removes tagged images too) | prune |
| ai agents | `claude-code-tmp` | `/tmp/claude-{uid}/-*/*` session dirs (project dirs are named after the project's path, so they start with `-`; `/tmp/claude-{uid}` is also the `TMPDIR` of the tools Claude Code runs, so `*/*` would match inside their temp dirs too) | `delete` |
| browsers | `chromium-tmp` | `.org.chromium.Chromium.*` (files) and `org.chromium.Chromium.*` (directories: URL fetcher and unpacker scratch of Chromium and Brave) in `/tmp` | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required) | `command` (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `jest-cache`, `playwright-tmp`, `node-compile-cache` | jest's cache dir; `playwright*` in `/tmp` and nix-shell temp dirs; `node-compile-cache` in `/tmp` and nix-shell temp dirs | `delete` |
| c / c++ | `cmake-build` | directories containing `CMakeCache.txt` and no `CMakeLists.txt` (an in-source build is a source tree; one holding a checkout is refused anyway, see [`contains_checkout`](safety.md#the-eligibility-walk-mandatory-per-candidate); roots required) | `delete` |
| jvm | `gradle-caches`, `maven-repo` | `~/.gradle/caches`, `~/.m2/repository` (roots required) | `delete` |
| embedded | `zephyr-tmp` | `/tmp/zephyr-*` scratch and `zephyr-*-run.*` dirs | `delete` |
| embedded | `west-build` | Zephyr `build/` dirs (`CMakeCache.txt` plus `zephyr/`, no `CMakeLists.txt`; roots required) | `delete` |
| embedded | `renode-tmp` | `renode-*` in `/tmp` and nix-shell temp dirs | `delete` |
| embedded | `platformio-cache` | PlatformIO's `.cache` (roots required) | `delete` |
| generic | `tmp-entry` | any top-level entry of `/tmp`, `/var/tmp` | `delete` |
| generic | `mktemp-dir` | `/tmp/tmp.*` | `delete` |
| filters | `idle-1h`, `idle-1d`, `idle-2d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d`, `has-cachedir-tag` | (no `find`) | none |

- `pytest-tmp` takes one test directory per candidate
  (`pytest-of-<user>/pytest-<N>/<test>`: what pytest's `tmp_path` made for
  one test), not a session or the whole `pytest-of-<user>`: a test dir
  that cannot go (one in use, `in_use`) keeps only itself. It says
  `checkouts: allow` (Mark, 2026-10-19): a test dir holding a `.git`
  (a test ran `git init`) is eligible like any other. pytest treats these
  dirs as disposable itself: it keeps only the last
  `tmp_path_retention_count` sessions (default 3) per base temp dir and
  deletes the older ones; they pile up because every nix shell has a
  `TMPDIR` of its own, so each holds its own few sessions that no later
  pytest run cleans. The per-test-dir granularity was chosen while the
  guard still applied here; it stays for now and can be revisited (its
  candidate count is task-70's cost question). Only
  directories are candidates (`type: dir`): pytest's `*current` symlinks
  (`pytest-current` next to the sessions, `<test>current` next to the
  test dirs) and plain files directly in a session dir never are. The
  session dirs and `pytest-of-<user>` itself are left alone, also once
  empty; `tmp-entry`, which takes only top-level entries of `/tmp` and
  `/var/tmp`, may take a whole `/tmp/pytest-of-<user>` (or the
  `nix-shell.*` or `tmp.*` directory around one), with whatever is still
  in it, never a session dir on its own. pytest's `garbage-*` directories
  (an interrupted cleanup of its own) are not matched. Limit: under
  pytest-xdist the test dirs are one level deeper
  (`pytest-<N>/popen-gw<K>/<test>`), so a candidate is a whole worker's
  directory, and one test dir there in use keeps all of that worker's.
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
- A user profile of kind `fs` carries `examples` with at least one
  `match`, like a built-in: the examples are what `profiles test` checks.
- `diskgc profiles test` runs every profile's `examples` against an
  in-memory model, so a profile is checked before it touches a disk.
- `check` warns about a user profile whose last path segment is only
  wildcards outside `/tmp` and `/var/tmp` (e.g. `roots: [/home/mark]`,
  `paths: ["*"]`), and about every user profile that says `checkouts:
  allow` (its candidates may be repositories holding uncommitted work,
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate)).
- **Profiles are meant to be written by an AI and checked by the tool**,
  because globs are hard for humans to read and concrete paths are not:
  `diskgc survey --json` shows what no profile matches yet; an AI agent
  drafts a profile with `examples` taken from real paths it saw;
  `diskgc profiles test` checks the examples; `diskgc survey --list
  --select profile:<new>` lists every path the profile would match on this
  machine. The human reviews that list and decides. The examples only show
  that the patterns match what the author intended; whether those files are
  disposable is the human's judgement from the list.
