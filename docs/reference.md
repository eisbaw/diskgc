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
diskgc check  [--config FILE]
diskgc profiles list [--json] [--config FILE]
diskgc profiles show NAME [--json] [--config FILE]
diskgc profiles test [--json] [--config FILE]
diskgc --help | --version
```

`watch` and `survey` are planned commands (tasks 32 and 38). This build
returns exit 2 for either; their intended interfaces are described below
for design review, not as available CLI behavior.

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

The current configured `ensure` implementation runs groups in their written
order, shuffles tiers inside `ranking: random` groups using the report's
seed, and discovers each tier before leaving it. Within a tier, `best-fit`
re-picks after each observed space sample. Its dry-run plan uses estimates
from the initial sample and cannot predict the exact reclaim order. Before
the first deletion, it checks
all configured roots, command executables and Docker bindings under the
host lock. A tier with an unsafe root or failed environment check is
excluded and reported while the other tiers remain eligible. Protected and
negated source matches are rechecked during reclaim. `ipc: live-only` remains
refused by `ensure` until its runtime check is implemented.

The `--roots` value is where the profile looks; the final `PATH` is the
directory whose filesystem must have the space. A complete pre-job hook
built on this call, branching on the exit code and keeping the `--json`
report, is [ci-pre-job.sh](ci-pre-job.sh); the CI operator's journey test
runs it as written.
An agent-oriented [pre-job hook](agent-pre-job.py) uses the JSON report to
decide whether to start, defer, abort or retry after a contended host lock.

For detached targets placed under `/tmp` with `CARGO_TARGET_DIR`, an
explicit tier inside a group's `tiers` can select the same Cargo profile:

```yaml
- name: cargo-tmp-old
  select: profile:cargo-target && filter:idle-7d
  roots: [/tmp]
```

Choose this tier only when `/tmp` is on the filesystem the policy intends
to free. A detached target remains blocked while any visible Cargo
process runs, even if that Cargo works in an unrelated directory; see
[safety.md](safety.md#cargo-target).

### `ensure`

- `PATH` is required: the directory the next job will write to, as an
  absolute path. Its filesystem is the one diskgc measures and frees. There
  is no default to the current directory, and a relative `PATH` is a usage
  error, so a hook launched from `/tmp` cannot silently free the wrong
  disk. It is reached like every configured path (no symlink on the way,
  [safety.md](safety.md#path-traversal)). The resolved mount point is
  printed in every report.
- `--min-free` (required): if available space is at least this, do nothing
  and exit 0, without taking the lock. `ensure` still checks that
  `/run/diskgc/lock` is a safely reachable, readable regular file. If it
  is missing or unusable, stderr warns with the provisioning detail (and
  the tmpfiles lines when missing); exit 0 and the JSON `met` report are
  unchanged. A later run needing to reclaim exits 2 if the lock is still
  unusable. This readiness check does not take a `flock`.
- `--target-free` (default: same as `--min-free`): once triggered, keep
  reclaiming until this much is available. Setting it higher is the
  hysteresis: a fleet of CI jobs then does one larger cleanup instead of a
  small one per job. The exit code depends only on `--min-free`; missing the
  target is reported (`target_met: false`) but is not a failure.
- `--timeout` (default `10m`, `0` = none): a **cooperative deadline** starting
  when the executor begins, after command-line and configuration loading,
  including waiting for another reclaim. At the deadline
  diskgc starts no new work and terminates a running child (SIGTERM, SIGKILL
  after 10 s). diskgc checks the deadline between operations; a single
  filesystem operation that blocks (a hung network mount, a slow disk)
  cannot be interrupted and can overrun it. See [safety.md](safety.md#deadlines).
- `--tally-budget` (default: the smaller of 30 s and a quarter of
  `--timeout`; `0` = no limit) and `--no-index`: see
  [architecture.md](architecture.md#the-tally). Without an index (before
  task-29), `--no-index` changes nothing, and `--tally-budget` bounds the
  tally that orders each active tier under the host lock: candidates it
  leaves untallied are taken after the tallied ones, in the order
  discovery found them, each judged in full under its own locks. A
  candidate the tally refuses (e.g. kept, another user's, too many entries) is
  skipped with that reason for the run and never locked; a refusal that
  stops being true later (a `.diskgc-keep` removed after the tally) is a
  missed reclaim until the next run, never an unsafe delete. When a tallied
  Cargo target's measured last build fails the tier's idle filter, it is
  skipped (`not_idle`, `too_idle`, or an unknown-age reason) before any of
  its Cargo locks are attempted. A target left untallied is judged under
  its locks; one that passes the tally's age check is judged there again
  before deletion. The dry run
  judges every candidate fully and is not bounded by it. The result's
  `tally_cut_short` is true when the budget leaves at least one discovered
  candidate untallied during reclaim.
- `--dry-run`: print, in order, every candidate diskgc would consider,
  whether it is eligible or the skip reason, its estimated size and the
  operation it would run (JSON: the `plan` array); delete nothing. The
  eligible candidates come first, in the order a reclaim would take them
  when its tally is not cut short (`--tally-budget 0`), then the skipped
  ones; a reclaim stops at the goal, the plan does not.
  Takes no lock (so it cannot tell `locked`: whether a target's locks or
  an `fs` candidate's `flock` are free is only tried by the reclaim) and
  always prints, even if the goal is already met. An `fs` tier's plan is
  the reclaim's discovery and judgement
  ([safety.md](safety.md#deletion)) without the lock and the delete. It deletes nothing, so the deletion's freshness bound
  does not apply: both `cargo-target` and `fs` plans reuse a process scan
  for up to a minute (half a second for a scan that blocks every candidate).
  With a `random` group, the order is for the printed seed
  only. Sizes are estimates; Docker and Nix tiers can only say what they
  would run. A complete plan exits 0 with JSON `status: dry_run`; a host or
  planning failure exits 3 with `status: error` and the available partial
  plan and `errors` (2 on usage errors, 124 at the deadline, 130/143 when
  interrupted). If one configured tier exhausts its own timeout, later
  tiers are still planned, but the incomplete grouped plan exits 3; the
  overall `--timeout` still exits 124.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors, when stdout accepts the write. Any argument
  `--json` (or `--json=...`, a usage error) asks for it, wherever it stands,
  also where the parser takes it as another flag's value (`--config --json`).
  `--help` (or `-h`) wins over it: the help is printed as text, exit 0.

### `watch`

Planned; this build returns exit 2. Intended syntax:

```
diskgc watch --min-free SIZE --target-free SIZE [--interval DUR]
             [--select EXPR] [--roots DIR]... [--config FILE] [--allow-root] PATH
diskgc watch --tally-only [--config FILE] PATH
```

- One process per filesystem, under any supervisor (e.g. one instance of a
  systemd template unit per watched path; systemd is not required).
- `--target-free` is required here: a watcher without two thresholds would
  reclaim a sliver at every poll. The error message says so.
- Every `--interval` (default `10s`): one `statvfs`. At or above
  `--min-free`: no deletions; only the background tally keeps the index
  fresh.
- Below `--min-free`: run the reclaim walk with goal `--target-free`. No
  overall deadline; each tier's work is bounded by its `timeout`. The wait
  for the host lock stays interruptible by SIGTERM.
- `--tally-only`: maintain the index and its snapshot, never reclaim (for
  CI runners, so that `ensure` in the pre-job hook finds a warm index).
  `--min-free` / `--target-free` are not used in this mode.
- **Back-off:** if a reclaim ends below `--min-free` because everything left
  is in use (`exhausted`) or with misconfiguration errors, the next one
  starts only after available space has dropped by a further
  `max(1 GiB, 1% of the filesystem)` or 15 minutes have passed. Backend
  errors and `backend_unavailable` skips (a Docker or Nix daemon
  unreachable, [Backend skips and errors](#backend-skips-and-errors))
  shorten the wait to 1 minute,
  since backends often recover quickly. One log line per back-off. These
  values are fixed in code.
- On SIGTERM/SIGINT: stop as described under Signals, exit 0.

### `survey`

Planned; this build returns exit 2. Intended syntax:

```
diskgc survey [--select EXPR] [--roots DIR]... [--list] [--json] [--config FILE] PATH...
```

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
found as described under Configuration file (its groups are validated but
not used by this command).
Without a configuration file (none found by the search), the built-ins
alone, and no error. A given file (`--config`, `$DISKGC_CONFIG`) that
is missing, or any file that does not load, is exit 2 with the same
message `ensure` gives. The resolved configuration path is logged on
stderr (`diskgc: config PATH`).

- `list`: every profile, one row each, in one aligned table, the values
  of `ORIGIN`, `ROLE`, `KIND`, `ACTION` and `CHECKOUTS` spelled as in the
  JSON below so the table can be grepped: `NAME`,
  `ORIGIN` (`built_in`, `user`, or `user_override`), `ROLE` (`source`: selected as
  `profile:NAME`; `filter`: `filter:NAME`), `CATEGORY`, `KIND`, `ACTION`
  (`delete`, `delete make_writable`, `command PROGRAM`, where `PROGRAM` is
  the one `env` runs for an `[env, NAME=value, ...]` command, or
  `cargo_lock_protocol`), `ROOTS` (the fixed roots, comma-separated, or
  `required`, which the JSON says as `find.roots_required: true`),
  `CHECKOUTS` (`refuse` or `allow`, so every profile that
  says `checkouts: allow` is marked), `DESCRIPTION`; `-` where a column
  does not apply (a filter has no kind, action, roots or checkouts).
  Sources come first, by name, then filters, the shortest `idle_for`
  first. An `override: true` replacement has ORIGIN `user_override`.
- `show NAME`: the profile as YAML in the schema of Profiles, as diskgc
  understood it: a comment line saying where it comes from, then the
  profile as a list item with every default spelled out (`kind`,
  `checkouts`, a command's resolved `cwd`, empty `no_match`). A kind's
  fixed action (`cargo-target`'s lock protocol) is a comment, since the
  schema takes no `action` there. Pasted under a config's `profiles:`,
  it loads as the same profile (a built-in's name needs `override: true`
  there). An unknown `NAME` is exit 2 with JSON `cause: usage`, naming the close names
  (those containing it or contained in it, then the fewest edits away,
  at most two) or, without any, pointing to `profiles list`. Fuzzy
  suggestions are omitted for names over 256 characters.
- `test`: run every source profile's path `examples` against an in-memory
  model of path, entry type and direct marker files. The abstract uid is
  1000; `{uid}` in an example is expanded to it. A string example has the
  profile's declared type (directory for `any`) and its required `has`
  marker, so type and marker changes need separate model/golden fixtures.
  Structural kinds and filters have no path examples. Human output reports
  every failure; `--json` prints `config`, `status` (`passed | failed`),
  `exit_code`, `profiles` (count), and `failures` (strings). Exit 0 when
  all examples pass, 1 when any fails.
- Exit codes of `list`, `show` and `test`: 0 when the list or profile was
  printed; 2 for a usage error, a configuration that is missing (when
  given) or does not load, or an unknown `NAME`. These read-only commands
  need no kernel or process-inspection startup check. `test` also exits 1
  on a failing example. Any failed stdout write exits 3.

`--json` prints exactly one JSON object on stdout, for every exit code when
stdout accepts the write (any argument `--json` asks for it, as for
`ensure`). A profile is the same object in `list` and `show`: every field of
the schema, always
present, `null` where the profile's role or kind has none. Values that are
YAML identifiers keep their YAML spelling (kind names, `delete`,
`command`, `dir`); values only the JSON has are snake_case, like the
report's enum values (`status`, `reason`): `built_in`, `user_override`,
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
    "origin": "built_in | user | user_override",
    "override": false,
    "role": "source | filter",
    "category": "build",
    "description": "Zephyr test runs left in /tmp",
    "kind": "fs",
    "find": {"roots": ["/tmp"], "roots_required": false, "paths": ["zephyr-*-run.*"], "exclude": [], "type": "dir | file | any"},
    "min_idle_for": null,
    "where": {"idle_for": null, "has": null, "has_dir": null, "has_not": null, "app_not_running": null, "lacks_entry": null, "keep_newest": null, "newest_group": null, "protect_links": [], "protect_link_dirs": [], "protect_parent_link": null, "protect_cursor_extensions": null, "source_anchor": null, "liveness_contract": null},
    "action": {"type": "delete | command | cargo_lock_protocol | docker_builder_prune | docker_image_prune | docker_image_prune_all", "make_writable": false, "command": null, "cwd": null},
    "checkouts": "refuse | allow",
    "examples": {"root": null, "match": ["/tmp/zephyr-h5e5-run.jTm9zGlH"], "no_match": ["/tmp/zephyr-hal"]},
    "verified_with": null
  }
}
```

- `role` says how a selection names the profile: `source` as
  `profile:NAME`, `filter` as `filter:NAME`.
- `kind`, `find`, `action`, `checkouts` and `examples` are `null` for a
  filter; `examples` is also `null` for `cargo-target`, which takes none;
  `verified_with` is set on built-in sources only.
- `find.roots` is the list of fixed roots, `null` when
  `find.roots_required` is `true` (`roots: required`: a tier or `--roots`
  supplies them), or `"daemon"` for a Docker backend. `find.paths`,
  `find.exclude` and `find.type` are `null` for `cargo-target`.
- `where` always has its fields, `null` when unused (link lists are empty);
  `idle_for` is a duration (`3d`).
- `min_idle_for` is a source's mandatory minimum age, or `null`; it is
  `null` for filters. A tier's positive idle filters may require longer.
- `action` has every field: `make_writable` (a boolean) for `delete`;
  `command` (the argv) and `cwd` (`candidate`, `parent`, or `null` when
  it is decided per candidate: `type: any` without `cwd`) for `command`;
  neither for `cargo_lock_protocol`. Later kinds add their own `type`.
- On an invalid-input error: `{"status": "usage_error", "exit_code": 2,
  "cause": "parse | config | safety | host | operation | usage", "error": "message"}`,
  the message as on stderr. Host I/O during startup has `status: error`
  and `exit_code: 3`; invalid input and configuration use exit 2.
  `status` is the field
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
profiles it resolves to, exit 0 or 2 (3 if writing stdout fails). It needs
nothing from the deployment machine (no `PATH` lookups, no filesystem access
beyond reading the config), so packaging and configuration tools run it
before deploying (the NixOS
module at build time, also when cross-building). Everything that depends on
the machine is checked at run time; see
[safety.md](safety.md#validation).

The text output starts with `config PATH`, then one line per tier in policy
order: `GROUP.TIER: NORMALIZED_EXPRESSION => [source profiles]`. Advisory
warnings go to stderr. A user profile with `checkouts: allow` warns about
repository contents; a path pattern ending in only wildcards warns when a
fixed or tier-bound root lies outside `/tmp` and `/var/tmp`. Warnings do
not make the configuration invalid. Selection warnings are described under
Selection grammar.

`check` validates an `owners:` list's shape and rejects system UIDs; whether
it is permitted is checked when `ensure` is invoked with or without
`--allow-root`. User-name resolution remains a run-time check.

## Exit codes (`ensure`)

The first matching row wins:

| Code | Meaning |
|---|---|
| 2 | Usage, config or provisioning error. Nothing was deleted. |
| 130 / 143 | Interrupted by SIGINT / SIGTERM. |
| 75 | Deadline reached while waiting for another reclaim, and a fresh sample is still below `--min-free` (EX_TEMPFAIL). Nothing was deleted; retry later. If the other holder freed enough meanwhile, the result is 0. |
| 0 | Final sample of available space ≥ `--min-free`. |
| 124 | Deadline reached during reclaim, below `--min-free`. |
| 3 | A host or planning operation failed (including the first or final space sample), or a tier failed or was excluded as misconfigured below `--min-free`. A dry run may have a partial plan; a reclaim may have deleted something. The report says what. Also returned if writing the result to stdout fails; stderr names the write error, and the report may be absent or partial. |
| 1 | Every eligible candidate was processed without errors, still below `--min-free`. The report lists what was skipped and why. |

The mapping is explicit in code and covered by tests. A failed stdout write
(including a failed flush) takes precedence over the operation's code and
exits 3. This also applies to `--help`, `--version`, `check`, and `profiles`.
`ensure` exits with no code but those in the table; an unexpected panic still
exits 101,
so a wrapper can use any other code below 126 for its own errors without a
collision (the shell reports 126 and 127 for a command it cannot run, and
128+N for one a signal ended; the example hook,
[ci-pre-job.sh](ci-pre-job.sh), uses 4 for a missing report). In a
reclaiming run, available space decides 0, 75, 124, 3 and 1 by a fresh
sample: a sample that could not be taken is a run error (3); 75 and 124
require a fresh sample below `--min-free`. An I/O failure in the first
sample or startup host check is exit 3; an invalid `PATH` or unsupported
kernel remains exit 2. A dry run with a host
or planning error also exits 3, reports its partial plan and errors, and
uses `avail_after_bytes: null` when its final sample failed. The JSON
`status` of each: 2 `usage_error` (also for a refusal to
run as root, a missing host lock file when reclaim is needed and, in
development builds, the pre-release sandbox gate,
[safety.md](safety.md#pre-release-sandbox-gate)),
130 / 143 `interrupted`, 75 `lock_timeout`, 0 `met` (or `dry_run`), 124
`deadline`, 3 `error`, 1 `exhausted`.

## Output

- stdout carries only the result. Human mode: silent when nothing needed
  doing (like `cp`); otherwise a summary. When the goal remains unmet, it
  names each kept candidate and its reason (grouped by reason after five),
  omitting Cargo targets whose previous reclaim left only lock files. The
  summary lists preflight profile refusals separately from kept candidates.
  Human results, including dry runs, report elapsed time and, when present,
  open deleted files by process count, bytes, and the largest three holders.
  `exhausted` is rendered
  as “nothing more could be reclaimed” in human mode; the JSON status remains
  `exhausted`. When two different byte counts would round to the same human
  size, both also show their exact byte counts in the result and in the
  below-minimum progress line. `--json`: one object when
  stdout accepts the write. A failed write can leave no result or a partial
  one; exit 3 and a stderr error identify that failure.
- stderr carries progress logs: one line per action with tier, profile,
  candidate, observed change in available space, duration. Output of child
  commands (Docker, Nix, command actions) goes to stderr, prefixed with the
  tier and profile name.
- `watch` logs to stderr only; a supervisor such as journald collects it.

JSON result (field names are part of the contract):

```json
{
  "status": "met | exhausted | error | usage_error | lock_timeout | deadline | interrupted | dry_run",
  "started_at": "2026-10-21T12:00:00Z", "duration_ms": 0, "version": "0.1.0",
  "mount": "/", "path": "/home/mark/src/app", "seed": null,
  "index": {"source": "live | snapshot | sync", "age_s": 0, "units": 0, "untallied": 0, "coverage": "complete | partial"},
  "tally_cut_short": false,
  "min_free_bytes": 0, "target_free_bytes": 0, "target_met": false,
  "avail_before_bytes": 0, "avail_after_bytes": 0,
  "plan":     [{"group": "", "tier": "", "profile": "", "candidate": "", "eligible": true, "reason": null, "estimated_bytes": 0, "operation": "", "checkouts": "refuse | allow"}],
  "actions":  [{"tier": "", "profile": "", "candidate": "", "avail_delta_bytes": 0, "duration_ms": 0, "idle_age_seconds": null, "count": 1}],
    "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | not_idle | too_idle | changed | empty | kept | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_backend | backend_unavailable | missing | unsafe_path | contains_ipc | contains_mount | contains_special | contains_checkout | protected | excluded | not_owner | has_not | filtered | future_time | stale | too_many_entries | unreadable | newest_version | linked_current | installed_extension | producer_unknown", "detail": null, "process_blockers": [{"pid": 42, "command": null, "evidence": "fd | cwd | root | exe | map | inode_map | environment | unreadable | target_cwd | target_exe | project_cargo | detached_cargo | cargo_manifest_dir | owner_pid"}], "estimated_bytes": null}],
  "held_by_deleted_files": [{"pid": 0, "command": "", "bytes": 0}],
  "overruns": [{"operation": "", "path": "", "over_ms": 0}],
  "errors":   [{"tier": "", "profile": "", "candidate": "", "cause": "parse | config | safety | host | operation | usage", "message": ""}]
}
```

`cause` is a stable fault-class code in error objects and `errors[]`.
Scripts match `cause`, `status` and the process exit code; message text is
for people and may change. `parse` means invalid command or explicit
`--select` syntax and grammar; `config` means invalid or missing
configuration, `safety` means a safety or privilege
precondition refused the run, `host` means a typed operating-system path,
sampling or startup failure, `operation` means a tier planning, action or
backend failure without a more specific typed cause, and `usage` covers
other invalid caller inputs, including `--roots` binding and an unsupported
kernel. An unsafe configured root is a `safety` refusal even when found
after initial root preflight; a host I/O failure while checking that root
is `host`.

- `started_at` is the UTC wall-clock time when an executor run entered the
  library, in RFC 3339 form, or `null` if the host time is outside the
  supported range. `duration_ms` is elapsed monotonic time for that run, or
  `null` if it cannot be represented as an unsigned 64-bit millisecond
  count. Both include early returns from the executor. Errors produced by
  CLI parsing, configuration or startup before the executor begins use
  `null` for both. `version` is always the build's package version.
- `seed` is `null` when no group uses `random`.
- `index` says where the order came from (the live index of a running
  `watch`, a snapshot, or a synchronous tally), its age, how many candidates
  were not yet tallied, and whether the index covers everything it should
  (`partial` when it hit its memory budget). Builds without an index
  (before task-29) emit `null`.
- `tally_cut_short` is true only after the reclaim tally actually stops at
  `--tally-budget` with at least one candidate left untallied. It is false
  for dry runs, runs that stop before tallying, and tallies that complete;
  false does not say that every possible candidate was discovered.
- Paths are JSON strings; bytes of a path that are not UTF-8 are shown as
  U+FFFD (the text is for reading; diskgc itself compares paths byte for
  byte).
- `plan` is filled only by `--dry-run`; `reason` uses the skip-reason enum.
  The current plan has one row per candidate. `estimated_bytes` is
  `null` for Docker and Nix without a reported size, for candidates not
  yet tallied, and for every skipped candidate (its checks stopped before
  its walk was complete, so a size would be a partial count); `checkouts`
  is the source profile's setting the candidate was judged under (`allow`:
  it may hold a version-control checkout, see
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate));
  `operation` is human-readable text (`cargo lock protocol`, a command's
  argv), not an enum: it is for reading, not for matching.
  A source-anchored build candidate whose source was established at
  discovery has `build_evidence` with `marker`, `source` and
  `idle_age_seconds`. The age is measured from the newest change in an
  eligible candidate. It is `null` when a skip stopped the eligibility
  walk before an age was established; the skip reason is retained. Source
  evidence on a skipped row records discovery and may not have survived
  every later recheck. Rows without established source evidence omit it.
- `avail_delta_bytes` is signed: the observed net change in available space
  around an action, never "bytes freed by this action" (see
  [architecture.md](architecture.md#measuring-space)); `null` when a
  sample around the action failed. Anything diskgc did not measure is
  `null`, never 0: `avail_after_bytes` when the final sample failed (and
  then `target_met` too, and the run never counts as met). A candidate
  deleted part-way (its deletion stopped, `changed`, `in_use` or an error)
  is an action too, and also in `skipped` or `errors` with why it
  stopped. `actions` records attempts, not a guarantee that each candidate
  was completely removed. The summary rows described below mean a consumer
  cannot always join an action to its final candidate outcome; use the run's
  status and exit code for the overall result.
- `actions[].idle_age_seconds` is the age of the newest recorded change or
  last Cargo build from a completed tally, measured against the wall clock
  just before that action starts. It is `null` when untallied, unknown, in
  the future, or an overflow summary. It describes the earlier tally, not
  a guaranteed age at deletion.
- `count` is 1 for a single candidate. When more than 20 skipped candidates
  share a tier, profile and reason, they become one `skipped` entry with
  `count` and the first 20 paths in `examples` (`candidate` is then empty).
  Up to 20 in a group remain individual entries. The first 1,000 `actions`
  remain individual entries; any further actions become one summary entry
  with their `count`.
- `skipped[].detail` is an advisory human diagnostic from the refusal, or
  `null` when no diagnostic was captured. Grouped rows always use `null`;
  programs should branch on `reason`, not parse this text.
- `skipped[].process_blockers` is `null` when the refusal cannot be
  attributed to an observed process, including flock contention, an
  unsettled process scan and grouped rows. Otherwise it is a nonempty array
  of `{pid: u32, command: string|null, evidence: string}`. `pid` and
  `command` are scan-time observations; an unreadable process can have a
  known PID but no command. Evidence is one of `fd`, `cwd`, `root`, `exe`,
  `map`, `inode_map`, `environment`, `unreadable`, `target_cwd`, `target_exe`,
  `project_cargo`, `detached_cargo`, `cargo_manifest_dir`. Cargo targets can
  have several blockers. The field reports visible evidence, not an
  exhaustive set of processes using a candidate.
- `skipped[].estimated_bytes` is the candidate's allocated-byte estimate
  only when a complete tally succeeded before a later refusal. Early skips,
  untallied paths and grouped rows use `null`. It is a point-in-time size,
  not a promise of space that could be reclaimed.
- A Cargo target whose profile directories hold only lock files is omitted
  from a live `ensure` result's `skipped` list and human skipped count. It
  remains discoverable and may appear as an ineligible `empty` plan row in
  `--dry-run`.
- `held_by_deleted_files` lists open-but-deleted files on the requested
  filesystem (visible processes only); always present, possibly empty. Their
  space returns only when the process closes them; diskgc never kills
  processes. A file held by several processes contributes its allocated bytes
  once, to the lowest listed pid; the other holders appear with zero bytes.
  On btrfs, mounted subvolumes with the same mountinfo source are included.
  It comes from the run's last process scan (one is made at the
  end if the run made none); a run that reclaimed nothing because the goal
  was met, or that ended before the reclaim (a usage error, a lock
  timeout), scans nothing and leaves it empty.
- `overruns` lists operations that blocked past the deadline (see
  [safety.md](safety.md#deadlines)); always present, usually empty.
- On a usage error, fields that could not be determined (`mount`, `path`,
  sizes, `target_met`) are `null`; `errors` says why. In `errors`, `tier`,
  `profile` and `candidate` are empty where they do not apply (a usage
  error, an error of the whole run).

### Backend skips and errors

Docker and Nix tiers act through a backend that diskgc binds first
([safety.md](safety.md#docker-kinds), [safety.md](safety.md#nix-store)).
One rule holds for every backend kind, at preflight and at the recheck
right before acting:

- **Skip** (the tier is excluded, reported in `skipped` with candidate
  `""`, the other tiers run, and the exit code follows available space:
  0, or 1 when nothing else failed): the host does not offer a supported,
  reachable backend of that kind, identified from a specific signal, never
  by turning failures into skips wholesale.

  | Reason | When |
  |---|---|
  | `missing` | the client is not installed, or the backend's store is absent |
  | `other_fs` | the backend's storage is not on the requested filesystem |
  | `remote_backend` | the caller's client selects a remote endpoint or store |
  | `unsupported_backend` | a backend diskgc has not verified: another local endpoint, context or store, single-user Nix, a client or daemon off the [allowlist](#backend-version-allowlist) or of another variant, an unverified storage layout or server identity, or a Nix daemon whose roots lack the system's generations |
  | `backend_unavailable` | installed and selected, but not serving diskgc's user: the socket absent, connection refused, `EACCES` on the socket, the daemon not answering or refusing the user (Nix `allowed-users`) |
  | `changed` | the backend bound at preflight differs at the recheck before acting (socket inode, versions, daemon identity) |

- **Error** (`errors[]`, exit 3 when below `--min-free`): every other
  failure. A query failing in a way not identified above, unparseable
  output from an allowlisted version, a host I/O error, the sandbox gate,
  and any failure once the deleting child has started
  (nonzero exit, signal). A deadline during the child is 124.
- Why: a host without a working backend of some kind is a configuration
  fact, not a failure of this run; reporting it as an error would make
  every timer-driven run on such a host exit 3 (TASK-113's Podman shim).
  Narrow identification keeps a real failure of a supported backend loud.
- `unsupported_store` is not a reason: store selection is part of binding
  a backend and reports `unsupported_backend` or `remote_backend`.
- Status: the Nix kind is specified this way (TASK-36). Docker's code
  still reports a stopped daemon, `EACCES` and the Podman CLI shim as
  errors; TASK-113 brings it to this rule.

### Backend version allowlist

Backend clients and daemons whose behaviour diskgc relies on are admitted
by exact version, in a constant table in `diskgc-core`: code, not
configuration, so a version is admitted by a reviewed change with its
evidence, never by an operator's config.

| Backend | Admitted | Evidence |
|---|---|---|
| Docker CLI (`docker-build-cache` builder dispatch) | client 29.8.2 | native dind fixture ([safety.md](safety.md#docker-kinds)) |
| Nix daemon (`nix-store`: GC and root semantics) | none yet; TASK-36 admits `2.34.8` when its container scenarios pass | [testing.md](testing.md#nix-store-backend-vectors) |
| Nix client (`nix-store`: command line, `--query --roots` output) | none yet; TASK-36 admits `nix-store (Nix) 2.34.8` with that daemon | same |

- Nix has two lists, because the GC's semantics live in the daemon and
  the client contributes only its command line and output format: the
  daemon version that `nix store info --json` reports, and the client's
  exact `nix-store --version` line. A version is admitted on one list by
  running the scenarios against an end already admitted on the other, so
  every admitted version has been run, though not every pairing; a host
  upgraded without restarting its daemon runs an untested pairing of two
  admitted versions. A host whose Nix is not admitted skips its Nix tier
  (`unsupported_backend`), which a dry run shows; the run's exit code
  follows space as for any skip.
- Admitting a version: pin it in the flake's test image, run every
  scenario of its kind on it, and add the entry and the evidence (command,
  exit code, date) in one commit. Variants (Lix, Determinate Nix) need
  their own verification task first; until then they are
  `unsupported_backend`.
- Removing an entry narrows what diskgc acts on and is noted in the
  release notes.

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
binary ships built-in *profiles* but **no built-in tiers**. The example
config lives at `docs/example.yaml` in the source tree and at
`share/doc/diskgc/examples/example.yaml` in the Nix package; it is not
installed as an active default. Copy it to a config search path or select
it with `--config` after setting deployment roots.
The CI pre-job hook at `docs/ci-pre-job.sh` is a source-tree example; the
package does not install it. Copy it from the source tree if that deployment
pattern is wanted.

The config holds `version`, optional user `profiles`, optional
`profile_policies`, optional `protect`, optional `owners` (only with
`--allow-root`), optional `ipc`, and the tier `groups`.
Thresholds, intervals and timeouts are command-line arguments, so the same
file serves `watch` on a laptop and `ensure` on a runner.

```yaml
version: 1
ipc: block-any                    # block-any (default) | live-only

profiles:                          # optional: your own, or overrides
  - name: zephyr-run
    category: build
    find: {roots: [/tmp], paths: ["zephyr-*-run.*"], type: dir}
    action: delete
    examples: {match: [/tmp/zephyr-h5e5-run.jTm9zGlH], no_match: [/tmp/zephyr-hal]}

profile_policies:                  # optional: checkout guard only; names a built-in source
  - profile: npm-cache
    checkouts: allow

protect:                           # never deleted by any tier (by overlap)
  - claude-code-tmp                # fixed-root profile: bare name
  - {profile: cargo-target, roots: [/home/mark/src/important]}

groups:
  - name: stale                    # nobody will miss these
    ranking: random                # ordered (default) | random
    tiers:
      - name: test-and-temp-cruft
        select: (profile:node-compile-cache || profile:zephyr-run) && filter:idle-3d
      - name: cargo-stale
        select: profile:cargo-target && filter:idle-7d
        roots: [/home/mark/src, /home/mark/topics]
      - name: docker-cache-stale
        select: profile:docker-build-cache && filter:idle-7d

  - name: pressure                 # costs rebuild or download time
    tiers:
      - name: docker-dangling
        select: profile:docker-dangling
      - name: tmp-old
        select: profile:chromium-tmp && filter:idle-2d
      - name: cargo-recent
        select: profile:cargo-target && filter:idle-1h
        roots: [/home/mark/src, /home/mark/topics]
```

Rules:

- **Groups run in list order. Always.** Escalation (stale before pressure)
  is group order; nothing reorders groups.
- Group and tier names use `NAME` (lowercase letters or digits first,
  followed by lowercase letters, digits, `-` or `_`). Names are unique at
  their level, and tier names are unique across all groups.
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
- **`owners:`** is required with `--allow-root` and forbidden otherwise.
  Entries are user names or explicit UID mappings, for example
  `owners: [tester, {uid: 2001}]`. Names resolve at run time; an unknown
  name is exit 2 before planning. UID mappings are accepted only for
  non-system IDs (1000 or greater); numeric-looking names must use the
  explicit mapping. A static musl binary cannot load LDAP/sssd NSS modules,
  so a fleet without local passwd entries or nscd can use trusted numeric
  UIDs. The operator must keep those UIDs stable: the policy applies to
  whoever owns that number at run time. Duplicate entries, including names
  that resolve to the same UID as another entry, are refused. `--allow-root`
  refuses generic command and Docker prune actions: neither can be bounded
  to these owner IDs.
- **`ipc:`** controls sockets and FIFOs inside a candidate. `block-any`
  (default) refuses a candidate containing any socket or FIFO. `live-only`
  permits stale entries only when neither a process nor the kernel's socket
  table reports them in use; it must configure the eligibility walk and
  in-use scanner together. See [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate).
- **Paths** must be absolute. `~` is rejected: it would mean `/root` for one
  caller and `/home/mark` for another. The only placeholder is `{uid}` (the
  effective user's id, e.g. in `/tmp/claude-{uid}`). Under `--allow-root`, a
  selected source using `{uid}` in its roots, paths, exclusions or
  `app_not_running` path is refused before planning: uid 0 cannot stand for
  one or more `owners:` entries. Supplied `--roots` using `{uid}` are refused
  for the same reason.
  Root lists (`find.roots`, tier `roots`, `protect` roots, and `--roots`)
  also reject `.` and `..` components, repeated slashes, trailing slashes,
  and duplicate entries. `/` itself is valid. After safe resolution,
  filesystem and Cargo discovery walk one root for each reported directory
  identity, choosing the outermost listed route first. A root whose inode
  identity is unavailable is skipped as `unsupported_fs`.
- **Literal guards:** `where.has`, `has_dir`, `has_not`, `lacks_entry`, and
  `protect_links`, `protect_link_dirs`, `protect_parent_link`,
  `protect_cursor_extensions` compare names or paths literally. They reject
  `{uid}` at config load because they do not expand it. The source's
  `app_not_running` path does expand it and follows the root-mode rule above.
- **Validation:** the config is validated completely at load, before
  anything is deleted; the full list of static and run-time checks is in
  [safety.md](safety.md#validation). `version` is mandatory; tier names are
  unique across all groups.
- Disabling a tier = commenting it out. There is no `enabled` field.

In this development build, `check` validates these fields, including all
configured groups. `ensure` runs configured groups in written order when
`--select` is absent. `ensure --select` accepts one positive source term with
supported idle and version filters, including negated idle filters, and unions
of positive filesystem sources with exactly the same `delete` or `command`
action and options. A filesystem union discovers each source separately, deduplicates
matching candidates by inode identity, and applies `checkouts: refuse` to a
candidate if any source term that admits it after its filters refuses
checkouts. A source contributes retention guards when its bound root, included
path pattern and type match after `find.exclude` and safe mount and identity
checks. Its own `where`, anchor and term filters govern action admission;
failing or uncertain source evidence does not erase the raw guard match.
Every such source applies its positive `keep_newest`, link and
installed-extension guards to the shared candidate, even when that term's
evidence, idle or negated-profile checks fail. A protected or unclassifiable
guard retains the candidate. Tallied candidates are checked before ranking;
an exhausted tally budget defers that check for untallied candidates until
reclaim. Every candidate is checked before its action, under the candidate
lock if it proceeds that far, and immediately before delete or command. A
guard refusal is attributed to the guarding source. For an admitted candidate,
term-local evidence and admitted-term checkout policy still decide which source
supplies the action and report identity. It rechecks each term under one
candidate lock. A command union
invokes its shared argv at most once per inode, with `DISKGC_PROFILE` and the
output prefix of the source selected by that locked recheck. If the final
policy scan excludes that source before the command starts, the union judges
every term again under the same lock, including its filters and checkout
setting, then tries another admitted source. Each source can lose this final
check once; the command is never run twice. A union of the same `cargo-target` source
with only positive idle filters is reduced to its shortest idle requirement;
an unfiltered member removes only the filters' added bound, preserving the
source's mandatory `min_idle_for` when set. The usual Cargo plan and
lock-time reclaim checks then apply once per target. Other Cargo unions are
deferred.
Filesystem and Cargo sources can use negated profiles. Filesystem terms can
use either polarity of a filter with a single `has` or `has_not` predicate.
Positive filesystem filters may combine `has`, `has_not`, `idle_for`,
`keep_newest` and link protection predicates in one profile: all predicates
must pass. The entry checks, idle measure and retention guards are repeated
under the candidate lock. A negated filesystem filter may combine `has`,
`has_not` and `idle_for`: it admits a candidate when their complete
conjunction is known false, and excludes it as `filtered` when all predicates
match. Retention guards cannot be negated, including as part of a mixed
filter. Unknown entry type or age admits neither polarity, even if another
predicate is known false. Its entry checks and idle measure are repeated
under the candidate lock; in a union, only admitted terms can impose their
checkout policy. The development build refuses a nonempty `ipc: live-only`
until its run-time check is implemented (tasks 16 and 21). Root mode requires
a protected config with `owners:`, resolves those owners before
planning, and refuses generic command, Docker prune and `{uid}` sources.
It never ignores a safety setting.

### Tier fields

| Field | Meaning |
|---|---|
| `name` | Unique; used in logs and reports. |
| `select` | Profile expression (see Selection grammar). |
| `roots` | Roots for the `roots: required` profiles in `select`. |
| `order` | `oldest` (default) or `best-fit`. |
| `timeout` | Limit for the tier's entire reclaim pass (default `10m`), also bounded by `ensure --timeout`; a running child is killed at the deadline. |

### `protect`

Profiles whose matches exclude overlapping candidates by **overlap**: a candidate
that is a protected match, lies inside one, or contains one is skipped
(`protected`). Entries are bare names for fixed-root profiles, or
`{profile: NAME, roots: [...]}` for `roots: required` profiles; a bare
`roots: required` name is a load-time error, so protection never depends on
which tier happens to supply roots. Only kinds `fs` and `cargo-target` can be
protected (Docker images and Nix paths are protected by Docker's and Nix's
own means: running containers, GC roots). Like `!profile:`, a protected
profile covers everything its `find` finds and ignores its `where`. The
example config protects `claude-code-tmp`. Filesystem and Cargo reclaim
re-enumerate matches under candidate locks and just before deletion; Docker
rechecks protection before invoking prune. An incomplete scan stops the
run. Concurrent changes in an independently writable protected tree cannot
be made atomic with deletion of another tree; do not rely on this rule as a
security boundary against another local writer. For a user-defined `command`
action, the command must itself limit its effects to the selected candidate;
diskgc cannot constrain arbitrary actions taken by that program.

### Protection marker

A file named `.diskgc-keep` in a filesystem candidate, anywhere inside it,
or in any directory between a profile root and the candidate, excludes it
(reported as `kept`).

## Profiles

diskgc's knowledge of *what* can be deleted and *how* lives in **profiles**:
named, composable rules for recognising one kind of cruft. Tiers say which
profiles apply and when.

```yaml
- name: python-pycache
  category: python / js
  description: Python bytecode cache directories through depth five below a supplied project root
  find:                              # makes this a *source* profile
    roots: required
    paths: [__pycache__, "*/__pycache__", "*/*/__pycache__", "*/*/*/__pycache__", "*/*/*/*/__pycache__"]
    type: dir
  action: delete
  examples:                          # checked by `diskgc profiles test` and CI
    root: /home/mark/src
    match: [/home/mark/src/app/__pycache__, /home/mark/src/app/pkg/sub/__pycache__]
    no_match: [/home/mark/src/app/__pycache__/nested, /home/mark/src/app/a/b/c/d/__pycache__]
  verified_with: CPython 3.13.15 and observed CPython 3.12 .pyc layout in a project tree, 2026-10-20
```

- **Source profiles** have `find`: roots plus path patterns relative to them
  (each pattern segment is one directory level; `**` is not allowed, so
  discovery never walks deeper than the patterns say), and the candidate
  type. An optional `find.exclude` omits exact matching paths relative to
  the same root. An excluded leaf is skipped before stat; an excluded
  intermediate directory may be statted and opened to follow a deeper
  positive pattern, but is never a candidate. `find` names candidates *positively*;
  nothing that no source profile
  found is ever a candidate.
- **Roots are fixed, required, or daemon-derived.** Filesystem profiles can
  use fixed roots (`/tmp`, `/var/tmp`) or `roots: required` (machine-specific
  locations such as `cargo-target`, `pytest-cache`). Docker profiles use
  `roots: daemon`: diskgc obtains the storage root from the bound local
  daemon and checks its filesystem against the requested path.
- **Filter profiles** have only `where` (predicates). They narrow a source's
  candidates; they never find anything.
- **`action`** says how a source profile's candidates are removed. Docker
  profiles declare one of the fixed Docker prune actions. Other code-backed
  kinds fix their action.
- **`kind`** (default `fs`): `fs` (a file or directory), or a code-backed
  kind: `cargo-target`, `docker-build-cache`, `docker-images`, `nix-store`.
- **`examples`**: paths that must and must not match. They check that the
  patterns do what their author meant; they do not prove that the matched
  files are disposable (a human decides that, from `survey --list`).
  Structural recognition (markers, Cargo layouts) is covered by fixtures in
  the test suite, not by path examples.
- **Built-in temp-prefix v1 preset:** only named built-ins opting in at load
  expand their base `find.paths` patterns under the fixed prefix list below.
  User profiles use exactly the paths they write; no placeholder or
  implicit expansion is added to the public YAML schema. `profiles show`
  prints a built-in's effective expanded patterns.

Fields (an unknown key anywhere in a profile is a load-time error):

The `command` action and Docker kind schema fields in this table
(their `kind` names, `action` forms and kind-specific constraints) are
**provisional until the release review (task-49)**. The
schema v1 freeze covers the other fields, the `delete` and Cargo actions,
the `nix-store` profile shape below, and `find.roots: daemon` as the
spelling for a root the bound backend supplies, in every backend kind
(final, TASK-118).

A `nix-store` source profile is exactly `name`, `category`, `description`,
`kind: nix-store`, `find: {roots: daemon}` and, on a built-in,
`verified_with`; any other field (`action`, `find.paths`, `where`,
`min_idle_for`, `examples`, `checkouts`) is a load-time error. The store
is not configurable: it is `/nix/store` of the bound system daemon
([safety.md](safety.md#nix-store)). Selected alone as `profile:NAME` (no
filters, no union), it reports one plan row and one action with candidate
`/nix/store`, or one skip with candidate `""`, and `estimated_bytes:
null`. Its skip reasons are those of [Backend skips and
errors](#backend-skips-and-errors) plus `changed`; the human `detail`
names the evidence (the client's canonical path, both versions, the
store URL seen, the generation link the daemon did not report) and is not
part of the contract.

| Field | On | Meaning |
|---|---|---|
| `name` | every profile | A `NAME` (see Selection grammar), unique among built-in and user profiles. |
| `category`, `description` | every profile; required on built-ins | Free text, shown by `profiles list`. |
| `kind` | sources | Default `fs`. |
| `find.roots` | sources | A list of absolute directories, `required`, or `daemon` for Docker kinds and `nix-store`. |
| `find.paths` | kind `fs`, required | Patterns relative to the roots, segments separated by `/`; no segment is empty, `.` or `..`, or contains `**`. Each segment matches one name, compared as bytes: `*` matches any run of bytes (none included, and a leading `.`: `*` matches `.X11-unix`), every other byte itself; `?`, `[`, `]` and `\` are reserved (a load-time error), so a later glob syntax cannot change what an existing pattern matches. `{uid}` is replaced first. |
| `find.exclude` | kind `fs`, optional | Paths omitted from this source, with the same syntax and root as `find.paths`. An excluded path is never a candidate or a skipped candidate. An excluded leaf is not statted; an excluded intermediate directory may be opened to reach descendants matched by a deeper positive pattern. The exclusion applies to that exact relative path, not automatically to descendants. A present `null` is invalid. |
| `find.type` | kind `fs`, required | `dir` (a directory), `file` (a regular file) or `any` (a directory or a regular file). Symlinks, sockets, FIFOs and devices are never candidates, whatever the type: a wildcard profile over `/tmp` would otherwise report every symlink there. |
| `where` | filters (at least one predicate); optional on sources | See Predicates. A source’s own predicates narrow action admission in every term, but its positive retention guards cover raw `find` matches even when other source predicates fail. `!profile:` and `protect` ignore source `where`. A source has no `where.idle_for`; the tier chooses its idle filter. `app_not_running` and `lacks_entry` are source-only. |
| `min_idle_for` | sources, optional | A nonzero duration that every candidate must meet even when the source is selected alone. The tier may demand a longer idle period with filters. A source uses this when a shorter period would make its matched entries unsafe to reclaim. |
| `action` | kinds `fs` and Docker, required | See Actions. Cargo and Nix kinds fix their action and take no `action`. |
| `examples` | kind `fs`, required | `{match: [...], no_match: [...]}`, absolute paths, at least one `match`; built-in and user profiles alike (User profiles). With `roots: required`, an optional `root: /absolute/sample/root` pins the supplied root used to check path depth in these examples; without it, any absolute ancestor can supply the root. Config load and `check` reject examples that contradict the path patterns. |
| `checkouts` | sources, any kind | `refuse` (the default, also when absent) or `allow`; any other value is a load-time error. Whether a candidate may hold a version-control checkout (`.git`, `.hg`, `.svn`) or lie inside a checkout's metadata; refused ones are `contains_checkout`. `allow` can delete a repository with uncommitted work: see [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate) (No source checkout) before writing it. |
| `verified_with` | built-in sources, required | The tool version the profile was checked against. |
| `override` | user profiles | See User profiles. |

`cargo-target` recognises Cargo target directories by their structure
([safety.md](safety.md#discovery-idleness-eligibility)), so its `find` has
only `roots`, and it takes no `examples`: `{kind: cargo-target, find: {roots:
required}}`.

Top-level `profile_policies:` changes only the checkout guard of a named
built-in source without copying its matcher or action. Each entry requires
`profile: NAME` and `checkouts: refuse | allow`; null, unknown names, filters,
and duplicate names are errors. A name also defined in `profiles:` is an
error; set `checkouts` inside that profile instead. An omitted policy leaves
the built-in setting intact: `cargo-target` explicitly allows checkouts,
while `npm-cache` and `uv-cache` currently default to `refuse`. The effective
setting appears in `profiles show --json` and in every dry-run plan entry's
`checkouts` field. Explicit `allow` emits a warning from `check` because it
can expose uncommitted work. The setting does not alter roots, matches,
predicates, actions, or the active release catalog.
`/tmp` remains an explicit root, as in the tier example under Commands;
the built-in profile has no implicit or fixed temp root.

### Predicates (`where`)

| Predicate | Meaning | Kinds |
|---|---|---|
| `idle_for` | not used or changed for this long, as the kind measures it (below) | `fs`, `cargo-target`, `docker-build-cache` |
| `has` / `has_not` | contains / does not contain a named file (e.g. `CACHEDIR.TAG`): a regular file of that name directly in the candidate directory (not followed; a file candidate contains nothing) | `fs` |
| `has_dir` | contains a directory of this name directly in the candidate directory (not followed); checked again under the candidate lock before reclaim | `fs` source only |
| `app_not_running` | A path to this application's Chromium-family `SingletonLock` symlink, absolute or relative to each source root (for a home root, `.config/BraveSoftware/Brave-Browser/SingletonLock`). The candidate is eligible only when the lock is absent in a safely reached directory. Every present lock retains it, even if its PID appears gone: that PID may be live in another namespace sharing the home directory. Unreadable locks and unsafe paths also retain it. Each source names one application's lock. | `fs` sources only |
| `lacks_entry` | contains no entry of this name directly in the candidate directory, of any type (file, directory, symlink, FIFO, socket, or device); checked again under the candidate lock before reclaim | `fs` source only |
| `source_anchor` | `cmake`: require `CMakeCache.txt` to record a normalized `CMAKE_HOME_DIRECTORY` that is a strict ancestor of the candidate under its bound root, with a regular `CMakeLists.txt` that survives outside the candidate; `meson`: require regular generated `meson-info/meson-info.json` with matching build and info paths, a normalized strict-ancestor source under the root, regular `meson-private/coredata.dat` and surviving regular source `meson.build`; `autotools`: require regular `config.status`, `config.log`, and `Makefile`, with the status script recording the exact build path and a strict-ancestor source that still contains regular `configure.ac`. Source identity and metadata are rechecked under the candidate lock. | `fs` source only |
| `keep_newest` | keep the newest positive N matching siblings by directory/file mtime; equal mtimes break by descending name bytes. Missing mtime or incomplete sibling listing keeps the candidate. For Cursor extensions `newest_group: cursor_extension` ranks each extension id separately; unknown name layouts are kept. | `fs` |
| `protect_links` / `protect_link_dirs` / `protect_parent_link` | keep a candidate containing a resolved symlink target. `protect_links` lists exact paths, `protect_link_dirs` scans every symlink in literal directories relative to the bound root, and `protect_parent_link` names a symlink beside the candidate's parent (e.g. a package's `current` beside `releases/`). An unsafe or unreadable directory, incomplete listing, ambiguous target, or missing named link retains the candidate. An unrelated launcher resolving outside the bound root is ignored; a dangling external launcher is ignored only after every existing prefix was checked for symlinks. Target paths with internal `..` are ambiguous and retain the candidate. Links are re-read during reclaim under the candidate lock. | `fs` |
| `liveness_contract` | a code-backed producer liveness contract (`nix-shell`; designed: `pytest`, `node-project`, `python-project`): the candidate is eligible only when the contract permits it, judged from the same process scan as the in-use check, in the plan and again immediately before deletion ([Liveness contract predicate](#liveness-contract-predicate)) | `fs` source only |
| `protect_cursor_extensions` | a literal path relative to the bound root to Cursor's `extensions.json`, beside extension candidates. Every `relativeLocation` listed as installed is kept, even if older than the newest N. The JSON array and each entry's `location.path` must agree with that one directory name. A missing, unreadable, malformed, oversized (over 1 MiB) or changing manifest retains all candidates. | `fs` |

In YAML, `where: {idle_for: 3d}` (a nonzero duration), `where: {has: CACHEDIR.TAG}`
(one file name), or `where: {app_not_running: .config/chromium/SingletonLock}`.
A present null or blank `app_not_running` is a load-time error; omitting the
key is allowed. `idle_for: 0s` is a load-time error: it would accept every
candidate with a known, nonfuture age. Each predicate appears at most once per profile, and all of
a profile's predicates must hold. A predicate on a kind
that does not support it is a load-time error. Two `idle_for` in one term
(its `filter:`s) and the source's `min_idle_for` combine to the longest;
a `!filter:` is a complement and does not combine. When the measure cannot
tell a candidate's age (the
kernel did not answer a timestamp, or the last use lies in the future),
the candidate passes neither `filter:` nor `!filter:` on `idle_for`: it is
skipped (`unsupported_fs`, `future_time`). Ownership is not a
predicate: it is an always-on check ([safety.md](safety.md)).

`keep_newest` and link protection are evaluated on a source's candidates
after discovery. They can be source `where` predicates or positive
`filter:` factors. Multiple `keep_newest` rules retain the greatest N;
conflicting grouping, parent-link scopes or Cursor manifests are rejected. A changed or
unrecognised version layout retains data. The
link check occurs again immediately before the deletion walk, but no
filesystem API makes a link update and deletion atomic: installers must
coordinate with cleanup if they repoint a link concurrently.
`keep_newest`, link protection (including `protect_parent_link`), and
`protect_cursor_extensions` are retention guards. A filter containing any
retention guard cannot be negated, even when it also contains `has`, `has_not`
or `idle_for`. Positive guard filters remain valid. Negated `idle_for`, `has`
and `has_not` filters still complement their known evidence; unknown evidence
passes neither polarity.

| Kind | `idle_for` means |
|---|---|
| `fs` | newest mtime or ctime in the candidate's tree (atime is ignored: under `relatime` any reader, such as a nightly scan, refreshes it) |
| `cargo-target` | last build: newest mtime or ctime of the profile dirs and everything in them, on every Cargo layout ([safety.md](safety.md#discovery-idleness-eligibility)) |
| `docker-build-cache` | last use, as recorded by Docker (`--filter until=`) |

The report always names the measure used.

### Liveness contract predicate

Status (TASK-112 step 4): `nix-shell` is implemented and judged, and the
`nix-shell-rc` built-in uses it; `pytest`,
`node-project` and `python-project` are designed below and refused at
load ("designed but not implemented yet", naming the step) until they
land, so nothing half-built can be selected. What each contract shows,
and when it cannot decide, is in
[safety.md](safety.md#producer-liveness); which built-in uses which
contract is in [Liveness contracts](#liveness-contracts).

`liveness_contract` is a source `where` predicate of kind `fs` whose value
names one code-backed contract, as `source_anchor` names a build system.
It holds only when that contract permits reclaiming the candidate. The
name says which evidence was checked, not that the producer is proved
gone: how much a contract proves is its class.

```yaml
where:
  liveness_contract: pytest
```

| Value | Candidates it accepts | Contract (safety.md) |
|---|---|---|
| `nix-shell` | directories named `nix-shell-<pid>-<suffix>` or `nix-develop-<pid>-<suffix>` | nix-shell rc directories (Owner PID) |
| `pytest` | a pytest session directory `pytest-of-*/pytest-*` | pytest sessions |
| `node-project` | a `node_modules` directory whose parent holds `package.json` and that lies in no other dependency directory | project dependency directories |
| `python-project` | a virtual environment (holding `pyvenv.cfg`) whose parent holds a Python project manifest and that lies in no other dependency directory | project dependency directories |

There is no default: without the key no contract is required.
`min_idle_for` stays a separate field; each built-in's value is in
[Liveness contracts](#liveness-contracts). The project contracts measure
the project against the term's idle bound, of which `min_idle_for` is a
part.

Validation (load-time errors, exit 2; `check` reports them without
touching the machine):

- a present null or a blank value (a written null must not remove a
  guard);
- a value that is not one of the contract names, the message naming them
  (`liveness_contract: pytset`); a mapping or a list;
- a contract this version does not judge yet (`pytest`, `node-project`,
  `python-project` until they land), the message naming the supported
  ones, so nothing half-built can be selected; the find-shape and
  checkout rules below apply to each from the version that judges it;
- `liveness_contract` on a filter profile: it is a source predicate, since
  the producer belongs to what the source finds; on a kind other than
  `fs`: the existing "predicate on a kind that does not support it" error;
- a source whose `find.type` is not `dir`, or whose `find.paths` patterns
  do not end in the contract's shape: `nix-shell-*-*` or
  `nix-develop-*-*` for `nix-shell`; `pytest-of-*/pytest-*` for `pytest`;
  `node_modules` for `node-project`; any name for `python-project`,
  whose contract itself requires `pyvenv.cfg` in the candidate;
- a project contract on a source that says `checkouts: allow`.

Each predicate appears at most once per profile, so a source has at most
one contract; `liveness_contract` combines by `&&` with the source's other
predicates (`app_not_running` included). In a union (`profile:a ||
profile:b`), every admitted source that found the candidate and whose
contract recognises it as an instance must permit it, whichever term
selects it: a contract states a fact about the candidate's producer,
which another term's admission does not change (a veto, as retention
guards veto). A name the contract does not recognise carries no such
fact and vetoes nothing; that term alone filters it. The plan and the reclaim
apply the same rule. `profile_policies` cannot change it. A user profile may use it like any predicate. A new contract is a new
value, added with the producer it was verified for; a generic form (a pid
pattern, a producer lock) is added only when a second producer needs the
same evidence.

Report additions (they extend the JSON enums of Output). Landed with
`nix-shell`: `producer_unknown` and `owner_pid`; `temp_root`,
`command_line` and `relative_path` land with `pytest`:

- skip reason `producer_unknown`, used only for a recognised instance of
  a contract whose producer state cannot be determined. Its detail names
  the failed rule: a malformed marker, a filesystem not on the local list,
  not the initial PID namespace, `hidepid` on the held `/proc`, a process
  that stayed unreadable, a relative or unresolvable temp-root or
  `--basetemp` value, an unreadable command line or environment, an
  oversized or unparsable workspace `package.json`, a project climb that
  reached a bind-mounted subdirectory. Its `process_blockers` is `null`
  unless one process caused it; then it holds that process's ordinary
  entry (a pid, as every entry has) with evidence `unreadable` (existing)
  or `relative_path` (new: the process has a relative or unresolvable
  value of a variable pytest resolves).
- what is not a recognised instance is `filtered` (a name without a valid
  pid, a name that is not a session name, a missing manifest, a nested
  dependency directory); a failed listing or project walk is
  `unreadable`; a project tree over the entry budget is
  `too_many_entries` (all existing reasons);
- `process_blockers` evidence `owner_pid` (a listed process carries the
  recorded pid number, a number match, not proof that it is the same
  process; the entry's pid is that process's pid in diskgc's namespace),
  `temp_root` (a process could use the session's temp root) and
  `command_line` (an element of a process's command line names the
  session; a value in its environment that names it is the existing
  `environment`). A pid that may be in use is `in_use`, as for
  `app_not_running`; a project that changed or was read within the idle
  bound is `not_idle`.

`profiles show` prints the value. `app_not_running` is the shipped
presence-marker contract and stays as it is.

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
parses back to the same expression). The `pytest-tmp` rows here use a
fixture-only parser catalog; that deletion profile is not shipped:

| Expression | Normalised | Selects |
|---|---|---|
| `profile:pytest-tmp && filter:idle-10d` | `profile:pytest-tmp && filter:idle-10d` | pytest temp dirs idle for 10 days |
| `(profile:pytest-tmp \|\| profile:node-compile-cache) && filter:idle-3d` | `(profile:pytest-tmp && filter:idle-3d) \|\| (profile:node-compile-cache && filter:idle-3d)` | either, idle for 3 days |
| `profile:chromium-tmp && filter:idle-2d && !filter:idle-30d` | `profile:chromium-tmp && filter:idle-2d && !filter:idle-30d` | Chromium scratch idle for 2 to 30 days |
| `profile:node-compile-cache && filter:idle-2d && !profile:claude-code-tmp` | `profile:node-compile-cache && filter:idle-2d && !profile:claude-code-tmp` | Node cache idle for 2 days, except what overlaps Claude Code's session dirs |
| `profile:pytest-tmp \|\| profile:node-compile-cache` | `profile:pytest-tmp \|\| profile:node-compile-cache` | either, at any age |
| `(profile:pytest-tmp && filter:idle-3d) \|\| profile:node-compile-cache` | `(profile:pytest-tmp && filter:idle-3d) \|\| profile:node-compile-cache` | pytest dirs idle for 3 days, Node cache at any age |
| `((profile:pytest-tmp \|\| profile:node-compile-cache) && filter:idle-3d) \|\| (profile:chromium-tmp && filter:idle-30d)` | `(profile:pytest-tmp && filter:idle-3d) \|\| (profile:node-compile-cache && filter:idle-3d) \|\| (profile:chromium-tmp && filter:idle-30d)` | the first two idle for 3 days, Chromium scratch idle for 30 |

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
- **Advisory warnings preserve the selection.** A duplicate union term or
  repeated filter factor warns. A term that excludes its own source with
  a negated source factor, or requires both positive and negative forms of
  the same filter, also warns because that term selects nothing. Other terms
  in its union can still select candidates. `check` and `ensure --select` print these
  warnings on stderr; they do not change the exit status.
- **Depth is explicit.** A `find.paths` segment reaches exactly one level;
  there is no `**` or implicit any-depth search. Profiles for project
  artifacts list the supported depths or take each project as a root. A
  deeper artifact outside those patterns is not found, even when a shallower
  one has the same name. When a source finds an outer candidate, it does not
  descend into that candidate to find a nested match of the same source. A
  path that matches a pattern but fails the source's own `where` is not a
  candidate, so discovery may continue to a deeper pattern. `!profile:` and
  `protect` ignore that `where` and use the outer raw match for overlap.
- **`||` is a union**; all source profiles in one expression share one kind
  and one action, so `a || b` and `b || a` select and remove exactly the
  same. Different actions belong in different tiers.
- **`!` only narrows.** `!filter:…` is a path-local complement of known
  evidence predicates; a filter containing a retention guard cannot be
  negated.
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
- **Cargo unions** may combine terms of the same `cargo-target` source with
  positive `idle_for` filters. The union accepts a target at the shortest
  required age; an unfiltered member still enforces the source's
  `min_idle_for`, if set. Unions of
  different Cargo sources remain unsupported by the runtime.
- Filters are evaluated from the index where it already knows the answer,
  and always re-evaluated at deletion time. A candidate that fails an idle
  filter is skipped as `not_idle`; one that matches a negated idle filter is
  skipped as `too_idle`. An unknown age passes neither polarity and is
  skipped as `unsupported_fs` or `future_time`. Other failed filters report
  their own reason. A known entry predicate that excludes a candidate reports
  `filtered`; an unreadable entry cannot pass either polarity.

Errors. A rejected expression is a load-time error (exit 2). The report
starts with `invalid selection`, the expression as written and, for a
syntax error, the column; then the message. The rules that depend on what a name is
report every broken rule, separated by `;`. Where a message suggests a
rewrite, the rewrite fixes that error without breaking another rule (if the
expression has other errors, they still need fixing); a suggestion that would
itself be rejected is left out (`filter:nix-garbage` below: `!profile:nix-garbage`
is rejected too). These examples use the parser test catalog, including
fixture-only names such as `pytest-tmp`, `tmp-entry` and `nix-garbage`; those names are not
necessarily shipped built-ins. The catalog also defines `a`, `b` (kind `fs`,
action `delete`) and `f` (`idle_for`):

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
| `profile:tmp-entry && !filter:retention-newest` | `!filter:retention-newest` uses `keep_newest`, a retention guard that cannot be negated |
| `profile:nix-garbage \|\| profile:nix-garbage` | kind nix-store acts on everything it finds at once (no per-candidate enumeration), so its expression has one term, not a union of 2; select each in its own tier |
| `profile:nix-garbage && filter:idle-7d` | kind nix-store takes only `profile:NAME`, so `filter:idle-7d` cannot narrow it: it has no per-candidate enumeration |
| `profile:tmp-entry && !profile:docker-dangling` | `!profile:docker-dangling`: a negated profile must be of kind fs or cargo-target, and `docker-dangling` is of kind docker-images |

### Actions

| Action | Use |
|---|---|
| `delete` | diskgc's own deletion ([safety.md](safety.md#deletion)). Option `make_writable: true` adds owner write permission to directories during the delete walk, after the last in-use recheck, for read-only trees such as Go's module cache or files copied from the Nix store. Opt-in per profile, since a read-only directory can be a deliberate protection. |
| `command` (provisional) | an argv list run per candidate, e.g. `[env, "GOCACHE={path}", go, clean, -cache]`. Trusted, arbitrary command execution; never run as root. See [safety.md](safety.md#command-actions). |
| `docker-builder-prune` (provisional) | Prune the bound Docker Engine's built-in BuildKit cache with `--all`, adding `--filter until=...` when the tier selects `idle_for`. One action covers the daemon cache. |
| `docker-image-prune` (provisional) | Prune dangling images in the bound Docker daemon. |
| `docker-image-prune-all` (provisional) | Prune all images unused by containers in the bound Docker daemon. |
| kind actions | Fixed by the kind: the Cargo lock protocol ([safety.md](safety.md#cargo-target)); for `nix-store`, one bounded GC through the bound Nix daemon ([safety.md](safety.md#nix-store)). |

In YAML:

```yaml
action: delete
action: {delete: {make_writable: true}}
action: {command: [env, "GOCACHE={path}", go, clean, -cache], cwd: parent}
action: docker-builder-prune
action: docker-image-prune
action: docker-image-prune-all
```

Command action fields: `command` (argv; `{path}` is replaced by the
candidate path as one argument or inside one, never through a shell; use
`[sh, -c, "..."]` explicitly if a shell is wanted) and `cwd` (`candidate` or
`parent`; the default is `candidate` for `type: dir`, `parent` for `type:
file`, and for `type: any` decided per candidate the same way; `cwd:
candidate` with `type: file` or `type: any` is a load-time error). The
program (`command[0]`) is a name or path without `{path}`: it is resolved
and logged to stderr before anything runs ([safety.md](safety.md#command-actions)),
so it is never the candidate. The tier's remaining `timeout`
bounds it (Tier fields). Two sources have the same action only if the
action and every option are equal; the "one action" error shows a command
action as its quoted argv and `cwd`. Environment passed:
`DISKGC_PATH`, `DISKGC_NEED_BYTES`, `DISKGC_MOUNT`, `DISKGC_PROFILE`.
For a command union, `DISKGC_PROFILE` is the source selected after the
candidate's locked recheck; the command runs once even if several sources
admit the same inode. Most cleaners pick their cache from their own config or environment, not from
the working directory, so the command must name the location explicitly
(`pip --cache-dir {path} cache purge`, `env GOCACHE={path} go clean
-cache`). A non-zero exit is an error in the report (exit 3 if the goal is
not met).

No Podman image-prune example ships yet. A read-only dry run on rootless
Podman storage found subuid-owned overlay files, so a command profile using
`containers/storage` as its candidate fails the required ownership walk with
`not_owner`. Running `podman image prune` against a different checked path
would not protect the image store it changes. Task-93 covers a Podman-specific
protocol that uses Podman's own pruning rules.

### Built-in profile library

Shipped inside the binary as YAML data, listed with `diskgc profiles list`
and at the end of `diskgc --help`.
A built-in profile must satisfy the profile-specific
[Definition of Done](testing.md#definition-of-done-every-task).
Every row below is a v1 *target*; until verified it is
listed as planned, not shipped.

The optional `proposed-release-01-catalog` build feature narrows the catalog.
On its own it retains the development sandbox gate. The separate
`release-01-candidate` package combines that feature with
`diskgc-linux/release-without-sandbox-gate` for disposable-container testing;
it is not approved for deployment. The candidate keeps `cargo-target` and the seven
idle filters, and rejects every operator-authored profile, including
overrides. It also refuses configured tiers and `--select` expressions that
refer to excluded built-ins, even during a dry run. `profiles list`,
`profiles show`, `check` and help use the same narrowed library.
`profile_policies` for admitted `cargo-target` works, but policies for
excluded sources, including `npm-cache` and `uv-cache`, are refused at load
time. No `/tmp` source is admitted yet: candidate policy has not established
which temporary paths are safe for release. The final 0.1 catalog and
operator-authored `fs` and delete policies remain release decisions under TASK67.

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | planned: pytest session directories `pytest-of-*/pytest-*`, one candidate per session (Mark, 2026-10-23), under `/tmp`'s temp-prefix v1 preset; a live pytest session can have no open descriptor in it, and `tmp_path_retention_policy=none` or `--basetemp` omits the session `.lock`; contract designed, see [Liveness contracts](#liveness-contracts) | no safe action yet |
| test | `pytest-cache` | `.pytest_cache` (roots required) | `delete` |
| nix | `nix-user-cache` | `.cache/nix/{gitv3,tarball-cache,tarball-cache-v2,eval-cache-v*}` below a supplied home root | `delete` |
| nix | `nix-shell-rc` | rc directories `nix-shell-*-*` and `nix-develop-*-*` directly in `/tmp` (not below shell temp directories); `liveness_contract: nix-shell`, `min_idle_for: 1h` ([Liveness contracts](#liveness-contracts)) | `delete` |
| nix | `nix-shell-tmpdir` | planned: `nix-shell.*` directories under `/tmp`'s temp-prefix v1 preset, including below `/tmp/claude-{uid}`; dormant shells can use them without an observable descriptor or initial environment path | no safe action yet |
| nix | `nix-garbage` | planned: dead store paths of the bound multi-user daemon (`{kind: nix-store, find: {roots: daemon}}`); protocol designed (TASK-118), unshipped until TASK-36 | no shipped action yet |
| rust | `cargo-target` | Cargo `target/` dirs (roots required); `checkouts: allow`: build scripts clone sources into a target (a vendored dependency in `build/<pkg>/out/`), which is Cargo's own output, reclaimed by its lock protocol | lock protocol |
| containers | `docker-build-cache` | BuildKit cache of the bound Docker Engine | prune |
| containers | `docker-dangling` | untagged images | prune |
| containers | `docker-unreferenced` | images no container uses (removes tagged images too) | prune |
| containers | `containers-image-tmp` | withdrawn (Mark, 2026-10-23): matched top-level `/var/tmp/buildah*` and `/var/tmp/container_images_storage*` with a mandatory `min_idle_for: 3d`, which cannot show that a producer is done with a spool it may reopen; removed from the library (TASK-112 step 0), its matching set kept as the user-probe vector `containers-image-age`; re-admission only under a new name after TASK-125 | none |
| agents | `cursor-agent-versions`, `codex-releases`, `claude-code-versions` | versions under a supplied home root; keep the two newest per sibling set and the installed symlink targets. `claude-code-versions` matches native installer version files only, not `~/bin/claude-*` | `delete` |
| editors | `cursor-extensions` | version directories under `.cursor/extensions` of a supplied home root; keep the two newest of each extension id and every installed directory in `extensions.json` | `delete` |
| ai agents | `claude-code-tmp` | `/tmp/claude-{uid}/-*/*` session dirs (project dirs are named after the project's path, so they start with `-`; `/tmp/claude-{uid}` is also the `TMPDIR` of the tools Claude Code runs, so `*/*` would match inside their temp dirs too) | `delete` |
| browsers | `chromium-tmp` | `.org.chromium.Chromium.*` (files) and `org.chromium.Chromium.*` (directories: URL fetcher and unpacker scratch of Chromium and Brave) in `/tmp` | `delete` |
| browsers | `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Only `Cache` and `Code Cache` directories in `Default` and `Profile *` under the corresponding browser's `.cache` directory below a supplied home root; each source requires its own `.config/.../SingletonLock` to be absent | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner |
| caches | `go-mod-cache` | `go/pkg/mod` below a supplied home root | `delete` with `make_writable` |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required) | `command` (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `bun-cache` | `.bun/install/cache` or `.cache/.bun/install/cache` below a supplied home root | `delete` |
| python / js | `python-pycache` | `__pycache__` directories at relative depths 1 through 5 below each supplied project root | `delete` |
| python / js | `mypy-cache` | `.mypy_cache` directories at relative depths 1 through 5 below each supplied project root, with a regular `CACHEDIR.TAG` file | `delete` |
| python / js | `node-modules`, `python-venv` | planned: `node_modules` directories, and `.venv` directories holding `pyvenv.cfg`, at relative depths 1 through 5 below a supplied project or workspace root and inside no other dependency directory, judged by their project's activity; `checkouts: refuse` | no safe action yet; contract designed, see [Liveness contracts](#liveness-contracts) |
| python / js | `jest-cache` | Jest's default `jest_*` cache under `/tmp` or `/var/tmp`, including the temp-prefix v1 preset below | `jest --config '{}' --cacheDirectory '{path}' --clearCache` from the checked parent; Jest removes the measured directory itself |
| python / js | `playwright-tmp`, `node-compile-cache` | `playwright*` and `node-compile-cache` under `/tmp`'s temp-prefix v1 preset | `delete` |
| c / c++ | `cmake-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `CMakeCache.txt`, a direct `CMakeFiles/` directory, no `CMakeLists.txt` entry, and a surviving recorded CMake source ancestor; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| c / c++ | `meson-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `build.ninja`, a direct `meson-private/` directory containing regular `coredata.dat`, generated `meson-info/meson-info.json` recording this exact build directory and a surviving Meson source ancestor, and no `meson.build` entry; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| c / c++ | `autotools-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with regular `config.status`, `config.log`, and `Makefile`, status script recording this exact build directory and a surviving `configure.ac` source ancestor, and no `configure.ac` entry in the output; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| jvm | `gradle-caches`, `maven-repo` | planned: `~/.gradle/caches`, `~/.m2/repository` (roots required) | no verified whole-directory cleaner: Gradle maintains cache entries by retention after a build, while Maven's dependency purge applies only to a project's dependencies |
| embedded | `zephyr-tmp` | `/tmp/zephyr-*` scratch and `zephyr-*-run.*` dirs | `delete` |
| embedded | `west-build-anchored` | Zephyr `build/` dirs through depth five below supplied project roots; regular `CMakeCache.txt`, direct `zephyr/` directory, no `CMakeLists.txt` entry, surviving recorded CMake source ancestor under the root, and mandatory 3-day idle age | `delete` |
| embedded | `renode-tmp` | `renode-*` directories under `/tmp`'s temp-prefix v1 preset, including Zephyr run dirs; plain files are excluded | `delete` |
| embedded | `platformio-cache` | `.platformio/.cache` below a supplied home root; PlatformIO's default `core_dir/.cache` | `pio system prune --cache --force` with `PLATFORMIO_CACHE_DIR=.cache`, starting in the checked `.platformio` parent; PIO re-resolves the path as described in [safety.md](safety.md#command-actions) |
| generic | `tmp-entry` | planned: top-level regular files and directories in `/tmp`, `/var/tmp`; a dormant process can use an entry without an observable descriptor or initial environment path | no safe action yet |
| generic | `mktemp-dir` | planned: `tmp.*` directories under `/tmp`'s temp-prefix v1 preset; a live shell can use one as `TMPDIR` after exec, invisible to the process scan | no safe action yet |
| generic | `xdg-cache-entry-v2` | top-level entries of a supplied `$XDG_CACHE_HOME`; excludes known state, browser roots and dedicated/deferred caches; `checkouts: refuse`; `min_idle_for: 30d` | `delete` |
| filters | `idle-1h`, `idle-1d`, `idle-2d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d` | (no `find`) | none |

`has-cachedir-tag` in the selection error examples is a parser-test fixture,
not a shipped built-in filter.

The command cache profiles use the tools' own cleaners on the exact measured
directory. They retain the default `checkouts: refuse` safety rule. Some real
uv and npm caches contain `.git` metadata; those candidates report
`contains_checkout` and are left intact. The profile does not override that
check merely because the tool has a cleaner. A cache tool must be available
on `PATH` for its command profile to run.

**Built-in temp-prefix v1 preset.** `node-compile-cache`, `playwright-tmp`,
`renode-tmp` and `jest-cache` opt in. Each base pattern
(`node-compile-cache`, `playwright*`, `renode-*`, `jest_*` respectively)
is searched under each prefix below,
relative to each profile's fixed roots. The empty prefix is the root itself.
The list is finite: nested shell search stops after two shell levels, or
Claude's temp root plus two. No profile refers to another profile; each
candidate still belongs to its source profile and goes through its ownership,
in-use and eligibility checks. The effective expanded patterns are printed
by `profiles show NAME`. A changed prefix list changes the matching
set and therefore requires a v2 preset and a new built-in profile name.

| Prefix (relative to the root) |
|---|
| *(empty)* |
| `nix-shell.*` |
| `tmp.*` |
| `nix-shell-*-*` |
| `nix-develop-*-*` |
| `zephyr-*-run.*` |
| `claude-{uid}` |
| `nix-shell.*/nix-shell.*` |
| `nix-shell.*/tmp.*` |
| `tmp.*/nix-shell.*` |
| `claude-{uid}/nix-shell.*` |
| `claude-{uid}/tmp.*` |
| `claude-{uid}/nix-shell.*/nix-shell.*` |

The preset reaches selected *contents* inside nested shell temp roots. It
does not make the enclosing `nix-shell.*` directories, including those below
`/tmp/claude-{uid}`, deletion candidates. Those outer directories remain out
of scope until dormant-shell liveness can be established at deletion time.
For example, `node-compile-cache` can select
`/tmp/claude-1000/nix-shell.Ab1/node-compile-cache` while preserving
`/tmp/claude-1000/nix-shell.Ab1` and its other contents. This is exact
entry matching, not permission to reclaim the enclosing TMPDIR. A shell
that sets `TMPDIR` after exec without holding a descriptor in it can make
that outer directory appear idle to the process scan; no age threshold
proves it is unused.

The browser cache sources take a home directory as their required root. They
check that browser's `SingletonLock` before judging and again before deleting
each cache. Any present lock retains the cache, even when its PID appears
gone; a browser in another PID namespace can share this home directory and
write a PID that appears absent in diskgc's namespace. They match only
`Cache` and `Code Cache`; the browser profile, `WebStorage`, and `Service Worker/CacheStorage`
never match. A custom browser user-data or disk-cache directory is outside
these built-ins. The lock check
is specific to the default browser configuration; do not treat it as a
general process check for arbitrary Electron applications. The surveyed
Cursor and teams-for-linux installations have cache directories but no
`SingletonLock` in their application roots, so their profiles are deferred
until a reliable app-liveness signal is available. Firefox `cache2` also
needs a different guard and is outside this Chromium-family set.

- `pytest-tmp` is unshipped. The former source selected individual test
  directories, including Git fixtures made by tests. In a disposable
  container, pytest 9.1.1 with `tmp_path_retention_policy=none` created
  `/tmp/pytest-of-root/pytest-0/test_active0` without a session `.lock`;
  while the test slept, it held no descriptor in that directory. The
  process scan would mark this live directory eligible. An idle filter
  cannot prove the session has ended, and a lock-only guard misses this
  supported pytest configuration. Its planned return takes one session per
  candidate and the `pytest` liveness contract.
- `claude-code-tmp` sees a live session as `in_use` when a process holds a
  file or directory in its candidate. An active session can hold no scratch
  file descriptor; use a tier idle filter and `protect` for sessions that
  must remain, including the current session.
- `sccache` is deferred: whether its server is using the cache cannot be
  established from open files (an idle server need not hold any); it needs
  a server-aware protocol first.
- `containers-image-tmp` is withdrawn (Mark, 2026-10-23). Its 3-day floor
  measured changes, not whether a producer would reopen a spool; a VM test
  with a fixture producer showed a dormant, reused spool judged eligible
  ([safety.md](safety.md#in-use)). Whether real Podman or Buildah does
  that is verified first (TASK-125).
- Gradle and Maven cache profiles remain planned. A Gradle daemon that is
  idle may hold no files open, so the generic in-use check alone has not
  established a safe reclaim policy for them.
- `nix-user-cache`, `go-mod-cache` and `bun-cache` use plain deletion because
  Nix-installed versions of their tools may be absent from a long-running
  diskgc service's `PATH`. The exact cache directories were checked against
  Nix 2.34.8, Go 1.26.7 (`go env GOMODCACHE`), and Bun 1.3.3 (`bun pm
  cache`). Bun 1.3.3 reports `.bun/install/cache` by default; setting
  `BUN_INSTALL_CACHE_DIR` to the second path makes `bun pm cache` report that
  path, and it is populated on this machine. These caches need a network fetch
  to rebuild and may make offline builds fail until repopulated. Choose an
  idle or pressure tier. `go-mod-cache` needs `make_writable` because Go
  stores downloaded modules in read-only directories. Each source takes a
  supplied home root, not an arbitrary project root. These profiles match
  only the listed directories; they do not infer redirected cache locations
  from environment variables.
- `pytest-cache` finds only `.pytest_cache` through three directory levels
  below each supplied root. Supply project roots for deeper trees. It
  deletes pytest's result cache, which pytest recreates on the next run.
- `xdg-cache-entry-v2` takes the cache directory itself as a required root,
  normally `$XDG_CACHE_HOME` or `~/.cache`, and enforces a 30-day minimum age
  even when selected without a filter. It explicitly refuses checkouts: a cache entry such as
  `pre-commit` can hold hook repositories with local changes. Its exact-name
  exclusions keep `borg` and `huggingface` (backup or model data),
  `claude`, `cursor_api_sessions`, `dconf`, `gh`, `ghidra`, `gpauth`,
  `Microsoft`, `opencode`, `Palo Alto Networks`, `sdkmanager`, `sessions`,
  and `teleport-device` (session, credential or application state),
  `containers` (container storage), `torch` (models), `wine` (prefix data),
  and `pre-commit`
  (repository cache). It excludes `BraveSoftware`, `google-chrome`,
  `chromium`, `microsoft-edge`, and `mozilla` so this broad profile cannot
  bypass the browser profiles' narrower cache paths and live-app checks.
  It also excludes `.bun`, `ccache`, `go-build`, `nix`, `pip`, `uv`, and
  `yarn` so dedicated profiles retain their narrower boundaries and
  cleaner actions. `sccache` remains excluded because its active server
  state is not safely classifiable by an idle timestamp. The former
  `xdg-cache-entry` name was withdrawn before release: v2 narrows the
  match set, which requires a new built-in name. These exact-name
  exclusions apply to children of the supplied root. Supply the XDG
  cache directory itself, never a directory inside a browser or other
  application's cache. A redirected cache root is supplied explicitly;
  the profile never infers an environment variable.
  Unknown applications
  can still store valuable data under the cache root, and a 30-day idle
  threshold only measures filesystem changes. Review its read-only plan
  and add `protect` rules or `.diskgc-keep` markers for local data before
  enabling it. The broad source is excluded from an ordinary dedicated
  cache tier by selecting the dedicated profile alone; selecting the two
  in separate tiers can still let this profile reach other entries, so its
  built-in exclusions are the guarantee for the named dedicated paths.
- Profiles that find at several depths under a project root (`pytest-cache`,
  `cmake-build-anchored`, `west-build-anchored`, `python-pycache`,
  `mypy-cache`) cannot use `**`;
  they list depths explicitly or take one root per project. The anchored
  CMake and West profiles reach five levels below each
  root. Supply the project or workspace directory as a root when a build lies
  deeper. A changed depth bound requires a new built-in name because it
  changes the profile's matching set. The former marker-only `cmake-build`
  and `west-build` names were retired: selecting either now fails validation.
  Use the anchored names after reviewing their source and idle requirements.
- `python-pycache` and `mypy-cache` use one explicitly supplied project or
  workspace root. Both stop at depth five and retain the default checkout
  guard. `mypy-cache` additionally requires a regular `CACHEDIR.TAG` in the
  candidate itself. They can be regenerated by Python and mypy, including
  while a project is active; the general in-use checks still apply. A root
  above many projects may take time to discover and walk every cache.
- A `node_modules` or `.venv` directory can be unchanged while its owning
  project is used. The current `idle_for` measure sees only changes inside
  the candidate, so it cannot establish project inactivity. The planned
  profiles judge the parent project instead, a verified heuristic Mark
  accepted on 2026-10-23
  ([safety.md](safety.md#producer-liveness), Project dependency
  directories); neither ships before it is implemented.

**A built-in profile's matching set never changes under its name.** Any
change to what a built-in matches, narrower or broader, ships under a new
name (e.g. `claude-code-tmp-2`). An old name is never redefined; a new config
`version` may retire it (using a retired name is then a load-time error). Narrowing is not safe either: a narrower profile protects
less where it is used in `!profile:` or `protect`. Each built-in profile has
a golden match set over fixtures; any change to it fails CI unless the
profile was renamed.

### Liveness contracts

Which liveness contract each decided profile has, and its verdict under
the admission rule of [safety.md](safety.md#producer-liveness): a built-in
source without a sound, producer-equivalent, verified-heuristic, tolerant
or delegated contract is not admissible. The rule applies to every
built-in, shipped ones included (Mark, 2026-10-23); TASK-122 classifies
those not listed here, and a built-in still of class *none* at a release
is withdrawn or given a contract first. This table is the single home for
these decisions (TASK-112). Rows marked *planned* name a contract that is
designed but not implemented.

| Profile | Producer (verified) | Contract | Class | Verdict |
|---|---|---|---|---|
| `nix-shell-rc` | legacy `nix-shell` and `nix develop` (Nix 2.34.8 on the host, 2.34.9 pinned by `nix_shell_rc_lifecycle_is_pinned`): the directory's pid is the shell's own for both, interactive and `-c`; `nix develop`'s directory is referenced by nothing after start; legacy `nix-shell`'s is the `TMPDIR` of the shell and its children and its rc trap removes it except after `SIGKILL` (safety.md) | `liveness_contract: nix-shell`, `min_idle_for: 1h` | sound, under Owner PID's preconditions | Shipped (TASK-112 step 4) after the lifecycle verification (both commands, interactive and `-c`, normal exit, `SIGHUP` and `SIGKILL`, a detached child outliving the shell, a shell in a private PID namespace sharing `/tmp`), with its vectors and container tests. Matching set as in the library row. Small in bytes, large in count. Residual: a consumer that outlives the shell and keeps only a path string (Owner PID's scope). `min_idle_for: 1h` narrows that residual and is not part of the contract's soundness: a detached child that wrote in the directory within the hour keeps it `not_idle`. |
| `pytest-tmp` | pytest 9.1.1 (source read; container tests planned) | planned: `liveness_contract: pytest`, `min_idle_for: 1h`, `checkouts: allow`; one candidate per session | verified heuristic: lock rows sound; the rank row producer-equivalent or stricter (it never permits a session pytest's own next cleanup, with the default retention, would keep; while a pytest of the same user runs it needs a session older than pytest's dead-lock age, Mark 2026-10-23); the unused-root row heuristic, and it does remove sessions above the rank that pytest keeps | Admitted by Mark, 2026-10-23, with the residuals in safety.md (pytest sessions); ships when implemented with its vectors and container tests. |
| `node-modules`, `python-venv` | npm: to be verified by the planned container test (an install from an offline tarball, a read-only run); pnpm, Yarn, Bun, venv, virtualenv and uv: planned, not verified; consumers are `node`, `python` and editors | planned: `liveness_contract: node-project` or `python-project`, `min_idle_for: 30d`, `checkouts: refuse` | verified heuristic: project activity | Admitted by Mark, 2026-10-23, with the residuals in safety.md (Project dependency directories); the loss in a residual is a reinstall, except hand-edited dependency files, a dated exception to the admission rule (Mark, 2026-10-23; remedy `.diskgc-keep` or `protect`). Nested dependency directories are never candidates, so a removal cannot silently switch Node to a hoisted copy. The matching set is fixed before the names ship. Ships when implemented with its vectors and container tests. |
| `nix-shell-tmpdir` | `nix develop`'s rc runs `mktemp -d -t nix-shell.XXXXXX` and exports it after `exec`; no exit cleanup (left after a normal exit, Nix 2.34.8) | none: the name records no owner; a dormant shell shows no use | none | Excluded. Counterexample: the dormant-temp-users container test. The class is fixed at the producer, upstream (TASK-123). |
| `mktemp-dir` | any program | none | none | Excluded. Counterexample: a shell that exports `TMPDIR=$(mktemp -d)` after `exec`. |
| `tmp-entry` | any program | none | none | Excluded for good: there is no one producer, and every counterexample above applies. |
| `containers-image-tmp` | containers/image and Buildah spools in `/var/tmp`; the real producers' lifecycle is unverified | `min_idle_for: 3d` only | none | Withdrawn (Mark, 2026-10-23). Counterexample: `vm_containers_image_dormant_spool_exposes_liveness_gap`. Re-admitted only under a new name after TASK-125 verifies the real lifecycle in a container test. |
| `gradle-caches` | Gradle; its daemons lock cache entries only on demand | none for diskgc: Gradle's own retention-based cleanup owns the directory, but there is no cleaner diskgc could invoke on a measured directory, so it is not *delegated* | none | Excluded; tune Gradle's cache retention instead. |
| `maven-repo` | Maven, `mvnd`, IDEs | none: a JVM holds a jar open only while it is loaded; the repository has no lock by default | none | Excluded. |
| `nix-garbage` | the Nix store's own garbage collector | Nix's GC roots and lock (kind `nix-store`), not a `where` predicate | delegated | Governed by the `nix-store` protocol (TASK-36, TASK-118). |
| `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Chromium-family browsers, default user-data directory | `app_not_running` | sound (presence marker) | Shipped (TASK-74). |
| `cargo-target` | Cargo | the kind's lock protocol | delegated (producer lock) | Shipped. |

### Provisional 0.1 candidate release notes

The candidate package ships a static `diskgc` binary, the tmpfiles rule and
`example.yaml`. Its supported test scope is Cargo target reclaim with the
seven idle filters and checkout policy narrowing. The default development
package remains sandbox gated. The candidate's gate lift is tested only in
disposable containers and does not constitute release approval.

Watch mode, persistent index, broader survey and catalog additions, Docker
actions, Nix store actions, operator command actions, and operator-authored
filesystem/delete actions are deferred. `/tmp` admission and operator-authored
policy rules still require an explicit release decision. The
`docs/ci-pre-job.sh` example stays in the source tree; it is not installed by
the package.

| Candidate deployment question | Evidence and current limit |
|---|---|
| Package contents | The actual `release-01-candidate` derivation passes the static ELF and installed-file check; its tmpfiles rule and example config are exercised in disposable Nix and Debian containers. |
| Cargo operator journey | Those containers run the packaged, gate-lifted binary against real Cargo targets: dry run, busy-lock preservation, eligible deletion judged by the filesystem oracle, and a successful rebuild. Default and explicit `checkouts: allow`, and explicit `checkouts: refuse`, are checked on vendored checkout content. |
| Catalog and excluded actions | The packaged binary exposes only Cargo plus seven idle filters. Excluded built-ins and operator-authored command/delete policies are rejected before execution; backend spy files and the deletion oracle remain untouched. |
| Host and filesystem deployment | The candidate has no approval for an operator host or its filesystem, mount, permissions and running-work mix. Container results establish behavior only in those disposable fixtures. |
| Race and temporary paths | Cargo race protocol tests use a separate development test driver, so they do not establish packaged-binary race behavior. No `/tmp` source is admitted; temporary-path liveness and the final release catalog remain unresolved. |
| Operator policy and rollout | Operator-authored `fs`/delete and command actions are excluded; the final policy contract, installation environment and rollout evidence are unresolved. Do not deploy this candidate as a 0.1 release. |

### User profiles

- The config's `profiles:` list adds profiles. A user profile with the name
  of a built-in must say `override: true`; otherwise it is a load-time
  error, so nobody shadows a built-in by accident. `override: true` with
  no built-in of that name is also an error, to catch misspelled targets.
- To change only a built-in source's checkout guard, use top-level
  `profile_policies:` instead of replacing its definition. This preserves
  its matching set, kind, action, and built-in origin.
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
