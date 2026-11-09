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
diskgc watch  --min-free SIZE --target-free SIZE [--interval DUR] [--timeout DUR]
              [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH
diskgc watch  --tally-only [--min-free SIZE]
              [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH
diskgc check  [--config FILE]
diskgc survey [--select EXPR] [--roots DIR]... [--in-use] [--list] [--json]
              [--timeout DUR] [--config FILE] PATH...
diskgc profiles list [--json] [--config FILE]
diskgc profiles show NAME [--json] [--config FILE]
diskgc profiles test [--json] [--config FILE]
diskgc --help | --version
```

A build runs the commands of its row of the release descriptor
(`crates/diskgc-core/src/releases.json`); `survey` runs in the
development build only, and a release build refuses it with exit 2,
naming the commands it has.

A usage error (an unknown command or flag, a missing or malformed
argument) exits 2 and prints, on stderr, `diskgc: error: MESSAGE`, then
the usage lines of the command given (of every command when there is
none) and one line pointing to `diskgc --help`; it does not print the
whole help. A mistyped command, flag or `profiles` subcommand names the
closest one (`unknown flag --min-fre; did you mean --min-free?`). With
`--json`, the one JSON object carries the same message (`cause:
parse`). It is reported before anything on the machine is read, so it is
exit 2 even where the startup check (an unsupported kernel, `/proc`
unavailable: Exit codes) would fail.

`--select EXPR` replaces the config's groups with one tier selecting `EXPR`
(the config's user profiles and `protect` list still apply); the JSON
report and stderr name that tier and its group `select`. With `--select`, a config file is
optional: if none is found, `EXPR` is evaluated against the built-in
profiles alone. Without `--select`, a missing config is exit 2 as described
under Configuration file. `--roots DIR` (repeatable, one directory each)
supplies roots for `roots: required` profiles in that selection, and is
only valid together with `--select`. A root that cannot be reached safely
(`unsafe_path`: a symlink or a mount crossing on the way) is a
misconfiguration, reported once as an error that excludes the tier
([safety.md](safety.md#validation)). A supplied root that does not exist
(here, in `--roots`, or in the configuration, see Roots under
Configuration file) refuses the whole run: exit 2, before anything is
planned. Example for a CI runner:

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
excluded and reported while the other tiers remain eligible. `--select`
runs the same way, as one group of its one tier without a timeout of its
own: a command's executable is looked up only once the run needs to
reclaim, under the host lock (a tool not installed is a `missing` skip of
the tier, exit 1 when nothing else frees the space). Protected and
negated source matches are rechecked during reclaim. `ipc: live-only` remains
refused by `ensure` until its runtime check is implemented.

The `--roots` value is where the profile looks; the final `PATH` is the
directory whose filesystem must have the space. A complete pre-job hook
built on this call, branching on the exit code and keeping the `--json`
report, is [ci-pre-job.sh](ci-pre-job.sh); the CI operator's journey test
runs it as written.
An agent-oriented [pre-job hook](agent-pre-job.py) uses the JSON report to
decide, per exit code, whether to start (0), defer for capacity (1), retry
later (75, 124), abort with an alert (3), abort for an unprovisioned host
(2 with `cause: provisioning`) or a wrong call or configuration (other 2s),
or stop because diskgc was interrupted (130, 143). Its example selection
keeps an idle filter, so parallel agents' fresh targets survive each
other's pre-jobs.

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
  printed in every report. On btrfs or ZFS a reclaiming or dry run says
  once (stderr) that a deletion frees nothing while a snapshot shares its
  extents: tallies are estimates there, and only the final sample of
  available space decides the exit code (the PRD's non-goal, warned
  about). `watch` says it at start.
- `--min-free` (required): if available space is at least this, do nothing
  and exit 0, without taking a lock. `ensure` still checks that
  `/run/diskgc/lock` is a safely reachable, readable regular file. If it
  is unusable, or missing as root, stderr warns with the provisioning
  detail and the commands that provision it; exit 0 and the JSON `met`
  report are unchanged. A later run needing to reclaim exits 2 (`cause:
  provisioning`) if it is still so. Missing is no fault for a user (The
  lock, below): the met run makes no lock and says nothing. This readiness check does not
  take a `flock`. The same where diskgc cannot see every process
  (`hidepid`, a PID namespace of its own): exit 0, with one stderr line
  that a reclaim would be refused; below `--min-free` exit 2 (`safety`),
  and the dry run exit 2 whatever the space
  ([safety.md](safety.md#in-use), Visibility: a process list that cannot
  be complete).
- **The lock** a reclaim takes (the rule and its limits:
  [safety.md](safety.md#host-lock)): the host lock `/run/diskgc/lock`
  when it is provisioned; without it, for a user (never root), that
  user's per-user lock, `/run/user/<uid>/diskgc/lock` or
  `~/.local/state/diskgc/lock`, which diskgc makes and names in one log
  line; where that cannot be made for a reason of the environment, none,
  with a warning. Exit 2 (`provisioning`) and exit 3 (`host`) as there.
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
  [architecture.md](architecture.md#the-tally). `--tally-budget` bounds
  the synchronous tally that builds the run's index before the host lock
  (`fs` tiers of one source without `!profile:`; their candidates are
  then taken in the index's order, each found again and judged in full,
  [architecture.md](architecture.md#the-reclaim-walk)), including its
  sizing of a first tier ordered `oldest`, which the reclaim walk does
  under the host lock as the tier's queue reaches each candidate, and, per tier,
  the tally that orders what that tier's own discovery finds under the
  host lock (every Cargo-target tier, union and `!profile:` tier, and
  what an indexed tier's queue did not take): candidates it leaves
  untallied are taken after the tallied ones, in the order discovery
  found them, each judged in full under its own locks. A run that
  tallies starts from the index snapshot of `PATH`'s filesystem
  (`$XDG_STATE_HOME/diskgc/index-<fs>`, mode 0600) when a valid one
  exists, and writes it once its run under the host lock ends, so the next run starts
  warmer; when it is read, ignored, written or not written is in
  [architecture.md](architecture.md#the-tally) (Snapshot). Each of those
  is one stderr line (`index: ...`), and none changes the exit code
  beyond the space the write takes, which the final sample includes.
  `--no-index` neither reads nor writes the snapshot: the
  run's index starts empty and is discarded. A candidate a tally of this run refuses (e.g.
  kept, another user's, too many entries, unreadable) is skipped with
  that reason for the run and never locked; a refusal that stops being
  true later (a `.diskgc-keep` removed after the tally) is a missed
  reclaim until the next run, never an unsafe delete. When a tallied
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
  in its first tier with candidates when its tally is not cut short
  (`--tally-budget 0`) and its index holds no collapsed blocks or stored
  refusal hints (otherwise, and in later tiers, which the synchronous
  tally does not size, a reclaim's order within a tier can differ;
  [architecture.md](architecture.md#order-within-a-tier)), then the
  skipped ones; a reclaim stops at the goal, the plan does not.
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
  interrupted). A dry run that reaches the deadline (124, `status:
  deadline`) keeps the work it did: `plan` has a row for every candidate
  judged before it, and when the deadline cut the plan short, `errors`
  says so (a plan completed before a later step crossed the deadline has
  no such error). Left out
  are the candidates it found but had not judged, the one it was judging,
  and, when protect matches apply, eligible candidates not yet checked
  against them (an eligible row is never a guess). If one configured tier
  exhausts its own timeout, later
  tiers are still planned, but the incomplete grouped plan exits 3; the
  overall `--timeout` still exits 124.
- **Open-file limit.** At startup `ensure` raises its soft
  `RLIMIT_NOFILE` to the hard limit: its in-use scan holds about one
  descriptor per visible process, more than the stock soft limit of 1024
  on a busy host. The hard limit is not raised, and commands `ensure`
  runs get the soft limit it was started with. If raising fails, stderr
  warns and the run goes on. A scan that still runs out of descriptors
  fails the run (exit 3, an error naming `EMFILE` or `ENFILE`), never
  skipping candidates as `in_use` ([safety.md](safety.md#in-use), limit
  4); raise the hard limit (`LimitNOFILE=` in a systemd unit, `nofile` in
  `limits.conf`) on a host with more processes than it.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors, when stdout accepts the write. Any argument
  `--json` (or `--json=...`, a usage error) asks for it, wherever it stands,
  also where the parser takes it as another flag's value (`--config --json`).
  `--help` (or `-h`) wins over it: the help is printed as text, exit 0.

### `watch`

The contract below was settled by TASK-117 and is implemented (TASK-32),
with this limit in this build: the record's `index` block is `null`
as in `ensure`'s report (TASK-244 defines it for both).

```
diskgc watch --min-free SIZE --target-free SIZE [--interval DUR] [--timeout DUR]
             [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH
diskgc watch --tally-only [--min-free SIZE]
             [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH
```

`watch` is the long-running form of `ensure`: it samples one filesystem
and runs the same reclaim walk ([architecture.md](architecture.md#the-reclaim-walk))
whenever available space falls below `--min-free`, with its own live
index kept fresh by the background tally
([architecture.md](architecture.md#the-tally)). It does not daemonize:
it runs in the foreground under a supervisor (systemd, a shell, a CI
runner's service manager); systemd is not required.

#### Watch flags

- `PATH`, `--select`, `--roots`, `--config`, `--allow-root`: as for
  `ensure`. `PATH` is resolved once, at start; its filesystem is the one
  watched for the life of the process (The reclaim cycle).
- `--min-free` (required, except with `--tally-only`): a reclaim cycle
  starts when a sample is below it.
- `--target-free` (required, except with `--tally-only`, where it is a
  usage error): the cycle's goal. Required because a watcher without two
  thresholds would reclaim a sliver at every poll; the error message says
  so. Equal to `--min-free` is allowed and means exactly that.
- `--interval` (default `10s`, from `1s` to `1h`; anything else is a
  usage error): the period of the space sample, one `statvfs`.
- `--timeout` (default `10m`, from `1m` to `24h`; `0` and anything else
  are usage errors, since an unbounded cycle would leave a service
  waiting forever on a lock held by an orphaned child): the cooperative
  deadline of each reclaim cycle, from the sample that triggered it,
  including the wait for the host lock; the same rules as `ensure`'s
  `--timeout` ([safety.md](safety.md#deadlines)). Per-tier `timeout`s
  apply inside it; a tier whose `timeout` is longer is cut by the cycle's
  deadline, and the start logs one warning per such tier. There is no
  deadline on the process.
- `--json`: JSON Lines on stdout, one object per reclaim cycle (Watch JSON
  lines). The human log stays on stderr either way.
- `--tally-only`: never reclaim (Tally only). `--interval`, `--timeout` and
  `--target-free` are usage errors in this mode, since nothing samples
  or reclaims; `--min-free` is optional and only bounds the snapshot
  write.
- Not accepted: `--dry-run` (preview a cycle with `ensure --dry-run`
  and the same thresholds), `--tally-budget` and `--no-index` (the live
  index replaces the synchronous tally).
- Sizes and durations as under [Sizes and durations](#sizes-and-durations);
  a percentage is resolved against the filesystem's size at start (at the
  first sample that works, when the start's failed); the snapshot write's
  guard resolves `--min-free` at each write instead
  ([architecture.md](architecture.md#the-tally), Snapshot).

**One process per filesystem and state directory.** The snapshot's
single-writer lock `index-<fs>.lock` ([architecture.md](architecture.md#the-tally),
Snapshot) lives in the state directory (`$XDG_STATE_HOME/diskgc`) and is
held for the process's lifetime, whichever the mode, so a second `watch`
(or `watch --tally-only`) with the same state directory on the same
filesystem exits 2 at start. The rule protects warmth and tally load,
not safety: a second watcher with another state directory runs, and the
host lock (or the user's per-user lock) still serialises its reclaims.
The start line names the state
directory. Two `PATH`s on one filesystem, or on two btrfs
subvolumes of one filesystem, are one filesystem
([architecture.md](architecture.md#filesystem-matching): by the device
mountinfo names, never a subvolume's own): they share available space
and one snapshot key, so one watcher serves both and a second exits 2.
Watchers of different users or different filesystems run side by side; their reclaims are serialised by
the host lock ([safety.md](safety.md#host-lock); without it, only each
user's own reclaims are, by the per-user lock) and their background
tallies' duty cycles add up.

**Configuration is read once.** Edits take effect on restart; there is no
reload (Watch signals: SIGHUP stops `watch`). Validate first with
`diskgc check`, then restart the service.

#### Watch start

In this order; the first failure exits with its code and deletes nothing:

1. Command line, configuration and the `--select` expression: exit 2 as
   for `ensure` (Configuration file, Selection grammar). Nothing on the
   machine beyond the configuration file is read for this step, so a bad
   configuration (what `check` reports) or expression is exit 2 even
   where the next step would fail. A configuration file that cannot be
   read for a reason of the host (`/proc`, through which the checked file
   is reopened, unavailable; an I/O error; diskgc out of descriptors or
   memory) is exit 3 (`host`), as the next step's failures are; one that
   is missing, not a regular file, too large or refused by its
   permissions is exit 2 (`config`).
2. Kernel and `/proc` requirements ([architecture.md](architecture.md#deployment)):
   exit 2 when unsupported (a kernel older than 5.8), 3 when one of their
   operations fails (`/proc` cannot be opened or is not this process's
   pid namespace's, `pidfd_open` refused). Raise `RLIMIT_NOFILE` as
   `ensure` does (Open-file limit under `ensure`).
3. Running as root without `--allow-root`, or `--allow-root` not as root
   or without `CAP_SYS_PTRACE` ([safety.md](safety.md#privileges)): exit 2
   (`safety`); a capability set that cannot be read: exit 3 (`host`). The
   configuration's checks that depend on the user diskgc runs as
   (`owners:` names, `protect` and tier roots with `{uid}`): exit 2. A
   tier whose kind this build does not reclaim: exit 2 (`usage`), as
   `ensure` refuses it before its gate (TASK-291). In a
   development build, the pre-release sandbox gate: exit 2 outside a test
   container ([safety.md](safety.md#pre-release-sandbox-gate)); not with
   `--tally-only`, which deletes nothing.
4. `PATH` (absolute, reached safely, [safety.md](safety.md#path-traversal)):
   exit 2 when invalid, 3 when an operation fails before `PATH` is open.
   The first space sample is not a start check: if it fails, it is the
   first cycle (The reclaim cycle). Then, without `--tally-only` and when
   a tier relies on the in-use check: its visibility
   ([safety.md](safety.md#in-use), Visibility), exit 2 (`safety`) or 3
   (`host`).
5. The lock its cycles take ([safety.md](safety.md#host-lock)): one its
   cycles could never take (the host lock not a regular file or missing
   as root, an unsafe per-user lock) exits 2 (`cause: provisioning`,
   with what to fix), since a watcher that can never reclaim must fail
   where its operator looks, not log forever. A user's per-user lock is
   made here and named in one line (a warning when it cannot be made:
   the cycles then run without a lock while it cannot). No `flock` is
   taken. `--tally-only` makes nothing: it names the lock it polls, or
   warns about a host lock it cannot use (a missing one reads as not
   held).
6. Supplied roots that do not exist: exit 2, as for `ensure`.
7. The single-writer lock: waited for at most 10 s (an `ensure` holds it
   only while writing); still held exits 2 (`cause: usage`, naming the
   other process's filesystem key and the state directory). Failing to
   create the state directory or the lock file (`ENOSPC` on a full
   `/home` is the likely cause) is not fatal: the snapshot is a cache, so
   `watch` logs one line, runs without loading or writing a snapshot,
   and tries to create and take the lock again at every 5-minute
   snapshot tick (a lock then found held is one log line, never an
   exit).
8. Load a valid snapshot as the seed of the live index
   ([architecture.md](architecture.md#the-tally), Validity), start the
   background tally, and take the first sample at once.

With `--json`, a start failure prints one object as every command does on
an error (Output: `status`, `exit_code`, `errors`). A start that succeeds
prints nothing to stdout; stderr gets one line naming the mount, the
thresholds, the interval, the configuration path and where the index
started.

#### The reclaim cycle

Every `--interval`, one `statvfs` of the filesystem `PATH` resolved to at
start (through the descriptor `watch` keeps open, so a mount later
stacked on `PATH` does not change what is watched):

- **At or above `--min-free`:** nothing. No lock, no deletion, no stdout,
  no log line. The background tally keeps its duty cycle.
- **Below `--min-free`, not in back-off:** one **reclaim cycle**: the
  reclaim walk with `min = --min-free`, `goal = --target-free`, the
  cycle's deadline, the live index, the configuration's tiers (or
  `--select`). Each cycle repeats the run-time checks `ensure` makes before
  its first deletion (roots, executables, backend binding, `owners:`
  names; [safety.md](safety.md#validation)) and shuffles `ranking: random`
  groups with a fresh seed, as each `ensure` would, since the machine changes under a
  long-running process; it also re-resolves `PATH` and requires the same
  device as at start (another one is a cycle error, `host`, and nothing
  is deleted). The background tally pauses while the lock this user's
  reclaims take (the host lock, or the per-user lock) is held (by this
  cycle or another reclaim) and resumes after it. Samples
  are not taken during a cycle (the walk samples around every action);
  the next one is at the first tick after it ends.
- **A sample that fails** counts as below `--min-free`. Outside back-off
  it is a cycle that ended at its first sample (status `error`, `cause:
  host`, so back-off `retry`, with its record and log line); in back-off
  it does nothing more, so a filesystem that stays unreadable costs one
  record per back-off step, not one per interval.
- **Below `--min-free`, in back-off:** no cycle until the back-off ends.
  Sampling continues at every `--interval`: it is what ends a back-off
  early.

A cycle on a busy host backs off between checks as `ensure` does, and
holds the lock (the background tally paused) until the host settles or
the cycle's deadline ([safety.md](safety.md#in-use), Freshness, Busy
host); a cycle that ends at that deadline is `deadline`, class `retry`
below.

The cycle's outcome is the status `ensure` would report for the same run
(Exit codes (`ensure`), JSON `status`): `met`, `exhausted`, `error`,
`lock_timeout`, `deadline`, `interrupted`, or `usage_error` (only `cause:
provisioning`: the lock became unusable after start,
[safety.md](safety.md#host-lock); or `cause: safety`: the process list
stopped being complete after start, [safety.md](safety.md#in-use),
Visibility; start-time checks catch the rest). It never ends the process.

A supplied root that existed at start and has since vanished is not the
refusal `ensure` makes for a root that does not exist (that guards
against a mistyped root before anything runs). In `watch` it is one
`missing` skip row of its tier (candidate: the root), the other tiers
run, and the status follows available space as for any skip: a deleted
project directory must not stop every later reclaim until a restart.
Nothing could be found there, so skipping it cannot let a later tier run
ahead of eligible candidates.

**Back-off** after a cycle that ends below `--min-free` (every status but
`met` and `interrupted`). Each cycle is classified:

| Class | When the cycle had | Why |
|---|---|---|
| `retry` | any of: an `errors[]` entry with cause `host`, `operation` or `provisioning`; a `backend_unavailable`, `changed` or `in_use_unknown` skip; status `lock_timeout` or `deadline` | the environment may recover without a restart: a daemon comes back, a lock holder finishes, a busy host calms down, a slow walk continues from a fresher index |
| `hold` | none of those: status `exhausted`, or only errors with cause `config`, `safety` or `usage` (a tier excluded as misconfigured) | nothing changes until space drops further or files age into eligibility; configuration changes need a restart |

The next cycle starts at the first sample, below `--min-free`, that meets
any of:

1. available space at or below the cycle's final sample minus
   `max(1 GiB, 1 % of the filesystem)` (new cruft has probably appeared);
   not applicable when the final sample failed or was below that step
   (a full filesystem cannot drop by it);
2. 15 minutes after the cycle ended;
3. class `retry` only: 1 minute after the cycle ended, doubling with each
   consecutive `retry` cycle (1, 2, 4, 8 minutes), capped by rule 2.

A sample at or above `--min-free` ends the back-off and resets the
doubling (one log line: space recovered); so does a `met` cycle. A mixed
outcome (an exhausted Cargo tier and an unreachable Docker daemon) is
therefore `retry`: the shortest applicable wait wins, and its cost is
bounded by the doubling. The constants are fixed in code and tentative
(PRD, Irreversibility map); the classes and the rule that sampling
continues are the contract. One log line per back-off: its class, the
causes that chose it, and both resume conditions.

#### Watch JSON lines

With `--json`, stdout carries [JSON Lines](https://jsonlines.org/): each
reclaim cycle, when it ends, writes exactly one line, one compact JSON
object terminated by `\n` and flushed. The object is the `ensure` report
(Output: same fields, enums and rules) for that cycle, with these
differences:

- `exit_code` is `null`: the process does not exit. Scripts branch on
  `status`.
- `index.source` is `live`; `dry_run` is `false`; `plan` is `[]`;
  `started_at` and `duration_ms` are the cycle's; `tally_cut_short` is
  `false`.
- `index.age_seconds` is the age of the least recently listed selected
  (profile, root): seconds since its last discovery listing, or `null`
  while one has not been listed since start.
- One more top-level field, `watch`, present only in `watch` records
  (never in an `ensure` report), shown here with `backoff` set; it is
  `null` where described:

  ```json
  {"watch": {"cycle": 1, "backoff": {"class": "retry | hold", "attempt": 1, "wait_seconds": 60, "resume_below_bytes": 0}}}
  ```

  `cycle` counts reclaim cycles from 1 since start. `backoff` is `null`
  after a `met` or `interrupted` cycle; otherwise `wait_seconds` is when
  rules 2 and 3 next allow a cycle, `resume_below_bytes` is rule 1's
  level (`null` when rule 1 cannot apply: the final sample failed, or it
  was below the step, so available space cannot fall by it), and `attempt` counts
  consecutive `retry` cycles (`0` for `hold`).

What is not a record, and why:

- **No record when nothing happens.** A sample at or above `--min-free`
  is not an event; at the default interval it would be 8,640 lines a day
  that say nothing. The stream holds what diskgc did and why, and its
  silence means space was there.
- **No heartbeat.** Whether `watch` is alive is the supervisor's
  knowledge (process state, `systemctl is-active`); a heartbeat in the
  data stream would mix liveness into the record of actions.
- **No start or stop record.** A refusal prints the error object
  every command prints (it has a non-null `exit_code`); a successful start
  and a stop are stderr log lines. A cycle a signal stopped is a record
  with status `interrupted`.

Every line is therefore a cycle report, except a refusal object
(Exit codes (`watch`)): a start failure's, as the only line, or the one
after start, as the last line. Consumers ignore unknown fields; a future kind of
record would be told apart by a new top-level field, never by reusing
these. A failed stdout write (a closed pipe, a full disk under the
redirect) exits 3 at once, as for every command: the machine interface
can no longer be honoured, and a supervisor restarts the process with a
working stdout. This is the one environment fault that ends `watch`
after start, a deliberate exception to "keep running" (TASK-117, open
for Mark's confirmation): a consumer of the stream would otherwise miss
deletions without knowing. Without `--json`, stdout stays empty.

#### Watch log

stderr only; the line format is tentative (PRD) and not a contract;
scripts use `--json`. journald or the supervisor adds timestamps.

- start: one line (Watch start); without PSI, one line saying the tally's
  pressure brake is off ([architecture.md](architecture.md#the-tally));
- a cycle: one line when it starts (available space, `--min-free`, goal,
  cycle number), then what `ensure` logs on stderr (one line per action,
  tier search lines, child output with its prefix; Output), then one line
  when it ends: status, space before and after, how many reclaimed and
  kept, then each source term's block as the human result shows it
  (`by profile: NAME [TIER]: reclaimed N (observed change ...) · kept N
  REASON SIZE, ...; NAME ...`, Output), the kept counted and sized by
  reason, and each profile skipped without a candidate (`skipped profile
  NAME [TIER]: REASON`) (one line in all, not one per candidate: a
  watcher that is `exhausted` every 15 minutes must not flood the
  journal);
- back-off: one line when it starts, one when space recovering ends it;
- the background tally: one line when the pressure brake engages and one
  when it releases, each with the number of batches since the previous
  such line; one when it pauses for the lock (naming a per-user lock) and one when it
  resumes; when it does not let go of the live index within 1 s (a turn
  blocked in the kernel), one line, and the cycle runs without the index
  (each tier's discovery alone) or the snapshot write is skipped; a
  skipped or failed snapshot write: one line
  ([architecture.md](architecture.md#the-tally), Snapshot);
- stop: one line naming the signal, one when the process exits.

A failed stderr write is ignored (the log is best effort; the process
keeps protecting the disk).

#### Watch signals

- **SIGTERM, SIGINT and SIGHUP** stop `watch` the same way. In a cycle:
  as `ensure` stops (Signals): no further candidate starts, a running
  child gets SIGTERM and SIGKILL after 10 s, the host lock is released,
  and the cycle's record (status `interrupted`) is written; the wait for
  the host lock is part of a cycle: it ends at the signal, and its
  record (status `interrupted`, nothing deleted) is written. In back-off
  or between samples: at once, no record. The
  background tally stops at its batch boundary (at most 50 ms). Then the
  snapshot is written (Snapshot: same space guard, at most once) and the
  process exits 0: a stopped watcher is not a failure, for a supervisor
  or a shell.
- **SIGHUP is not a reload.** It stops like SIGTERM, with a log line
  saying that diskgc does not reload and that the configuration is read
  at start; under a supervisor that restarts the service (systemd
  `Restart=always`) a SIGHUP therefore applies an edited configuration by
  restart. Stopping rather than ignoring it is what a hangup does to a
  foreground program, so a `watch` started in a terminal ends with it,
  and nothing keeps running in a state its operator did not ask for. If
  SIGHUP was ignored when `watch` started (`nohup`), it stays ignored.
- **A second SIGTERM, SIGINT or SIGHUP** during the stop skips the
  snapshot write (its temporary file is never linked in) and the rest of
  a running child's 10 s SIGTERM grace (the child is left running with
  the host lock it inherited, never killed unawaited), and exits 0 at
  once. The cycle's record (status `interrupted`, the child's action an
  error saying it is still running) is still written. The child's output
  pipes close with `watch`, so a child that writes output after it may
  end then (`SIGPIPE`); one that does not keeps running, and keeps the
  host lock, until it exits. It does not keep the candidate's own lock,
  which diskgc holds and never passes on, as when diskgc is killed. The
  child always gets its SIGTERM first: a second signal only cuts its
  grace short (also one that came before the SIGTERM was sent), and a
  child whose action failed outside a signal or deadline is still killed
  and awaited. `ensure` counts the second signal and ignores it. SIGKILL at any point is safe: deletions are crash-safe (the kill
  points of [testing.md](testing.md#controlled-pause-and-kill-points))
  and a child keeps the host lock until it exits
  ([safety.md](safety.md#host-lock)); only the snapshot write is lost.
- During start (the single-writer wait): exit 0 at once, writing nothing.
- No other signal is handled; SIGPIPE is ignored, so a closed stdout is
  a write error (Watch JSON lines), not a silent death.

#### Exit codes (`watch`)

| Code | Meaning |
|---|---|
| 0 | Stopped by SIGTERM, SIGINT or SIGHUP. |
| 2 | Refused at start: usage, configuration, privilege (`safety`), the sandbox gate, a lock its cycles cannot take (`provisioning`: [safety.md](safety.md#host-lock)), a supplied root missing, or another `watch` holding the filesystem's single-writer lock. Nothing was deleted. Or refused after start, once: a percentage `--target-free` that, resolved at the first sample that works (the start's failed), is below `--min-free` (`cause: usage`; the cycles before it, if any, each ended at its failed sample and deleted nothing). |
| 3 | A host operation failed at start before `PATH` was open (`/proc`, the kernel checks), or writing stdout failed at any time. |

Each refusal, at start or after it, prints with `--json` the one object
every command prints on an error (Output: `status`, `exit_code`,
`errors`).

A cycle's outcome never ends the process: environment errors (a backend
down, the host lock held, a sample that fails, also the first one; a
state directory that cannot be written) are cycle records, back-off or
log lines, as above. `watch` exits with no other code; a panic (an
internal error: an assertion that failed) exits 101, and the supervisor
decides whether to restart.

#### Tally only

For CI runners, so that `ensure` in the pre-job hook starts from a warm
snapshot instead of a cold synchronous tally.

- Runs the background tally over the configuration's tiers (or
  `--select` and `--roots`, which should be the pre-job hook's own:
  units of profiles it does not tally are not in the snapshot) and
  writes the snapshot every 5 minutes and at stop. Never samples for a
  goal, never takes the host lock, never deletes (no `Delete` port call
  at all), so the sandbox gate does not apply.
- Holds the single-writer lock like `watch`; the pre-job `ensure` reads
  the snapshot as its seed and, finding the lock held, skips its own
  write (one log line; [architecture.md](architecture.md#the-tally)).
- While any reclaim holds the host lock (the pre-job `ensure`), the tally
  pauses, so it never competes with the reclaim it serves. (Without the
  host lock it pauses only for its own user's per-user lock; the system
  template requires the host lock.)
- The snapshot write's space guard uses `--min-free` when given (pass the
  pre-job's `--min-free`: the write never takes the filesystem below
  it); without it, only a write that fails (`ENOSPC`) is skipped.
- Lifecycle: run it as a service for the life of the runner, like
  `watch`; it needs no stop between jobs (the pressure brake and duty
  cycle yield to the job). A runner that must stop it around jobs
  (`systemctl stop` in a post-job hook) loses nothing: a stop writes the
  snapshot. On an ephemeral runner there is nothing to warm.
- Its JSON stream is empty (no cycles); `--json` only makes a start
  failure machine-readable.

#### Watch beside `ensure`

- **Reclaims** take turns through the host lock (without it, through the
  user's per-user lock: only one user's reclaims, and only those that
  resolve the same per-user lock, [safety.md](safety.md#host-lock)). A cycle that waits past
  its `--timeout` ends `lock_timeout` (`retry`); one that gets the lock
  re-samples and is `met` without deleting if the other freed enough. An
  `ensure` waiting on a long `watch` cycle (a Nix GC) can exit 75.
- **The index:** `watch` owns the snapshot; `ensure` seeds from it and
  skips its write. What `ensure` deletes leaves stale units in the
  watcher's live index, which its re-checks and the walk's re-match
  (`stale`) absorb: a cost in speed, never in safety.
- **Thresholds:** an agent's `ensure --min-free` above the watcher's
  `--target-free` makes the agent reclaim what the watcher would not;
  set the watcher's `--target-free` at or above the largest `--min-free`
  the agents ask for so that their `ensure` stays an instant no-op.

#### Watch as a systemd service

The development package and the `release-candidate` package (a package
ships them when its release runs a `watch`: the release descriptor, [Built-in
profile library](#built-in-profile-library)) ship the two templates of
[architecture.md](architecture.md#the-watch-units):
`lib/systemd/user/diskgc-watch@.service` and
`lib/systemd/system/diskgc-watch@.service`, whose `ExecStart` runs
`/usr/local/bin/diskgc`.

**Which package.** A reclaiming instance needs a release package: 0.2
runs a reclaiming `watch` and `watch --tally-only` (Mark, 2026-11-05,
TASK-276; TASK-362), with the 0.2 catalog only, as its `ensure`
([Built-in profile library](#built-in-profile-library)). The
development package refuses to delete outside test containers
([safety.md](safety.md#pre-release-sandbox-gate)): its reclaiming
`watch` exits 2 at start naming the sandbox gate, and the unit stays
failed; outside test containers it supports only `--tally-only`
instances (Tally only), such as the CI runner's below. A
`release-candidate` package built from master is a development version
for disposable test environments only (PRD, Safety boundaries). The
steps are the same for every package.

Install once, as root, from the package:

```sh
install -Dm0755 bin/diskgc /usr/local/bin/diskgc
install -Dm0644 lib/tmpfiles.d/diskgc.conf /etc/tmpfiles.d/diskgc.conf
systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf
install -Dm0644 lib/systemd/user/diskgc-watch@.service /etc/systemd/user/diskgc-watch@.service
install -Dm0644 lib/systemd/system/diskgc-watch@.service /etc/systemd/system/diskgc-watch@.service
```

`systemd-tmpfiles --create` makes the host lock now; at every boot
systemd does it. The tmpfiles steps are needed only for the system
template (which does not start without the host lock) and for root; they
let every user's reclaims take turns. Without them a user's watcher and
`ensure` work all the same, on a per-user lock
([safety.md](safety.md#host-lock)). A user may instead keep the user
template in `~/.config/systemd/user/`, and needs no root at all then.

**Laptop owner**, as themselves, from the package. `watch` needs a
configuration (the example's Cargo root replaced with directories you
own: a supplied root that does not exist is exit 2). The instance name
is the watched `PATH` escaped by `systemd-escape --path` (`/home` is
`home`), and the thresholds, which must fit the filesystem (`--min-free`
below its size), go in that instance's environment file:

```sh
install -Dm0644 share/doc/diskgc/examples/example.yaml ~/.config/diskgc/config.yaml
# replace the example's Cargo root with directories you own, then:
mkdir -p ~/.config/diskgc/watch
echo 'DISKGC_WATCH_ARGS=--min-free 30G --target-free 60G' > ~/.config/diskgc/watch/home.env
diskgc check
systemctl --user daemon-reload
systemctl --user enable --now "diskgc-watch@$(systemd-escape --path /home).service"
loginctl enable-linger "$USER"
```

`daemon-reload` comes before the first `enable`, so that the running
manager loads the newly installed template.
The user manager runs only while its user has a session; lingering keeps
the watcher running after logout, for overnight agents. Without the
environment file `watch` has no thresholds: it exits 2 naming
`--min-free` and the unit stays failed (`systemctl --user status
diskgc-watch@home.service` shows diskgc's message). Exit 2 is never
restarted; any other end but `systemctl stop` (a SIGHUP's 0, 3, a
panic) restarts after 30 s. After editing the
configuration, `systemctl --user restart` the instance (Watch signals:
diskgc does not reload).

**CI operator**, as root, per watched filesystem: a `User=` drop-in (an
instance without one runs as root without capabilities and exits 2), the
environment file in `/etc/diskgc/watch/`, and the configuration in
`/etc/diskgc/config.yaml` (the system template sets `DISKGC_CONFIG`):

```sh
mkdir -p '/etc/systemd/system/diskgc-watch@srv-ci.service.d' /etc/diskgc/watch
printf '[Service]\nUser=gitlab-runner\n' > '/etc/systemd/system/diskgc-watch@srv-ci.service.d/user.conf'
echo 'DISKGC_WATCH_ARGS=--tally-only --min-free 50G' > /etc/diskgc/watch/srv-ci.env
systemctl daemon-reload
systemctl enable --now diskgc-watch@srv-ci.service
```

On NixOS, the module installs and starts the instances instead
([architecture.md](architecture.md#nixos-module): its options, and what
a switch starts).

### `survey`

```
diskgc survey [--select EXPR] [--roots DIR]... [--in-use] [--list] [--json]
              [--timeout DUR] [--config FILE] PATH...
```

Read-only: it deletes nothing, takes no lock, runs no command action and
reads or writes no index snapshot, so it runs anywhere, also outside the
test containers (the sandbox gate guards deletion only). For each `PATH`
it reports what the profiles find below it, what keeps each candidate,
and what no profile matches yet, so the next profile worth writing is
visible. It is `ensure --dry-run`'s plan without a goal: every candidate
is judged by the same checks (Output: `plan`), and nothing is reclaimed
toward a size, except that the in-use check is opt-in (Mark, TASK-116,
2026-11-04: "in-use detection runtime selectable via flag"):

- **Without `--in-use` (the default)** no other process is looked at:
  no `/proc/PID` entry, no process scan, no bound sockets or loop
  devices, no Cargo process heuristics, and no refusal for `hidepid` or a
  PID namespace (it runs in a container too; diskgc still reads its own
  `/proc/self` entries and the kernel version). Skipped are the in-use
  check and every rule judged on processes: Owner PID (its
  local-filesystem rule included, so nix-shell-rc's "shell ended" and a
  pytest `.lock` holding a pid), pytest-tmp's rows 4 to 11 ("no pytest
  run seen"), the project contracts' visible-use rule, and the live half
  of a `protect_toolchains` guard (a running manager, a pin a process
  selects). Every other check runs as in a dry run, with the same
  verdict: discovery, the source's `where` (an `app_not_running` lock
  file included), keep markers, checkouts, container storage, ownership,
  special files, the eligibility walk with its sizes, the idle filters
  by age, `future_time`, version rules, `protect`, the retention guards
  (a `protect_toolchains` guard's names, recent reads and the host's
  pins among them), and a [liveness contract](#liveness-contracts)'s
  rules on files, in its order: its instance recognition (`filtered`);
  for pytest-tmp, the checks before its table
  ([safety.md](safety.md#producer-liveness), pytest sessions: the
  `pytest-of` directory's owner and the temp root, `producer_unknown`;
  the session reached again, `changed` or `unreadable`) and rows 2 and 3 (a
  listing that failed, `unreadable`; a `.lock` that is no pid marker,
  `producer_unknown`); for a project contract, its location
  (`producer_unknown`) and "no change or read for 30 days" (`not_idle`).
  In a union, every term that found the candidate vetoes by those
  contract rules, as with `--in-use` (TASK-343). A candidate
  that passes all of them is **not checked**, never eligible: the human
  result says `N matched (in use not checked)`, `--list` says `not
  checked`, and the JSON has `in_use: "not_checked"`, the count under
  `summary[].not_checked` with `eligible: null`, and each such row's
  `eligible: null`. The kept reasons are those of the checks above;
  `in_use` then comes only from an application's lock file.
- **`--in-use`** judges candidates on in-use and liveness evidence as
  `ensure --dry-run` does: one process scan, reused while young
  ([safety.md](safety.md#in-use)), and the same refusal where diskgc
  cannot see every process. A candidate that passes is eligible, one
  that does not is kept `in_use`, `in_use_unknown` or `producer_unknown`.
  On a busy host it waits as a dry run does ([safety.md](safety.md#in-use),
  Busy host): a candidate is judged once the host settles, for up to
  `--timeout` (with `--timeout 0`, until a signal). It costs `/proc` scans of every visible process (one per reuse window
  and profile, and one at the end of each profile's plan for its notes on
  what it could not see). On the development laptop the scans were a
  small part of a survey's time: walking the trees is most of it.

Only a survey can plan without the in-use check: the option is a value
only the survey's own code can make, so `ensure`, its dry run, a reclaim
and `watch` always check.

- **Per profile:** one line (one JSON `summary` block): candidates found,
  how many are eligible (with `--in-use`) or not checked (without) and
  their estimated size, the kept ones by reason
  with their sizes (`in_use` among them), and the idle ages of the eligible
  or not checked ones (`idle 2 h to 30 d`: the newest change in an `fs` candidate's tree,
  a Cargo target's last build). Profiles that found nothing share one
  line, as do the profiles not looked for below `PATH` (below); a profile
  refused before it could look (`missing`: its command's executable is
  not installed) has its own.
- **`--list`** (always in `--json`): the candidates themselves, one line
  each, largest first, with size, verdict (`eligible`, `not checked` or
  the skip reason)
  and idle age, so a human reviews concrete paths, not globs. At most
  1,000 per profile are listed; the rest are counted (`... N more not
  listed`, JSON `left_out`).
- **Unmatched top-level entries** of `PATH`, those that are not a
  candidate and hold none, grouped by **name family**: the name with a
  trailing random-looking suffix after `.`, `-` or `_` (five or more
  letters and digits, at least one a letter, after a separator that is
  not the name's first character) replaced by `*`, then each run of
  digits replaced by `#`, e.g. `nix-shell.8raHjU` → `nix-shell.*`,
  `nix-shell-1144586-3257819220` → `nix-shell-#-#`, `.cache` stays
  `.cache`. Each entry is sized as the tally sizes a unit (allocated
  bytes, each inode once, no symlink followed, no mount crossed); a tree
  the eligibility walk would refuse as it is (more than 2,000,000 entries,
  an unreadable or `systemd-private-*` entry inside) is counted
  `unsized`. The human result shows the 20 largest families with their
  size, idle ages and an example; the JSON has every family. Mount points
  below `PATH` (another filesystem) and `systemd-private-*` names (never
  statted) are not entries. The grouping is a hint for people; which
  families a name falls into may change between releases (it is the
  index's collapse rule, [architecture.md](architecture.md#the-index)).

Which profiles run below a `PATH`: without `--select`, every source
profile of kind `fs` or `cargo-target`, built-in and user, one tier each,
in the library's order; `PATH` is the root of each `roots: required`
profile (so `diskgc survey ~` finds Cargo targets and caches below the
home directory), and a profile with fixed roots runs only for its roots
at or below `PATH` (`/tmp`'s profiles under `diskgc survey /tmp`, not
under `diskgc survey ~`). The backend kinds (`docker-*`, `podman-*`,
`nix-store`) keep their stores elsewhere and run only when `--select`
names them. With `--select EXPR`, the one tier `EXPR` selects, its
`roots: required` profiles bound to `--roots DIR` (repeatable, only with
`--select`) or else to `PATH`; a root outside `PATH` is not searched
there (one log line), and a selection none of whose roots is left is not
surveyed there (`not_surveyed`, below). Each `PATH` is absolute and is
taken without a trailing `/` or `.` parts (`diskgc survey ~/` is `~`); a
`..` in it is a usage error. A configuration file is optional: its user
profiles and `protect` list apply (protected candidates are kept
`protected`), its groups do not. Several `PATH`s are surveyed one after
the other; a filesystem shared by two of them is reported under each.

Each profile is planned on its own: one profile's error (an unsafe root,
a listing or scan the host failed) is reported with its profile and stops
no other. A failed profile is never shown as finding nothing: it has no
summary line or block, and the human result says `profile NAME: could
not be surveyed: WHY`.
`--timeout DUR` (default `10m`, `0`: none) bounds the whole survey, as
`ensure`'s does: the plan stops judging at the deadline (its unjudged
candidates are counted, `not_judged`; each profile not reached is an
error of its own), and unmatched entries not sized by then are
`unsized`; the status is then `deadline`. Ctrl-C (SIGINT) or SIGTERM
stops it with the result so far and status `interrupted`. A survey does
not report `held_by_deleted_files` (`ensure --dry-run` does).

A survey is refused before it looks at anything, exit 2, for what
refuses `ensure --dry-run`: a malformed command line or `--select`, an
invalid configuration, a `PATH` or supplied root that does not exist,
running as root (a survey has no root mode), and, with `--in-use`, an
in-use check that cannot see every process (`hidepid`, a PID namespace other than the
initial one: [safety.md](safety.md#in-use)). Exit codes: 0 every `PATH`
surveyed (status `dry_run`), 2 refused, 3 a host failure (`error`: a
plan's error, or a `PATH` whose entries could not be listed), 124
`deadline`, 130 / 143 interrupted. Over several `PATH`s the first of
usage error, interruption, error and deadline that any of them has
decides.

Example (the development laptop, 2026-11-04, abridged; `diskgc survey
~ /tmp` took 7 minutes there, most of it walking 146,000 `/tmp` rc dirs
and the trees below `/tmp` for each `roots: required` profile):

```
$ diskgc survey /tmp
in use: not checked (no process was looked at; --in-use checks it)
survey /tmp (filesystem /, 36.5 GiB available) in 315.6 s
  profile cargo-target: 20 found · 3 matched (in use not checked) est. 1.3 GiB · kept 2 contains_special unsized, 15 unreadable unsized · idle 57 m to 2 d
  profile nix-shell-rc: 146844 found · 145832 matched (in use not checked) est. 839.3 MiB · kept 1011 not_idle est. 5.9 MiB, 1 contains_ipc unsized · idle 1 h to 76 d
  profile node-compile-cache: 842 found · 842 matched (in use not checked) est. 1.0 GiB · idle 9 m to 69 d
  profile pytest-tmp: 387 found · 53 matched (in use not checked) est. 128.4 MiB · kept 331 future_time est. 3.5 GiB, 3 contains_special unsized · idle 19 h to 6 d
  profile jest-cache: not planned: missing
  not surveyed below a PATH (select one with --select): docker-build-cache, docker-dangling, docker-unreferenced, nix-garbage, podman-dangling, podman-unreferenced
  found nothing: bazel-output-base-anchored, brave-browser-cache, ...
  unmatched: 69419 top-level entries, est. 1.6 GiB, in 4031 name families (20 largest):
    nix-shell.*  59710 entries  est. 554.1 MiB  idle 1 m to 67 d  e.g. nix-shell.G2pnAk
    mpedersen  1 entry  est. 503.4 MiB  idle 38 m  e.g. mpedersen
    tmp.*  638 entries  est. 62.4 MiB  idle 1 d to 92 d  e.g. tmp.U9REh1N81x
    zephyr-h#e#-run.*  2578 entries  est. 10.1 MiB  idle 4 d to 11 d  e.g. zephyr-h5e5-run.00KnhMjN
    playwright-artifacts-*  1249 entries  est. 4.9 MiB  idle 12 d to 16 d  e.g. playwright-artifacts-04G9jL
    ... 4011 more families, 5228 entries, est. 128.0 MiB
```

With `--in-use` the same lines say `eligible` and add the verdicts on
process evidence, e.g. `profile nix-shell-rc: 146846 found · 145754
eligible est. 838.9 MiB · kept 17 in_use est. 116.0 KiB, 1007 not_idle
est. 5.9 MiB, 67 producer_unknown est. 340.0 KiB, 1 contains_ipc
unsized`. Errors are listed once per message, naming the profiles
(`error: deadline passed before this profile was planned (31 profiles:
...)`).
When the deadline or a signal stopped the survey, a last line says that
what is shown is partial.

#### Survey JSON

`--json` prints one object. Its schema is **versioned and may change**
between releases (Mark, TASK-116), unlike `ensure`'s report: a consumer
checks `schema_version` (now 2: 1 had no `in_use` and no `not_checked`,
and its `eligible` was never `null`) and treats a different value as a
format it does not know. Within one version fields are only added. The types
are `ensure`'s where they are the same thing: `status`, `exit_code`,
`started_at`, `duration_ms`, `version` and `errors` (Output); a `summary`
block is the dry run's (Output: `summary`), with `would_reclaim` and
`reclaimed` always `null` (a survey has no goal); reasons are the
skip-reason enum. A refusal before any plan has `paths: []` and the
error; a command line that is not understood prints only `status`,
`exit_code` and `errors`, as every command does (Output, Errors).

```json
{
  "schema_version": 2,
  "in_use": "checked | not_checked",
  "status": "dry_run | error | usage_error | deadline | interrupted",
  "exit_code": 0, "started_at": "2026-11-04T12:00:00Z", "duration_ms": 0, "version": "0.2.0",
  "paths": [{
    "path": "/tmp", "mount": "/", "status": "dry_run", "avail_bytes": 0,
    "summary": [{"tier": "pytest-tmp", "profile": "pytest-tmp", "found": 0,
      "eligible": {"count": 0, "estimated_bytes": 0, "unsized": 0},
      "not_checked": {"count": 0, "estimated_bytes": 0, "unsized": 0},
      "would_reclaim": null, "reclaimed": null,
      "kept": [{"reason": "in_use", "count": 0, "estimated_bytes": 0, "unsized": 0}],
      "not_judged": 0, "oldest_idle_age_seconds": null, "newest_idle_age_seconds": null,
      "listed": 0, "left_out": 0}],
    "refused": [{"profile": "go-build-cache", "reason": "missing"}],
    "not_surveyed": [{"profile": "nix-garbage", "why": "roots_elsewhere | backend"}],
    "candidates": [{"profile": "pytest-tmp", "candidate": "/tmp/pytest-of-mark/pytest-1",
      "eligible": true, "reason": null, "estimated_bytes": 0, "idle_age_seconds": null}],
    "unmatched": {"count": 0, "estimated_bytes": 0, "unsized": 0,
      "families": [{"family": "nix-shell.*", "count": 0, "estimated_bytes": 0, "unsized": 0,
        "oldest_idle_age_seconds": null, "newest_idle_age_seconds": null,
        "examples": ["/tmp/nix-shell.8raHjU"]}]},
    "errors": [{"tier": null, "profile": null, "candidate": null, "cause": "host", "message": ""}]
  }],
  "errors": [{"tier": null, "profile": null, "candidate": null, "cause": "usage", "message": ""}]
}
```

- `paths` has one object per `PATH` surveyed, in the order given (fewer
  when a signal stopped the survey). Its `status` is that `PATH`'s plan's
  (as `ensure --dry-run`'s report would say), `mount` and `avail_bytes`
  its filesystem and the available space sampled when its plan started
  (`null` when not measured); `errors` its plan's errors (Output,
  `errors`), and one of cause `host` when its entries could not be
  listed.
- `in_use` says whether candidates were judged on in-use and liveness
  evidence (`checked`, `--in-use`) or not (`not_checked`).
- `summary` has a block per profile the survey ran below that `PATH`, in
  run order, also for a profile that found nothing (`found: 0`). The
  candidates that passed every check it made are `eligible` when
  `in_use` is `checked` (`not_checked` is then `null`), and
  `not_checked` when it is not (`eligible` is then `null`): one of the
  two is always `null`. `found` counts both.
  `oldest_idle_age_seconds` and `newest_idle_age_seconds` are the range
  of those candidates' idle ages (`null` without one); `listed` and
  `left_out` say how many of its candidates `candidates` lists (at most
  1,000) and how many it leaves out.
- `refused` lists the profiles refused before they found anything
  (`reason`: `missing`, `backend_unavailable`, ...): they have no block.
- `not_surveyed` lists the profiles not run below this `PATH`:
  `roots_elsewhere` (every root outside `PATH`, also for a `--select`
  whose roots all are) or `backend` (a Docker, Podman or Nix kind, without
  `--select`). What they would match is not looked for there, and may be
  listed as unmatched.
- `candidates` are the plan's rows of a candidate, each profile's
  largest first (unsized last), then by path: `eligible: true` with
  `reason` `null` (checked), `eligible: null` with `reason` `null` (in
  use not checked), or `eligible: false` with its skip reason; `estimated_bytes` as `plan[]`'s
  (`null`: refused before its walk was complete); `idle_age_seconds` of
  an eligible or not checked row (`null` for a kept one, and when unknown or in the
  future). A root a tier could not search (`missing`, `other_fs`) is a
  row of its own, as in the plan.
- `unmatched` is `null` when `PATH` was not listed: its plan was refused,
  its mount chain is one diskgc does not look into (a FUSE, network or
  automount mount, [safety.md](safety.md#before-the-walk); one log line),
  the listing failed (an error says why), or a signal stopped the plan.
  It is empty when `PATH` is itself a candidate. A profile whose plan
  failed, or that the deadline left unplanned, has no `summary` block and
  no `refused` row, only its `errors` entry (with its `profile`); what it
  would have matched can then be listed as unmatched. `count`, `estimated_bytes`
  and `unsized` cover every unmatched entry; `families` are largest
  first, each with up to five `examples` (its largest members' paths).
  The idle ages of a family are those of its sized members.

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
  every failure, or how many examples of how many profiles passed (a
  profile without examples is not counted as passing); `--json` prints `config`, `status` (`passed | failed`),
  `exit_code`, `profiles` (the number of profiles in the library, filters
  included), and `failures` (strings). Exit 0 when
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
    "find": {"roots": ["/tmp"], "roots_required": false, "backend": null, "paths": ["zephyr-*-run.*"], "exclude": [], "type": "dir | file | any"},
    "min_idle_for": null,
    "where": {"idle_for": null, "has": null, "has_dir": null, "has_not": null, "app_not_running": null, "lacks_entry": null, "keep_newest": null, "newest_group": null, "protect_links": [], "protect_link_dirs": [], "protect_parent_link": null, "protect_cursor_extensions": null, "protect_toolchains": null, "source_anchor": null, "liveness_contract": null},
    "action": {"type": "delete | command | cargo_lock_protocol | docker_builder_prune | docker_image_prune | docker_image_prune_all | podman_image_prune | podman_image_prune_all | nix_store_gc", "make_writable": false, "command": null, "cwd": null},
    "checkouts": "refuse | allow",
    "empty_checkout_markers": [],
    "examples": {"root": null, "match": ["/tmp/zephyr-h5e5-run.jTm9zGlH"], "no_match": ["/tmp/zephyr-hal"]},
    "verified_with": null,
    "liveness_class": null,
    "releases": []
  }
}
```

- `role` says how a selection names the profile: `source` as
  `profile:NAME`, `filter` as `filter:NAME`.
- `kind`, `find`, `action`, `checkouts` and `examples` are `null` for a
  filter; `examples` is also `null` for `cargo-target`, which takes none,
  and for the backend kinds; `checkouts` is also `null` for `nix-store`,
  which takes none (a GC has no file candidates); `verified_with` is set on
  built-in sources only. `empty_checkout_markers` is a list for every kind
  `fs` source (empty but on a built-in that says it, such as `uv-cache`'s
  `["sdists-v*/.git"]`) and `null` for filters and every other kind.
- `liveness_class` is a built-in source's contract class, spelled as in
  YAML (`sound`, `producer-equivalent`, `verified-heuristic`, `tolerant`,
  `delegated`, `none`; [Liveness contracts](#liveness-contracts)), `null`
  for a filter and for every user profile. `releases` lists the release
  catalogs that include a built-in (`["0.1", "0.2"]` for one in both,
  `["0.2"]` for one only 0.2 ships, such as `nix-garbage`), and is empty
  otherwise.
- `find.roots` is always a list: the fixed roots, or empty when
  `find.roots_required` is `true` (`roots: required`: a tier or `--roots`
  supplies them) or when `find.backend` names where the kind's storage
  comes from (`"daemon"`, YAML's `roots: daemon`, for the Docker kinds,
  `podman-images` and `nix-store`; `null` for every other kind). `find.paths`,
  `find.exclude` and `find.type` are `null` for `cargo-target`.
- `where` always has its fields, `null` when unused (link lists are empty);
  `idle_for` is a duration (`3d`).
- `min_idle_for` is a source's mandatory minimum age, or `null`; it is
  `null` for filters. A tier's positive idle filters may require longer.
- `action` has every field: `make_writable` (a boolean) for `delete`;
  `command` (the argv) and `cwd` (`candidate`, `parent`, or `null` when
  it is decided per candidate: `type: any` without `cwd`) for `command`;
  neither for `cargo_lock_protocol` and `nix_store_gc` (kind `nix-store`'s
  one bounded GC). Later kinds add their own `type`.
- On an error, the object every command fails with under `--json`
  ([Output](#output), Errors): `{"status": "usage_error", "exit_code": 2,
  "errors": [{"tier": null, "profile": null, "candidate": null, "cause":
  "usage", "message": "..."}]}`, the message as on stderr. Host I/O during
  startup has `status: error` and `exit_code: 3`; invalid input and
  configuration use exit 2.

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
with `--json`; a release build says instead that it runs only these
built-ins). Before the Profiles section it says where the configuration
is looked up (Configuration file, Lookup) and where the package installs
the example configuration. It reads no configuration. `--help` and
`--version` exit 0, and print text also when `--json` is given: help wins
over `--json`.

`diskgc --version` prints `diskgc VERSION` for a release (a version
without a pre-release suffix, built only from its tag:
[architecture.md](architecture.md#release-procedure)), and `diskgc VERSION
(COMMIT)` for every other build, such as `diskgc 0.2.0-dev (9c1e331)`
from master. The flake's two shipped packages (`default` and
`release-candidate`) compile in the commit they were built from; after a
tree with uncommitted changes `-dirty` follows the commit, which then
names only the base those changes sit on. Test builds, and a plain
`cargo build` unless `DISKGC_GIT_REV` is set, print `unknown`. So
`--version` identifies a build only roughly: verification notes cite the
commit (and whether the tree was clean) themselves. The JSON `version`
field is the package version alone.

### `check`

Static validation only, machine-independent: load the config, print the
resolved config path and each tier's expression normalised with the
profiles it resolves to, exit 0 or 2 (3 if writing stdout fails). Its exit
code needs nothing from the deployment machine (no `PATH` lookups, no
filesystem access beyond reading the config, the mount table, and a lookup
of each supplied root whose mounts allow one), so packaging and
configuration tools run it before deploying (the NixOS module at build
time, also when cross-building). Everything that depends on the machine
is checked at run time; see [safety.md](safety.md#validation). One
run-time refusal is previewed, by the same decision `ensure` makes: a
supplied root (a tier's or a `protect` entry's `roots:`) that does not
exist where `check` runs is a stderr warning naming the root, the config
line and the entry, saying that `ensure` refuses the configuration there.
A supplied root on a mount `ensure` never opens (FUSE, network, an
untriggered automount, systemd PrivateTmp; [safety.md](safety.md#before-the-walk))
is not looked up and warns that it is not. The exit code is unchanged
either way, since a build sandbox has none of the deployment's roots;
where the roots cannot be inspected (no mount table, or diskgc's host
checks fail there), a warning says they were not checked.

The text output starts with `config PATH`, then one line per tier in policy
order: `GROUP.TIER: NORMALIZED_EXPRESSION => [source profiles]`. Advisory
warnings go to stderr. A user profile with `checkouts: allow` warns about
repository contents; a path pattern ending in only wildcards warns when a
fixed or tier-bound root lies outside `/tmp` and `/var/tmp`. Warnings do
not make the configuration invalid. Selection warnings are described under
Selection grammar.

An invalid configuration is reported completely in one run, by `check`
and by every command that loads it: each problem on its own line as
`PATH:LINE: message` (the line where it is written, when known), in file
order, at most 20 listed and the rest counted. A YAML syntax or schema
error (malformed YAML, an unknown or missing key, a wrong type) ends the
reading, so it is reported alone, with an excerpt of the file around it. A
rule that needs a part already refused (a tier's selection, when a user
profile does not load) is not checked, so one mistake is reported once.

`check` validates an `owners:` list's shape and rejects system UIDs; whether
it is permitted is checked when `ensure` is invoked with or without
`--allow-root`. User-name resolution remains a run-time check.

## Exit codes (`ensure`)

The first matching row wins:

| Code | Meaning |
|---|---|
| 2 | Usage, config, safety or provisioning error, the dry run's too ([safety.md](safety.md#privileges), [safety.md](safety.md#in-use): Visibility). Nothing was deleted. |
| 130 / 143 | Interrupted by SIGINT / SIGTERM. |
| 75 | Deadline reached while waiting for another reclaim, and a fresh sample is still below `--min-free` (EX_TEMPFAIL). Nothing was deleted; retry later. If the other holder freed enough meanwhile, the result is 0. |
| 0 | Final sample of available space ≥ `--min-free`. |
| 124 | Deadline reached during reclaim, below `--min-free` (also while a tier waited for a busy host to settle: the candidate it waited for is kept `in_use_unknown`, `busy`, and the ones it had not reached were not judged, [safety.md](safety.md#in-use), Freshness, Busy host); or during a dry run (its plan has every candidate judged before it, and the one it waited for a busy host to settle for, `in_use_unknown`). |
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
require a fresh sample below `--min-free`. The refusals a run meets only
below `--min-free` are not reached when the deadline passes first: the
process-list visibility refusal ([safety.md](safety.md#in-use)) is asked
right after the first sample, so only a deadline that passes before that
sample (in the sandbox gate or before it) ends the run with 124
or 0 instead; the lock's provisioning refusal is asked when the lock is
taken, after the tally. An I/O failure in the first
sample or startup host check is exit 3; an invalid `PATH` or unsupported
kernel remains exit 2. The command line, the configuration (what
`check` reports) and the `--select` expression are checked before that
startup check, reading
nothing on the machine beyond the configuration file, so a bad one is
exit 2 even where the startup check would fail; a configuration file the
host fails to read (`/proc` unavailable, an I/O error) is exit 3
(`host`), as in `watch` (Watch start, step 1). A dry run with a host
or planning error also exits 3, reports its partial plan and errors, and
uses `avail_after_bytes: null` when its final sample failed. The JSON
`status` of each: 2 `usage_error` (also for a refusal to
run as root, or to run `--allow-root` other than as root or without
`CAP_SYS_PTRACE`, [safety.md](safety.md#privileges), to reclaim under
`hidepid` or outside the initial PID namespace,
[safety.md](safety.md#in-use), a lock a needed
reclaim cannot take, with `cause: provisioning`,
[safety.md](safety.md#host-lock), and, in
development builds, the pre-release sandbox gate,
[safety.md](safety.md#pre-release-sandbox-gate)),
130 / 143 `interrupted`, 75 `lock_timeout`, 0 `met` (or `dry_run`), 124
`deadline`, 3 `error`, 1 `exhausted`.

## Output

- stdout carries only the result. Human mode: silent when nothing needed
  doing (like `cp`); otherwise a summary with one line per candidate and
  its final outcome over every tier: `reclaimed PATH: CHANGE` (the first
  ten, then a count), `reclaimed part-way PATH: CHANGE, then kept: ...`, or
  `kept PATH: REASON, DIAGNOSTIC`, where a candidate no tier reclaimed
  carries the refusal of the last tier that judged it (grouped by reason
  after five; `unreadable` and `not_owner`, on a shared host mostly other
  users' entries, from two), and Cargo targets whose previous reclaim left only lock
  files are omitted. Reasons are the JSON names, except `keep_marker`,
  which reads `keep marker`. A candidate kept as `in_use_unknown` because
  no scan stayed fresh or the process list did not settle reads "the host
  was too busy to check whether it is in use" (also when a process's
  reads did not settle, `unknown_cause: busy`), and one line counts them
  and says to run again when the host is less busy; one kept because a
  process of the user cannot be inspected names that process, and one
  line counts them and says to run again once it has exited; one kept
  because a loop device's backing file cannot be located, or because the
  process list stopped being complete during the run, gives that cause.
  Only the first are counted as the host's load. A candidate kept
  `producer_unknown` because diskgc cannot see every process (it runs in
  its own PID namespace, as in most containers, or `/proc` has `hidepid`)
  says so in words, and one line per such cause counts them and says
  where to run instead. A root on another filesystem than `PATH`'s
  (`other_fs`, below) is one line naming every tier that has it. Lines
  name their tier (`[TIER]`) when the run had more than one. The summary lists preflight profile refusals separately
  from kept candidates. A dry run (the report's `dry_run`, also when it
  failed) gives each candidate one verdict, `would reclaim`, `eligible,
  not needed` (the estimates before it already cover the goal) or `kept`,
  and says what the candidates it would take free by estimate against
  what the goal needs. It is bounded like a live summary: the first ten of
  each eligible verdict, then a count with their summed estimate; kept
  candidates as above (grouped by reason after five). `--json` lists every
  candidate. Its per-candidate verdicts are not repeated on stderr. Human results, including dry runs, report elapsed
  time and, when present, open deleted files that hold space by process
  count, bytes, and the largest three holders (holders of 0 B are left
  out). A human result that prints also has one line per source
  term (`summary` below; a dry run's include a term that found nothing),
  after the first line: `profile NAME [TIER]: ...`, its JSON `summary`
  block in words, e.g. `profile pytest-tmp: 6425 found · 2971
  eligible est. 5.8 GiB (would reclaim 2971) · kept 145 future_time est.
  1.6 GiB, 1 in_use est. 34.3 MiB · 3308 not judged (deadline)`, or for a
  live run `profile cargo-target: reclaimed 3 (observed change +2.1 GiB)
  · kept 2 not_idle est. 1.0 GiB, 1 keep marker unsized`. `unsized`
  counts candidates refused before their walk measured them.
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
  tier and profile name, as far as stderr takes it: lines it does not take
  in time are dropped and counted on the action's line, never failing the
  action ([safety.md](safety.md#command-actions)). Each tier logs where it looked (`N target(s)
  below ROOT`), or `nothing searched` when none of its roots is on
  `PATH`'s filesystem. A note of a process scan (`process scan: ...`, e.g.
  how many processes of other users were not inspected) is logged once per
  run while its text stays the same, not once per tier.
- `watch` logs to stderr, where a supervisor such as journald collects
  it; with `--json` it writes one JSON line per reclaim cycle on stdout
  ([Watch JSON lines](#watch-json-lines)).

JSON result (field names are part of the contract):

```json
{
  "status": "met | exhausted | error | usage_error | lock_timeout | deadline | interrupted | dry_run",
  "exit_code": 0, "dry_run": false,
  "started_at": "2026-10-21T12:00:00Z", "duration_ms": 0, "version": "0.1.0",
  "mount": "/", "path": "/home/mark/src/app", "seed": null,
  "index": {"source": "live | snapshot | sync", "age_seconds": 0, "units": 0, "blocks": 0, "untallied": 0, "coverage": "complete | partial"},
  "tally_cut_short": false,
  "min_free_bytes": 0, "target_free_bytes": 0, "target_met": false,
  "avail_before_bytes": 0, "avail_after_bytes": 0,
  "plan":     [{"group": "", "tier": "", "profile": "", "candidate": "", "eligible": true, "reason": null, "estimated_bytes": 0, "operation": "", "checkouts": "refuse | allow", "build_evidence": {"marker": "", "source": "", "idle_age_seconds": null}}],
  "actions":  [{"tier": "", "profile": "", "candidate": "", "avail_delta_bytes": 0, "duration_ms": 0, "idle_age_seconds": null, "count": 1}],
    "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | in_use_unknown | not_idle | too_idle | changed | empty | keep_marker | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_backend | backend_unavailable | missing | unsafe_path | contains_ipc | contains_mount | contains_special | contains_checkout | protected | excluded | not_owner | filtered | future_time | stale | too_many_entries | unreadable | newest_version | linked_current | installed_extension | pinned_toolchain | producer_unknown", "detail": null, "process_blockers": [{"pid": 42, "command": null, "evidence": "fd | cwd | root | exe | map | inode_map | environment | unreadable | target_cwd | target_exe | project_cargo | detached_cargo | cargo_manifest_dir | owner_pid | command_line | temp_root | unresolved_path"}], "unknown_cause": null, "estimated_bytes": null}],
  "summary":  [{"tier": "", "profile": "", "found": 0, "eligible": {"count": 0, "estimated_bytes": 0, "unsized": 0}, "would_reclaim": {"count": 0, "estimated_bytes": 0, "unsized": 0}, "reclaimed": {"count": 0, "avail_delta_bytes": 0}, "kept": [{"reason": "", "count": 0, "estimated_bytes": 0, "unsized": 0}], "not_judged": 0}],
  "held_by_deleted_files": [{"pid": 0, "command": "", "bytes": 0}],
  "overruns": [{"operation": "", "path": "", "over_ms": 0}],
  "errors":   [{"tier": "", "profile": "", "candidate": "", "cause": "parse | config | safety | host | operation | usage | provisioning", "message": ""}]
}
```

Not applicable is `null`, never an empty string: a field the example
shows as a string is `null` where it does not apply to that entry (each
case is listed below), and a field the example shows as `null` may also
be its value.

Errors: every command fails under `--json` with an object that opens
like this report, `status`, `exit_code` and `errors` with entries of the
shape above; `ensure` prints its whole report and `survey` its whole
object (`paths: []`, [Survey JSON](#survey-json)) once its command line
is understood, every other command (`check`, `profiles`, and the
commands this version does not have) only those three fields, e.g. `{"status": "usage_error", "exit_code": 2,
"errors": [{"tier": null, "profile": null, "candidate": null, "cause":
"parse", "message": "..."}]}`.

`cause` is a stable fault-class code in error objects and `errors[]`.
Scripts match `cause`, `status` and the process exit code; message text is
for people and may change. `parse` means invalid command or explicit
`--select` syntax and grammar; `config` means invalid or missing
configuration, `safety` means a safety or privilege
precondition refused the run, `host` means a typed operating-system path,
sampling or startup failure, `operation` means a tier planning, action or
backend failure without a more specific typed cause, and `usage` covers
other invalid caller inputs, including `--roots` binding and an unsupported
kernel. `provisioning` means the host lacks a usable lock a reclaim
needs (exit 2, status `usage_error`; which cases:
[safety.md](safety.md#host-lock)): the host lock missing for root or not
the regular file its tmpfiles line makes, or an unsafe per-user lock. A
user without the host lock is not a `provisioning` error: the reclaim
falls back to its per-user lock, or runs without one. A failure of the
host while locking is `host` (exit 3). The fix is the environment's, not
the caller's call: the message says what is wrong and prints the
commands that provision the host lock (the package's
tmpfiles.d drop-in, written out, installed in `/etc/tmpfiles.d` and
applied with `systemd-tmpfiles --create`; or `mkdir`/`touch`/`chmod`
until the next boot), so a harness alerts ops rather than retrying or
rewriting its call (TASK-151). An unsafe configured root is a `safety` refusal even when found
after initial root preflight; a host I/O failure while checking that root
is `host`.

- `exit_code` is the process's exit code for this `status` (the table
  above); it is derived from `status`, never set apart from it. A failed
  stdout write exits 3 whatever the report says. It is `null` in `watch`
  cycle records, where no process exits
  ([Watch JSON lines](#watch-json-lines)).
- `dry_run` is `true` when `--dry-run` was given, also for a dry run that
  failed (`status: error` or `usage_error`, not `dry_run`); on a command
  line that was not understood it says whether `--dry-run` was among the
  arguments.
- `min_free_bytes` and `target_free_bytes` are the goal: as given when it
  is a byte count, known before anything is measured, so a run that fails
  at startup (a host check, the kernel) still says what it was asked to
  reach; a percentage is `null` until the filesystem's size is sampled.
- `started_at` is the UTC wall-clock time when an executor run entered the
  library, in RFC 3339 form, or `null` if the host time is outside the
  supported range. `duration_ms` is elapsed monotonic time for that run, or
  `null` if it cannot be represented as an unsigned 64-bit millisecond
  count. Both include early returns from the executor. Errors produced by
  CLI parsing, configuration or startup before the executor begins use
  `null` for both. `version` is always the build's package version.
- `seed` is `null` when no group uses `random`.
- `index` says where the run's index started (`live`: the running
  `watch`'s own; `snapshot`: a valid snapshot, then refreshed by the
  synchronous tally; `sync`: built from nothing by that tally, also
  under `--no-index` or when the snapshot was unusable), the age in
  seconds of what it started from, how many units it holds and how many
  of those are collapsed blocks (`blocks`: each stands for many
  candidates, found and checked one by one when it is expanded,
  [architecture.md](architecture.md#memory-budget-and-collapse)), how
  many candidates were not yet tallied, and whether it covers everything
  it should: `partial` when some selected source root has no complete
  discovery listing in it (the tally budget ended first, or `watch` has
  not finished its first pass). Hitting the memory budget collapses
  units into blocks; it never makes coverage `partial`. It is `null`
  until TASK-244 fills it, although `ensure` builds, uses and saves an
  index (its stderr says where the index started and what its
  synchronous tally did).
- `tally_cut_short` is true only after a reclaim's tally (the
  synchronous one before the host lock, its sizing of an `oldest` first
  tier as the walk reaches each candidate, or a tier's under the lock)
  actually stops at `--tally-budget` with at least one candidate left
  untallied (or, for the synchronous one, a source not yet listed). It is false
  for dry runs, runs that stop before tallying, and tallies that complete;
  false does not say that every possible candidate was discovered.
- Paths are JSON strings; bytes of a path that are not UTF-8 are shown as
  U+FFFD (the text is for reading; diskgc itself compares paths byte for
  byte).
- `plan` is filled only by `--dry-run`; `reason` uses the skip-reason enum.
  The current plan has one row per candidate. `estimated_bytes` is
  `null` when the size is unknown: Docker and Nix without a reported
  size, candidates not yet tallied, and a skipped candidate whose checks
  stopped before its walk was complete (a size would be a partial count); `checkouts`
  is the source profile's setting the candidate was judged under (`allow`:
  it may hold a version-control checkout, see
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate));
  `operation` is human-readable text (`cargo lock protocol`, a command's
  argv), not an enum: it is for reading, not for matching. A skipped row
  has a size when its eligibility walk was complete before the refusal
  (e.g. an idle requirement, the in-use check, a liveness contract); a
  refusal before or during the walk (e.g. ownership, a keep marker, a
  version rule) has `null`.
  A source-anchored build candidate whose source was established at
  discovery has `build_evidence` with `marker`, `source` and
  `idle_age_seconds`. The age is measured from the newest change in an
  eligible candidate. It is `null` when a skip stopped the eligibility
  walk before an age was established; the skip reason is retained. Source
  evidence on a skipped row records discovery and may not have survived
  every later recheck. Rows without established source evidence have
  `build_evidence: null`. A row of the tier as a whole (its profile
  refused before discovery: a missing executable, an unavailable backend)
  has `candidate: null`.
- `avail_delta_bytes` is signed: the observed net change in available space
  around an action, never "bytes freed by this action" (see
  [architecture.md](architecture.md#measuring-space)); `null` when a
  sample around the action failed. Anything diskgc did not measure is
  `null`, never 0: `avail_after_bytes` when the final sample failed (and
  then `target_met` too, and the run never counts as met). A candidate
  deleted part-way (its deletion stopped, `changed`, `in_use`, `in_use_unknown` or an error)
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
  `count` and the first 20 paths in `examples` (`candidate` is then
  `null`). Up to 20 in a group remain individual entries. The first 1,000
  `actions` remain individual entries; any further actions become one
  summary entry with their `count`, summed `avail_delta_bytes` and
  `duration_ms`, and `tier`, `profile`, `candidate` and `idle_age_seconds`
  `null`.
- `skipped[].detail` is an advisory human diagnostic from the refusal, or
  `null` when no diagnostic was captured. Grouped rows always use `null`;
  programs should branch on `reason`, not parse this text.
- `skipped[].unknown_cause` (added with TASK-263; a consumer of the
  0.1 contract that ignores unknown fields is unaffected) says why an
  `in_use_unknown` could not be judged, and is `null` for every other
  reason and for an aggregated row: `busy` (no in-use scan stayed fresh
  for this candidate in its attempts, checked in full; or a scan that
  did not settle, because the process list kept growing, a process's own
  reads did not hold still while it was exiting or changing, or, for the
  project contracts, its command line did not, was waited out for this
  candidate until the deadline, a signal, or the goal reached by someone
  else ended the waits, or it stopped a Cargo target part-way: a later
  run may judge it; since TASK-340 a busy host makes a tier back off,
  never skip candidates unchecked; since TASK-348 a process whose reads
  did not settle is `busy`, its detail naming it; since TASK-364 a scan
  that did not settle is waited out for each candidate before it is
  kept), `unreadable_process` (a process of the user could not be
  inspected and can stay so while it runs, such as a `newgrp` shell, or
  one has not held still in any scan for 30 s and is taken to be stuck,
  TASK-364; `process_blockers` names it),
  `incomplete_process_list` (`hidepid`, a PID namespace other than the
  initial one, or its facts unread or changed during the run) and
  `loop_device` (a loop device's backing file cannot be told apart from
  the candidate; until the device is detached; [safety.md](safety.md#in-use)).
  A candidate a tier skipped unchecked after three in a row (its detail
  starts `not checked:`) carries the cause of the last of them, and its
  process, never `busy` (TASK-340). The dry run's plan does not carry it.
- `skipped[].process_blockers` is `null` when the refusal cannot be
  attributed to an observed process, including flock contention, an
  unsettled process scan, a process whose reads or command line did not
  settle (`unknown_cause: busy`, a busy host's; its detail names it,
  TASK-348, TASK-366; once taken to be stuck it is `unreadable_process`
  and named here) and grouped rows. Otherwise it is a nonempty array
  of `{pid: u32, command: string|null, evidence: string}`. `pid` and
  `command` are scan-time observations; an unreadable process can have a
  known PID but no command. Evidence is one of `fd`, `cwd`, `root`, `exe`,
  `map`, `inode_map`, `environment`, `unreadable`, `target_cwd`, `target_exe`,
  `project_cargo`, `detached_cargo`, `cargo_manifest_dir`, and for liveness
  contracts `owner_pid`, `command_line`, `temp_root`, `unresolved_path`
  ([Liveness contract predicate](#liveness-contract-predicate)). Cargo
  targets and pytest sessions can have several blockers, one entry per
  process, sorted by pid. The field reports visible evidence, not an
  exhaustive set of processes using a candidate.
- `skipped[].estimated_bytes` is the candidate's allocated-byte estimate
  only when a complete tally succeeded before a later refusal, or the
  refusing reclaim's own walk was complete before it refused and before
  anything was deleted (also after `--tally-budget` left the candidate
  untallied). Early skips, grouped rows and refusals during or before the
  walk use `null`. It
  is a point-in-time size, not a promise of space that could be reclaimed.
- `summary` (added with TASK-324; a consumer that ignores unknown fields
  is unaffected) has one block per source term, a tier's source profile
  (`--select 'profile:a || profile:b'` is one tier with two), in run
  order; a dry run has a block for every term it planned, a live run
  only for those with a row. It counts the rows of `plan` (a dry run) or of `actions` and
  `skipped` (a live run) of that tier and profile: per tier, not per
  candidate, so a candidate one tier keeps and a later tier reclaims is
  kept in the first block and counted in the second. A row without a
  candidate (a profile refused before discovery) is not counted. A
  size object `{count, estimated_bytes, unsized}` counts candidates:
  `estimated_bytes` sums the sizes known (an estimate from the walk,
  never extra I/O), `unsized` counts those without one, which are in
  `count` but not in the sum.
  - `found` (dry run): `eligible.count` + every `kept` count +
    `not_judged`; `null` when `not_judged` is. Like the human
    per-candidate lines, `kept` also counts the roots and directories
    discovery refused (a `missing` root, an `unreadable` directory on the
    way), which are rows of their own.
  - `eligible` (dry run): the term's eligible rows; `would_reclaim`
    (dry run): those of them a reclaim would take before the goal is
    reached by estimate, each candidate once, by the first tier that
    finds it eligible (the human `would reclaim` verdict).
  - `reclaimed` (live run): the candidates with an action in this tier,
    including those deleted part-way, and the sum of their observed
    `avail_delta_bytes` (`null` when any is).
  - `kept`: the term's refused candidates by `reason` (the skip-reason
    enum, in the order each first appears), each counted and sized as
    its rows are (`plan[].estimated_bytes`,
    `skipped[].estimated_bytes`). A live run's candidate deleted
    part-way is in `reclaimed`, not here.
  - `not_judged` (dry run): candidates the term found that the plan did
    not judge, or judged and dropped, because it stopped first (the
    deadline, a failure that stopped the run, a `!profile:` whose
    matches could not be enumerated, a misconfigured root that excluded
    the tier): they are in no `plan` row. Discovery's refusals listed
    before such a stop keep their rows (in `kept`). When discovery itself
    stopped, it counts what was listed by then (none for a Cargo tier).
    `null` when the term's planner did not report: the run stopped
    before it, or it failed.
  - A dry run has `reclaimed` `null`; a live run has `found`,
    `eligible`, `would_reclaim` and `not_judged` `null` (a reclaim judges
    a candidate only when it takes it and stops at the goal, so it has no
    such count). A candidate a live run did not reach (the goal, a
    signal, the deadline, also one a tier waited for a busy host until)
    is in no row and no count; status `deadline` or `interrupted` says
    the run stopped before it judged them all.
- A Cargo target whose profile directories hold only lock files is omitted
  from a live `ensure` result's `skipped` list and human skipped count; a
  stderr line names it (`TIER: PATH: its profile directories hold nothing
  but lock files; nothing to reclaim, not listed in the result`), so the
  tier's `N target(s) below` count adds up. It remains discoverable and may
  appear as an ineligible `empty` plan row in `--dry-run`.
- A source root of a tier on another filesystem than `PATH`'s
  ([architecture.md](architecture.md#filesystem-matching); another
  subvolume of `PATH`'s btrfs is the same filesystem) is not
  searched (a negated profile's root is: its matches only exclude). It
  is one row per tier that has it (in `skipped`, or in `plan` for
  `--dry-run`), however many of the tier's sources share it: `candidate`
  is the root, `reason` is `other_fs`, `profile` is the first source
  with that root, and `detail` says it is a root (decided for 0.1,
  TASK-210: no field of its own).
- `held_by_deleted_files` lists open-but-deleted files on the requested
  filesystem (visible processes only): an empty list when the run's
  process scan found none, `null` when it is unknown (TASK-187: never `[]`
  then). Their
  space returns only when the process closes them; diskgc never kills
  processes. A file held by several processes contributes its allocated bytes
  once, to the lowest listed pid; the other holders appear with zero bytes.
  Which files are on the requested filesystem:
  [architecture.md](architecture.md#filesystem-matching).
  `command` is `null` when the process's command could not be read.
  It comes from the run's last process scan (one is made at the
  end if the run made none). It is `null` when that scan failed (diskgc
  out of file descriptors, the process list unreadable), stopped (the
  deadline, a signal) or did not settle (a process list that kept
  growing), when the mount table could not be read after it, and when
  the run made no scan: a run that reclaimed nothing
  because the goal was met, that ended before the reclaim (a usage error,
  a lock timeout), or that stopped before its end-of-run scan.
- `overruns` lists operations that blocked past the deadline (see
  [safety.md](safety.md#deadlines)); always present, usually empty.
- On a usage error, fields that could not be determined (`mount`, `path`,
  sizes, `target_met`) are `null`; `errors` says why. In `errors`, `tier`,
  `profile` and `candidate` are `null` where they do not apply (a usage
  error, an error of the whole run, an error of a tier and not of one
  candidate).

### Backend skips and errors

Docker, Podman and Nix tiers act through a backend that diskgc binds first
([safety.md](safety.md#docker-kinds), [safety.md](safety.md#podman-images),
[safety.md](safety.md#nix-store)).
One rule holds for every backend kind, at preflight and at the recheck
right before acting:

- **Skip** (the tier is excluded, reported in `skipped` with candidate
  `null` at preflight, or with the storage candidate the plan showed (the
  graph root, `/nix/store`, ...) at the recheck right before acting, as
  every backend kind does; the other tiers run, and the exit code follows
  available space:
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
- The Nix daemon logs one line per diskgc probe (step 4 of
  [safety.md](safety.md#nix-store)), `unexpected Nix daemon error: read
  of 32768 bytes: Connection reset by peer`: diskgc closes the connection
  after the daemon's magic answer. Two per reclaim, one per dry run; not
  a fault.
- `unsupported_store` is not a reason: store selection is part of binding
  a backend and reports `unsupported_backend` or `remote_backend`.
- Status: the Nix kind is implemented this way (TASK-36). Docker's code
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
| Podman (`podman-images`: `info` output, `image prune` rules) | 5.8.7 | an isolated store's prune and the development machine's read-only queries ([safety.md](safety.md#podman-images)) |
| Nix daemon (`nix-store`: GC and root semantics) | 2.34.8 | the Nix GC container scenarios on CppNix 2.34.8 (`nix develop -c just e2e-nix-gc`; TASK-36 records the runs); [testing.md](testing.md#nix-store-backend-vectors) |
| Nix client (`nix-store`: command line, `--query --roots` output) | `nix-store (Nix) 2.34.8`, with that daemon | same |

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
report; exit 130/143. `watch` also stops on SIGHUP and exits 0
([Watch signals](#watch-signals)). Lock inheritance by children is
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

A configuration for diskgc 0.1 uses only the 0.1 built-ins
(`cargo-target`, `nix-shell-rc`, `pytest-tmp` and the idle filters); 0.2's
catalog adds `nix-garbage`, `pytest-cache`, `python-pycache`,
`mypy-cache`, `python-venv` and `node-compile-cache` (see
[Release notes](#release-notes-next-release-draft)).
It extends `docs/example.yaml` (the example the package installs) with a
laptop's roots, a protected project and a pressure group:

```yaml
version: 1
protect:                           # never deleted by any tier (by overlap)
  - {profile: cargo-target, roots: [/home/mark/src/important]}
groups:
  - name: stale                    # nobody will miss these
    tiers:
      - name: tmp-pytest-sessions
        select: profile:pytest-tmp && filter:idle-1d
      - name: tmp-nix-shell-rc
        select: profile:nix-shell-rc && filter:idle-1d
      - name: cargo-old
        select: profile:cargo-target && filter:idle-30d
        roots: [/home/mark/src, /home/mark/topics]
  - name: pressure                 # costs rebuild time
    tiers:
      - name: cargo-recent
        select: profile:cargo-target && filter:idle-1d
        roots: [/home/mark/src, /home/mark/topics]
```

**Development build only.** The example below shows the whole schema
`version: 1`; diskgc 0.1 and 0.2 refuse it, naming in one
run each part they do not ship: user `profiles:` (including overrides),
`profile_policies` for built-ins outside their catalogs (`npm-cache`),
`protect` of a user profile, and tiers that select a user profile or a
built-in outside their catalogs (`docker-build-cache`,
`docker-dangling`). Every key keeps its meaning in a later release.

```yaml
# Development build only: diskgc 0.1 and 0.2 refuse this configuration.
version: 1
ipc: block-any                    # block-any (default) | live-only

profiles:                          # optional: your own, or overrides
  - name: zephyr-run
    category: build
    find: {roots: [/tmp], paths: ["zephyr-*-run.*"], type: dir}
    action: delete
    examples: {match: [/tmp/zephyr-h5e5-run.jTm9zGlH], no_match: [/tmp/zephyr-hal]}
  - name: claude-sessions          # Claude Code's session dirs (no built-in)
    find: {roots: [/tmp], paths: ["claude-{uid}/-*/*"], type: dir}
    action: delete
    examples: {match: [/tmp/claude-1000/-home-mark-src/7cc7f78e]}

profile_policies:                  # optional: checkout guard only; names a built-in source
  - profile: npm-cache
    checkouts: allow

protect:                           # never deleted by any tier (by overlap)
  - claude-sessions                # fixed-root profile: bare name
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
        select: profile:pytest-tmp && filter:idle-2d
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
  error. A root you supply (a tier's `roots:`, a `protect` entry's
  `roots:`, or `--roots`) must exist on the machine `ensure` runs on: one
  that does not (ENOENT or ENOTDIR) is exit 2 before anything is planned
  or deleted, also in a dry run and when space is already met, with every
  such root named with where it was written (`config PATH: line N, tier
  `NAME``, `protect `PROFILE``, or `--roots`; JSON `cause: config`, or
  `usage` when only `--roots` are missing). Under `--select` the
  configured tiers' roots are not used and not checked; `protect` roots
  are. A missing `PATH` is reported first. Roots a profile fixes itself
  (`/tmp` for the `/tmp` built-ins, a user profile's `find.roots`) may be
  absent: such a root is a `missing` plan row and skip, not an error. A
  root removed after this check is likewise a `missing` skip (TASK-152).
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
candidate lock. The in-use check is not term-local evidence: it judges the
candidate for the first term whose own evidence admits it. A refusal
there (`in_use`; `in_use_unknown`; a live socket's `contains_ipc`) is
never handed to a later term to admit, in the plan as in the reclaim, so a
cause that ends between two terms' checks cannot move a candidate to
another profile (TASK-337); a process scan that does not settle is waited
out under the term that asked for it (TASK-364). A term that may still supply the
action is checked on its own walk against the scan current then, never on
an earlier term's clearance. The live half of a `protect_toolchains` guard
(a running manager, a pin in a process's environment or above its working
directory) judges the scan after the in-use check, and like a liveness
contract it vetoes the candidate for every term that found it, in the plan
as in the reclaim. A command union invokes its
shared argv at most once per inode, with `DISKGC_PROFILE` and the
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
development config example protects `claude-sessions`, a user profile over
Claude Code's session directories (the matching set of the withdrawn
`claude-code-tmp`); diskgc 0.1 has no user profiles, so it can protect only
its own built-ins, such as `{profile: cargo-target, roots: [...]}` in the
0.1 example. The packaged `example.yaml` has no `protect` entry. Filesystem and Cargo reclaim
judge each candidate against the matches under its locks and just before
deletion: a `delete` against the matches enumerated for its tier, enumerating
them again when the candidate changed since; a `command` against matches
enumerated again at each check ([safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate),
Not protected, not excluded). Docker
rechecks protection before invoking prune. An incomplete scan stops the
run. Concurrent changes in an independently writable protected tree cannot
be made atomic with deletion of another tree; do not rely on this rule as a
security boundary against another local writer. For a user-defined `command`
action, the command must itself limit its effects to the selected candidate;
diskgc cannot constrain arbitrary actions taken by that program.

### Protection marker

A file named `.diskgc-keep` in a filesystem candidate, anywhere inside it,
or in any directory between a profile root and the candidate, excludes it
(reported as `keep_marker`; the human summary says `keep marker`).

## Profiles

diskgc's knowledge of *what* can be deleted and *how* lives in **profiles**:
named, composable rules for recognising one kind of cruft. Tiers say which
profiles apply and when.

The profile below is a built-in's definition; the same schema written in a
configuration's `profiles:` is a user profile, which only development
builds accept: diskgc 0.1 runs its built-in profiles alone.

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
  verified_with: CPython 3.13.15 and observed CPython 3.12 .pyc layout in a project tree, 2026-10-20; CPython 3.14.7 and pytest 9.1.1 in the container tests (TASK-350)
  liveness_class: tolerant           # built-ins only; see Liveness contracts
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
  daemon and checks its filesystem against the requested path. Podman
  profiles (`podman-images`) use it too, for the graph root of the
  caller's local store, which Podman reports (it has no daemon).
- **Filter profiles** have only `where` (predicates). They narrow a source's
  candidates; they never find anything.
- **`action`** says how a source profile's candidates are removed. Docker
  and Podman profiles declare one of their fixed prune actions. Other
  code-backed kinds fix their action.
- **`kind`** (default `fs`): `fs` (a file or directory), or a code-backed
  kind: `cargo-target`, `docker-build-cache`, `docker-images`,
  `podman-images` (provisional, TASK-93), `nix-store`.
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

The `command` action and the Docker and Podman kind schema fields in this
table (their `kind` names, `action` forms and kind-specific constraints) are
**provisional until the release review (task-49)**. The
schema v1 freeze covers the other fields, the `delete` and Cargo actions,
the `nix-store` profile shape below, and `find.roots: daemon` as the
spelling for a root the bound backend supplies, in every backend kind
(final, TASK-118).

A `nix-store` source profile is exactly `name`, `category`, `description`,
`kind: nix-store`, `find: {roots: daemon}` and, on a built-in,
`verified_with`, `liveness_class` and `releases`; any other field (`action`, `find.paths`, `where`,
`min_idle_for`, `examples`, `checkouts`) is a load-time error. The store
is not configurable: it is `/nix/store` of the bound system daemon
([safety.md](safety.md#nix-store)). Selected alone as `profile:NAME` (no
filters, no union), it reports one plan row and one action with candidate
`/nix/store`, or one skip (candidate `null` at preflight, `/nix/store` at
the recheck: [Backend skips and errors](#backend-skips-and-errors)), and
`estimated_bytes: null`. Its skip reasons are those of [Backend skips and
errors](#backend-skips-and-errors) plus `changed`; the human `detail`
names the evidence (the client's canonical path, both versions, the
store URL seen, the generation link the daemon did not report) and is not
part of the contract.

| Field | On | Meaning |
|---|---|---|
| `name` | every profile | A `NAME` (see Selection grammar), unique among built-in and user profiles. |
| `category`, `description` | every profile; required on built-ins | Free text, shown by `profiles list`. |
| `kind` | sources | Default `fs`. |
| `find.roots` | sources | A list of absolute directories, `required`, or `daemon` for Docker kinds, `podman-images` and `nix-store`. |
| `find.paths` | kind `fs`, required | Patterns relative to the roots, segments separated by `/`; no segment is empty, `.` or `..`, or contains `**`. Each segment matches one name, compared as bytes: `*` matches any run of bytes (none included, and a leading `.`: `*` matches `.X11-unix`), every other byte itself; `?`, `[`, `]` and `\` are reserved (a load-time error), so a later glob syntax cannot change what an existing pattern matches. `{uid}` is replaced first. |
| `find.exclude` | kind `fs`, optional | Paths omitted from this source, with the same syntax and root as `find.paths`. An excluded path is never a candidate or a skipped candidate. An excluded leaf is not statted; an excluded intermediate directory may be opened to reach descendants matched by a deeper positive pattern. The exclusion applies to that exact relative path, not automatically to descendants. A present `null` is invalid. |
| `find.type` | kind `fs`, required | `dir` (a directory), `file` (a regular file) or `any` (a directory or a regular file). Symlinks, sockets, FIFOs and devices are never candidates, whatever the type: a wildcard profile over `/tmp` would otherwise report every symlink there. |
| `where` | filters (at least one predicate); optional on sources | See Predicates. A source’s own predicates narrow action admission in every term, but its positive retention guards cover raw `find` matches even when other source predicates fail. `!profile:` and `protect` ignore source `where`. A source has no `where.idle_for`; the tier chooses its idle filter. `app_not_running` and `lacks_entry` are source-only. |
| `min_idle_for` | sources, optional | A nonzero duration that every candidate must meet even when the source is selected alone. The tier may demand a longer idle period with filters. A source uses this when a shorter period would make its matched entries unsafe to reclaim. |
| `action` | kinds `fs`, Docker and Podman, required | See Actions. Cargo and Nix kinds fix their action and take no `action`. |
| `examples` | kind `fs`, required | `{match: [...], no_match: [...]}`, absolute paths, at least one `match`; built-in and user profiles alike (User profiles). With `roots: required`, an optional `root: /absolute/sample/root` pins the supplied root used to check path depth in these examples; without it, any absolute ancestor can supply the root. Config load and `check` reject examples that contradict the path patterns. |
| `checkouts` | sources, any kind | `refuse` (the default, also when absent) or `allow`; any other value is a load-time error. Whether a candidate may hold a version-control checkout (`.git`, `.hg`, `.svn`) or lie inside a checkout's metadata; refused ones are `contains_checkout`. `allow` can delete a repository with uncommitted work: see [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate) (No source checkout) before writing it. |
| `empty_checkout_markers` | built-in kind `fs` sources with `type: dir` and `checkouts: refuse`, optional | Places below a candidate, in `find.paths` syntax, each ending exactly in `.git` with no `.git`, `.hg` or `.svn` before it, where an entry of that name does not refuse the candidate if it is an empty regular file: `uv-cache`'s `["sdists-v*/.git"]` (Mark, 2026-11-05, TASK-360). Anything else of that name there (a directory, a symlink, a file with content), and every marker elsewhere, still refuses ([safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate), No source checkout). Part of the built-in's definition: `profile_policies` does not change it. Not a user profile field (a load-time error there); non-empty, no duplicates. |
| `verified_with` | built-in sources, required | The tool version the profile was checked against. |
| `liveness_class` | built-in sources, required | The source's liveness contract class under the admission rule ([safety.md](safety.md#producer-liveness)): `sound`, `producer-equivalent`, `verified-heuristic`, `tolerant`, `delegated` or `none`. Its evidence, residuals and verdict are its row in [Liveness contracts](#liveness-contracts). Recorded data: diskgc does not act on it at run time. |
| `releases` | built-ins, optional | The release catalogs that include the built-in, listed explicitly: `["0.1", "0.2"]` (quoted strings) puts it in the 0.1 and 0.2 catalogs, which release rows of the release descriptor name ([Built-in profile library](#built-in-profile-library)). Membership, not "since": a later catalog lists its own members. No duplicates; refused with `liveness_class: none` unless empty. Absent, null or empty: the development library only. |
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
while `npm-cache` and `uv-cache` currently default to `refuse` (`uv-cache`
tolerating only its own empty `sdists-v*/.git`, `empty_checkout_markers`;
a user `override: true` of `uv-cache` cannot carry that field, so it
refuses every real uv cache unless it says `checkouts: allow`). The effective
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
| `source_anchor` | `cmake`: require `CMakeCache.txt` to record a normalized `CMAKE_HOME_DIRECTORY` that is a strict ancestor of the candidate under its bound root, with a regular `CMakeLists.txt` that survives outside the candidate; `meson`: require regular generated `meson-info/meson-info.json` with matching build and info paths, a normalized strict-ancestor source under the root, regular `meson-private/coredata.dat` and surviving regular source `meson.build`; `autotools`: require regular `config.status`, `config.log`, and `Makefile`, with the status script recording the exact build path and a strict-ancestor source that still contains regular `configure.ac`. None of these three takes a build directory inside another build system's managed tree: beside its stamps (Buildroot's `.stamp_*`, OpenWrt's `.built`, `.configured*`, `.prepared*`) in the parent or the three directories above it, not above the root, anywhere below a Buildroot output, Yocto TMPDIR or Bazel output base, or below npm's `node_modules` ([safety.md](safety.md#builds-inside-another-build-systems-tree)). Development library only, pending Mark's admission (TASK-101): `buildroot`: a candidate named `output` with no `.config` entry and a regular `.br2-external.mk` whose single generated `BR2_EXTERNAL ?=` line lists no br2-external tree (it is Buildroot's only record of them), whose parent keeps regular `Config.in`, `Makefile`, `.config`, `package/Config.in` and `support/scripts/br2-external`; `yocto`: a regular `saved_tmpdir` holding exactly the candidate's path and a regular `abi_version`, whose parent keeps regular `conf/local.conf` and a `conf/bblayers.conf` with one literal `BBLAYERS ?=` or `=` assignment of normalized absolute layers, each keeping `conf/layer.conf`, and no other line naming `BBLAYERS` and no `include`, `include_all`, `require`, `inherit`, `addfragments`, `export` or `unset` line; `bazel`: a regular `DO_NOT_BUILD_HERE` holding one normalized absolute workspace path outside the candidate, which keeps `MODULE.bazel`, `REPO.bazel`, `WORKSPACE.bazel` or `WORKSPACE`. The source tree must be on the candidate's filesystem and none of its files may lie inside the candidate; Yocto layers may lie anywhere but inside the candidate. Source identity and metadata are rechecked under the candidate lock. | `fs` source only |
| `keep_newest` | keep the newest positive N matching siblings by directory/file mtime; equal mtimes break by descending name bytes. Missing mtime or incomplete sibling listing keeps the candidate. For Cursor extensions `newest_group: cursor_extension` ranks each extension id separately; unknown name layouts are kept. | `fs` |
| `protect_links` / `protect_link_dirs` / `protect_parent_link` | keep a candidate containing a resolved symlink target. `protect_links` lists exact paths, `protect_link_dirs` scans every symlink in directories relative to the bound root, literal but for at most one whole `*` segment, which stands for every directory of that level (`.kani/*`, listed afresh at each judgement), and `protect_parent_link` names a symlink beside the candidate's parent (e.g. a package's `current` beside `releases/`). An unsafe or unreadable directory, incomplete listing, ambiguous target, or missing named link retains the candidate. An unrelated launcher resolving outside the bound root is ignored; a dangling external launcher is ignored only after every existing prefix was checked for symlinks. Target paths with internal `..` are ambiguous and retain the candidate. Links are re-read during reclaim under the candidate lock. | `fs` |
| `liveness_contract` | a code-backed producer liveness contract (`nix-shell`, `pytest`; designed: `node-project`, `python-project`): the candidate is eligible only when the contract permits it, judged from the same process scan as the in-use check, in the plan and again immediately before deletion ([Liveness contract predicate](#liveness-contract-predicate)) | `fs` source only |
| `protect_cursor_extensions` | a literal path relative to the bound root to Cursor's `extensions.json`, beside extension candidates. Every `relativeLocation` listed as installed is kept, even if older than the newest N. The JSON array and each entry's `location.path` must agree with that one directory name. A missing, unreadable, malformed, oversized (over 1 MiB) or changing manifest retains all candidates. | `fs` |
| `protect_toolchains` | `rustup` or `elan`: keep a toolchain directory (directly in a `toolchains` directory) that its manager cannot install again, whose binaries were read recently, or that a pin or a live process could select; pin state that cannot be read completely keeps it. The rules and limits: [safety.md](safety.md#toolchain-pins). Kept as `pinned_toolchain` (recent use: `not_idle`). | `fs` |

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
conflicting grouping, parent-link scopes, Cursor manifests or toolchain managers are rejected. A changed or
unrecognised version layout retains data. The
link check occurs again immediately before the deletion walk, but no
filesystem API makes a link update and deletion atomic: installers must
coordinate with cleanup if they repoint a link concurrently.
`keep_newest`, link protection (including `protect_parent_link`),
`protect_cursor_extensions` and `protect_toolchains` are retention guards. A filter containing any
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

Status: every contract is implemented and judged. `nix-shell` and
`pytest` (TASK-112 step 5) are used by the `nix-shell-rc` and
`pytest-tmp` built-ins; `node-project` and `python-project` (TASK-79) by
`node-modules` (development library only) and `python-venv` (in the 0.2
catalog). What
each contract shows,
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
| `node-project` | a `node_modules` directory whose parent holds a regular `package.json` and that lies in no other dependency directory | project dependency directories |
| `python-project` | a virtual environment (holding a regular `pyvenv.cfg`) whose parent holds a Python project manifest and that lies in no other dependency directory | project dependency directories |

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
- a contract this version does not judge yet (none since TASK-79), the
  message naming the supported ones, so nothing half-built can be
  selected; the find-shape and checkout rules below apply to each from
  the version that judges it;
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
apply the same rule. The veto covers only a candidate a contract source
found itself: a source without a contract whose candidate *contains* an
instance (a user profile over `nix-shell.*` directories holding pytest
sessions) deletes it with the rest, guarded only by the in-use check; no
built-in does that (`no_builtin_candidate_contains_a_contract_builtins_candidate`
over the golden tree), and a user profile that does carries its author's
risk, as for any contract-less profile. `profile_policies` cannot change it. A user profile may use it like any predicate. A new contract is a new
value, added with the producer it was verified for; a generic form (a pid
pattern, a producer lock) is added only when a second producer needs the
same evidence.

Report additions (they extend the JSON enums of Output). Landed with
`nix-shell`: `producer_unknown` and `owner_pid`; with `pytest`:
`temp_root`, `command_line` and `unresolved_path`. A pytest verdict's
`detail` names the row of the decision table that decided
(`liveness_contract pytest row 10 (unused root): ...`), or none for the
checks before the table (`liveness_contract pytest: ...`); programs branch on
`reason` and the evidence, not on that text:

- skip reason `producer_unknown`, used only for a recognised instance of
  a contract whose producer state cannot be determined. Its detail names
  the failed rule: a malformed marker, a filesystem not on the local list,
  not the initial PID namespace, `hidepid` on the held `/proc`, a process
  that stayed unreadable, a relative or unresolvable temp-root or
  `--basetemp` value, an unreadable command line or environment, an
  oversized or unparsable workspace `package.json`, a project climb that
  reached a bind-mounted subdirectory or found the nearest project above
  the source root. Its `process_blockers` is `null`
  when no particular process caused it (a precondition, a marker, a
  listing); otherwise it holds one ordinary entry (a pid, as every entry
  has) per process that caused it, sorted by pid, each with evidence
  `unreadable` (existing) or `unresolved_path` (new: a path the contract needs for that process
  did not resolve: a relative value of a variable or option pytest
  resolves, an absolute one whose lookup in the process's root failed, or
  a failed lookup of one of Python's fallback roots).
- what is not a recognised instance is `filtered` (a name without a valid
  pid, a name that is not a session name, a missing manifest, a nested
  dependency directory, a `node_modules` that an outer one would replace); a failed listing or project walk is
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
The examples in this section name built-ins of the development library;
`diskgc profiles list` shows which ones a build ships (diskgc 0.1:
`cargo-target`, `nix-shell-rc`, `pytest-tmp` and the idle filters).

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
| `profile:nix-shell-rc && filter:idle-2d && !filter:idle-30d` | `profile:nix-shell-rc && filter:idle-2d && !filter:idle-30d` | nix-shell rc directories idle for 2 to 30 days |
| `profile:node-compile-cache && filter:idle-2d && !profile:claude-sessions` | `profile:node-compile-cache && filter:idle-2d && !profile:claude-sessions` | Node cache idle for 2 days, except what overlaps Claude Code's session dirs (the example config's `claude-sessions`) |
| `profile:pytest-tmp \|\| profile:node-compile-cache` | `profile:pytest-tmp \|\| profile:node-compile-cache` | either, at any age |
| `(profile:pytest-tmp && filter:idle-3d) \|\| profile:node-compile-cache` | `(profile:pytest-tmp && filter:idle-3d) \|\| profile:node-compile-cache` | pytest dirs idle for 3 days, Node cache at any age |
| `((profile:pytest-tmp \|\| profile:node-compile-cache) && filter:idle-3d) \|\| (profile:nix-shell-rc && filter:idle-30d)` | `(profile:pytest-tmp && filter:idle-3d) \|\| (profile:node-compile-cache && filter:idle-3d) \|\| (profile:nix-shell-rc && filter:idle-30d)` | the first two idle for 3 days, nix-shell rc directories idle for 30 |

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
  `podman-images`, `nix-store`) or `profile:NAME`, optionally followed by `&&` filters whose
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
| `profile:tmp-entry && !filter:claude-sessions` | `!filter:claude-sessions`: `claude-sessions` is a source profile of kind fs, not a filter; write `!profile:claude-sessions` |
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
| `command` (provisional) | an argv list run per candidate, e.g. `[env, "GOCACHE={path}", go, clean, -cache]`. Trusted, arbitrary command execution; never run as root. See [safety.md](safety.md#command-actions). A release runs only the built-ins' command actions its row lists (Built-in profile library; 0.2: four cleaners). |
| `docker-builder-prune` (provisional) | Prune the bound Docker Engine's built-in BuildKit cache with `--all`, adding `--filter until=...` when the tier selects `idle_for`. One action covers the daemon cache. |
| `docker-image-prune` (provisional) | Prune dangling images in the bound Docker daemon. |
| `docker-image-prune-all` (provisional) | Prune all images unused by containers in the bound Docker daemon. |
| `podman-image-prune` (provisional) | `podman --remote=false image prune --force`: Podman prunes the dangling images of the caller's bound local store ([safety.md](safety.md#podman-images)). |
| `podman-image-prune-all` (provisional) | The same with `--all`: every image no container (running or stopped) uses, tagged ones included, and, as Podman does with `--all`, its persistent `RUN --mount=type=cache` build caches ([safety.md](safety.md#podman-images)). |
| kind actions | Fixed by the kind: the Cargo lock protocol ([safety.md](safety.md#cargo-target)); for `nix-store`, one bounded GC through the bound Nix daemon ([safety.md](safety.md#nix-store)). |

In YAML:

```yaml
action: delete
action: {delete: {make_writable: true}}
action: {command: [env, "GOCACHE={path}", go, clean, -cache], cwd: parent}
action: docker-builder-prune
action: docker-image-prune
action: docker-image-prune-all
action: podman-image-prune
action: podman-image-prune-all
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

Podman images are pruned by kind `podman-images` and Podman's own
command, never by a `command` or `delete` profile: one whose candidate is
container storage is refused (`contains_special`,
[safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate)),
rootless overlay files belong to subuids (`not_owner`), and a command run
against a different checked path would not bind the store it changes
([safety.md](safety.md#podman-images)).

### Built-in profile library

Shipped inside the binary as YAML data, listed with `diskgc profiles list`
and at the end of `diskgc --help`.
A built-in profile must satisfy the profile-specific
[Definition of Done](testing.md#definition-of-done-every-task).
Every row below is a v1 *target*; until verified it is
listed as planned, not shipped.

Built-ins key on a build system's or a tool's own markers, not on one
project's tree: a project-specific tree such as OpenWrt, AOSP or a Linux
kernel tree is left to a user-written profile (Mark, 2026-11-03).

What a release runs, which built-ins its catalog holds and which files
its package ships is one row per release of the release descriptor,
`crates/diskgc-core/src/releases.json` (the binary, the flake's packages,
the NixOS module and the release notes' header read it; its rules are
checked when it is read). The optional `release-catalog` build feature
builds the row of the binary's version (`X.Y` of `X.Y.Z-dev`); rows of
tagged releases stay as they shipped (0.1: `ensure`, `check` and
`profiles`, no unit templates). On its own the feature retains the
development sandbox gate. The separate `release-candidate` package
combines it with `diskgc-linux/release-without-sandbox-gate` for
disposable-container testing; it is not approved for deployment. It is a
release only when built from that release's tag; built from master it is a
development version of release 0.2's row (`diskgc 0.2.0-dev (COMMIT)`),
for disposable test containers only. The 0.2 row runs `ensure`, a
reclaiming `watch` and `watch --tally-only`, `check` and `profiles`
(Mark, 2026-11-05, TASK-276: "Reclaiming watch in 0.2"; TASK-362), and
refuses `survey` (exit 2 before it looks at the machine, naming the
commands it has); its `--help` shows only the commands it runs, and its
package ships the two `watch` unit templates. Its `watch`, in either
mode, loads the configuration and `--select` through the same narrowing
as its `ensure`: the 0.2 catalog below, no user profile or override,
and a built-in it does not ship refused at start (exit 2). A row that
runs only `watch --tally-only` would refuse a reclaiming one the same
way ("a reclaiming `watch` is not in diskgc VERSION yet; its commands
are ..."); 0.1.0 refused every `watch`. The development build keeps a
reclaiming `watch`, behind the sandbox gate. The 0.2 row's catalog is
builtin.yaml's label `0.2` (`catalog: "0.2"`): 0.1's built-ins, which
list both labels, plus `nix-garbage` (Mark, 2026-11-04, TASK-335), five
project-tree sources, `pytest-cache`, `python-pycache`, `mypy-cache`,
`python-venv` and `node-compile-cache` (Mark, 2026-11-05, TASK-359;
`node-modules` held out, Mark, 2026-11-06, TASK-365), and four
package-manager caches whose tool's own cleaner is safe against a run of
that tool in flight, `pip-cache`, `uv-cache`,
`pnpm-store` and `ccache` (Mark, 2026-11-05, TASK-360: "only the
lock-safe five"; the fifth, `jest-cache`, left after review: its clear
can fail a run in flight), three runtime caches deleted whole,
`go-mod-cache`, `bun-cache` and `nix-user-cache`, and four
source-anchored build outputs, `cmake-build-anchored`,
`meson-build-anchored`, `autotools-build-anchored` and
`west-build-anchored` (Mark, 2026-11-07, TASK-370; cmake and meson once
TASK-367's guard kept builds inside another build system's tree). A release runs a command action only when its
row lists the built-in with exactly that command (`command_actions`:
argv and `cwd`); the 0.2 row lists those four, and the catalog is refused
for a command action the row does not list, or for a listed one the
catalog does not hold with that command, so no other cleaner (not
`npm-cache`, `go-build-cache`, `yarn-cache`, `platformio-cache` or
`jest-cache`), no
Docker or Podman prune and no operator-written command runs in 0.2. So
the candidate keeps `cargo-target`, the two `/tmp` built-ins with a
liveness contract (`nix-shell-rc`, `pytest-tmp`; TASK-65, Mark
2026-10-24), `nix-garbage`, those five, those four, those three, those
four builds and the seven idle filters, and rejects every operator-authored profile, including
overrides. It also refuses configured tiers and `--select` expressions that
refer to excluded built-ins, even during a dry run. `profiles list`,
`profiles show`, `check` and help use the same narrowed library.
`profile_policies` for the admitted sources works only to narrow: `checkouts:
refuse` on any of them (on `nix-garbage` it changes nothing: a GC has no
file candidates), `allow` only where the built-in already allows
(`cargo-target`, `pytest-tmp`); `allow` on `nix-shell-rc`, `nix-garbage`,
one of the five project-tree sources, one of the four caches (`uv-cache`
included: its built-in tolerates only its own empty marker) or one of
TASK-370's seven is refused, as
are policies for excluded sources, including `npm-cache`, at load time. The other `/tmp` proposals stay out: `nix-shell-tmpdir`, `mktemp-dir`
and `tmp-entry` have no owner evidence ([Liveness
contracts](#liveness-contracts)). Operator-authored `fs` and delete policies
remain a release decision under TASK-67.

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | pytest session directories `pytest-of-*/pytest-*`, one candidate per session (Mark, 2026-10-23), under `/tmp`'s temp-prefix v1 preset; `liveness_contract: pytest`, `min_idle_for: 1h` ([Liveness contracts](#liveness-contracts)); `checkouts: allow`: tests create Git repositories as fixtures in their temp directories, which are the run's own output | `delete` |
| test | `pytest-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-359): `.pytest_cache` (roots required) | `delete` |
| nix | `nix-user-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): `.cache/nix/{gitv3,tarball-cache,tarball-cache-v2,eval-cache-v*}` below a supplied home root | `delete` |
| nix | `nix-shell-rc` | rc directories `nix-shell-*-*` and `nix-develop-*-*` directly in `/tmp` (not below shell temp directories); `liveness_contract: nix-shell`, `min_idle_for: 1h` ([Liveness contracts](#liveness-contracts)) | `delete` |
| nix | `nix-shell-tmpdir` | planned: `nix-shell.*` directories under `/tmp`'s temp-prefix v1 preset, including below `/tmp/claude-{uid}`; dormant shells can use them without an observable descriptor or initial environment path | no safe action yet |
| nix | `nix-garbage` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-04, TASK-335): dead store paths of the bound multi-user daemon (`{kind: nix-store, find: {roots: daemon}}`), Nix's own choice ([safety.md](safety.md#nix-store)) | one bounded `nix-store --gc` |
| rust | `cargo-target` | Cargo `target/` dirs (roots required); `checkouts: allow`: build scripts clone sources into a target (a vendored dependency in `build/<pkg>/out/`), which is Cargo's own output, reclaimed by its lock protocol | lock protocol |
| containers | `docker-build-cache` | BuildKit cache of the bound Docker Engine | prune |
| containers | `docker-dangling` | untagged images | prune |
| containers | `docker-unreferenced` | images no container uses (removes tagged images too) | prune |
| containers | `podman-dangling` | development library only, pending Mark's admission (TASK-93): dangling images of the caller's local rootless Podman store | `podman image prune` |
| containers | `podman-unreferenced` | development library only, pending Mark's admission (TASK-93): images no container, running or stopped, uses in that store, tagged ones included (diskgc's own e2e images when no container uses them), and Podman's persistent build caches ([safety.md](safety.md#podman-images)) | `podman image prune --all` |
| containers | `containers-image-tmp` | withdrawn (Mark, 2026-10-23): matched top-level `/var/tmp/buildah*` and `/var/tmp/container_images_storage*` with a mandatory `min_idle_for: 3d`, which cannot show that a producer is done with a spool it may reopen; removed from the library (TASK-112 step 0), its matching set kept as the user-probe vector `containers-image-age`; re-admission only under a new name after TASK-125 | none |
| withdrawn | `claude-code-tmp`, `chromium-tmp`, `playwright-tmp`, `zephyr-tmp`, `renode-tmp`, `xdg-cache-entry-v2` | withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24): class *none* ([Liveness contracts](#liveness-contracts)); removed from the library, each matching set kept as a user probe profile in the vectors (`<name>-probe`); re-admission only under a new name (TASK-135, TASK-136) | none |
| agents | `cursor-agent-versions`, `codex-releases`, `claude-code-versions` | versions under a supplied home root; keep the two newest per sibling set and the installed symlink targets. `claude-code-versions` matches native installer version files only, not `~/bin/claude-*` | `delete` |
| rust | `rustup-toolchains` | development library only, pending Mark's admission: toolchain directories `.rustup/toolchains/*` below a supplied home root that `protect_toolchains: rustup` and `protect_link_dirs` do not keep ([safety.md](safety.md#toolchain-pins)); `min_idle_for: 1h` | `delete` |
| lean | `elan-toolchains` | development library only, pending Mark's admission: toolchain directories `.elan/toolchains/*` below a supplied home root that `protect_toolchains: elan` and `protect_link_dirs` do not keep ([safety.md](safety.md#toolchain-pins)); `min_idle_for: 1h` | `delete` |
| editors | `cursor-extensions` | version directories under `.cursor/extensions` of a supplied home root; keep the two newest of each extension id and every installed directory in `extensions.json` | `delete` |
| browsers | `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Only `Cache` and `Code Cache` directories in `Default` and `Profile *` under the corresponding browser's `.cache` directory below a supplied home root; each source requires its own `.config/.../SingletonLock` to be absent | `delete` |
| electron | `teams-for-linux-electron-cache`, `figma-linux-electron-cache` | development library only, pending Mark's admission of the app names: only `Cache`, `Code Cache` and `GPUCache` directories in the app's user-data directory `.config/<app>` below a supplied home root (teams-for-linux also in each `Partitions/*`); each source requires its own `.config/<app>/SingletonLock` to be absent | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required); `pip-cache` and `ccache` in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-360), `go-build-cache` and `npm-cache` development library only | `command` with the tool's own cleaner |
| caches | `go-mod-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): `go/pkg/mod` below a supplied home root | `delete` with `make_writable` |
| caches | `huggingface-hub-repos` | development library only, pending Mark's admission (TASK-40): one candidate per repository directory (`models--*`, `datasets--*`, `spaces--*`) of the Hugging Face hub cache `.cache/huggingface/hub` below a supplied home root; never the hub's `.locks`, the login token or the xet cache; `min_idle_for: 1h` | `delete` |
| rust | `cargo-registry-src` | development library only, pending Mark's admission (TASK-40): one candidate per crate source `.cargo/registry/src/*/*` below a supplied home root holding a regular `.cargo-ok` (an extraction Cargo completed); never the `.crate` downloads, the index or `bin`; `min_idle_for: 1h` | `delete` |
| python / js | `node-gyp-headers`, `electron-download-cache` | development library only, pending Mark's admission (TASK-40): node-gyp's header versions `.cache/node-gyp/*` holding a regular `installVersion`, and @electron/get's download directories `.cache/electron/*` (one per URL hash), below a supplied home root; `min_idle_for: 1h` | `delete` |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required); `uv-cache` and `pnpm-store` in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-360), `yarn-cache` development library only. `uv-cache` refuses checkouts but tolerates the empty `sdists-v*/.git` file every uv cache holds (`empty_checkout_markers`) | `command` (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `bun-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): `.bun/install/cache` or `.cache/.bun/install/cache` below a supplied home root | `delete` |
| python / js | `python-pycache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-359): `__pycache__` directories at relative depths 1 through 5 below each supplied project root | `delete` |
| python / js | `mypy-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-359): `.mypy_cache` directories at relative depths 1 through 5 below each supplied project root, with a regular `CACHEDIR.TAG` file | `delete` |
| python / js | `node-modules`, `python-venv` | `node_modules` directories beside a regular `package.json`, and `.venv` directories holding `pyvenv.cfg` beside a Python project manifest, at relative depths 1 through 5 below a supplied project or workspace root, with no hidden directory between the root and them (`find.exclude`: tool-managed installs such as `~/.npm/_npx`, editor extensions and `~/.cache` are not the user's projects), and inside no other dependency directory; `liveness_contract: node-project` or `python-project`, `min_idle_for: 30d`, judged by their project's activity ([Liveness contracts](#liveness-contracts)); `checkouts: refuse`. `python-venv` is in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-359); `node-modules` is in the development library only (Mark held it, 2026-11-06, TASK-365; successor TASK-357) | `delete` |
| python / js | `jest-cache` | Jest's default `jest_*` cache under `/tmp` or `/var/tmp`, including the temp-prefix v1 preset below | `jest --config '{}' --cacheDirectory '{path}' --clearCache` from the checked parent; Jest removes the measured directory itself |
| python / js | `node-compile-cache` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-05, TASK-359): `node-compile-cache` under `/tmp`'s temp-prefix v1 preset | `delete` |
| c / c++ | `cmake-build-anchored` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `CMakeCache.txt`, a direct `CMakeFiles/` directory, no `CMakeLists.txt` entry, and a surviving recorded CMake source ancestor; minimum 3-day idle age and `checkouts: refuse`; never inside another build system's tree ([safety.md](safety.md#builds-inside-another-build-systems-tree)) | `delete` |
| c / c++ | `meson-build-anchored` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `build.ninja`, a direct `meson-private/` directory containing regular `coredata.dat`, generated `meson-info/meson-info.json` recording this exact build directory and a surviving Meson source ancestor, and no `meson.build` entry; minimum 3-day idle age and `checkouts: refuse`; never inside another build system's tree ([safety.md](safety.md#builds-inside-another-build-systems-tree)) | `delete` |
| c / c++ | `autotools-build-anchored` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with regular `config.status`, `config.log`, and `Makefile`, status script recording this exact build directory and a surviving `configure.ac` source ancestor, and no `configure.ac` entry in the output; minimum 3-day idle age and `checkouts: refuse`; never inside another build system's tree ([safety.md](safety.md#builds-inside-another-build-systems-tree)) | `delete` |
| embedded | `buildroot-output-anchored` | development library only, pending Mark's admission (TASK-101): Buildroot's in-tree `output/` through depth five below a supplied root, holding a regular `.br2-external.mk`, a direct `build/` and no `.config` entry, beside a Buildroot source that keeps regular `Config.in`, `Makefile`, `.config`, `package/Config.in` and `support/scripts/br2-external`; the generated file names no br2-external tree; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| embedded | `yocto-tmpdir-anchored` | development library only, pending Mark's admission (TASK-101): a Yocto TMPDIR of any name through depth five below a supplied root, whose `saved_tmpdir` records exactly its own path, with `abi_version` and a direct `stamps/`, beside a build directory that keeps regular `conf/local.conf` and `conf/bblayers.conf`; every layer `BBLAYERS` lists (literal absolute paths only) keeps `conf/layer.conf`; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| build | `bazel-output-base-anchored` | development library only, pending Mark's admission (TASK-101): Bazel output bases `_bazel_*/*` below a supplied output user root (by default `~/.cache/bazel`), excluding `_bazel_*/cache` and `_bazel_*/install`, holding `DO_NOT_BUILD_HERE` and a direct `execroot/`, whose recorded workspace survives on the same filesystem with `MODULE.bazel`, `REPO.bazel`, `WORKSPACE.bazel` or `WORKSPACE`; the workspace's `bazel-*` convenience symlinks are never followed; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| jvm | `gradle-caches`, `maven-repo` | planned: `~/.gradle/caches`, `~/.m2/repository` (roots required) | no verified whole-directory cleaner: Gradle maintains cache entries by retention after a build, while Maven's dependency purge applies only to a project's dependencies |
| embedded | `west-build-anchored` | in the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370): Zephyr `build/` dirs through depth five below supplied project roots; regular `CMakeCache.txt`, direct `zephyr/` directory, no `CMakeLists.txt` entry, surviving recorded CMake source ancestor under the root, and mandatory 3-day idle age; never inside another build system's tree ([safety.md](safety.md#builds-inside-another-build-systems-tree)) | `delete` |
| embedded | `platformio-cache` | development library only (left out of 0.2, TASK-360): `.platformio/.cache` below a supplied home root; PlatformIO's default `core_dir/.cache` | `pio system prune --cache --force` with `PLATFORMIO_CACHE_DIR=.cache`, starting in the checked `.platformio` parent; PIO re-resolves the path as described in [safety.md](safety.md#command-actions) |
| generic | `tmp-entry` | planned: top-level regular files and directories in `/tmp`, `/var/tmp`; a dormant process can use an entry without an observable descriptor or initial environment path | no safe action yet |
| generic | `mktemp-dir` | planned: `tmp.*` directories under `/tmp`'s temp-prefix v1 preset; a live shell can use one as `TMPDIR` after exec, invisible to the process scan | no safe action yet |
| filters | `idle-1h`, `idle-1d`, `idle-2d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d` | (no `find`) | none |

`has-cachedir-tag` in the selection error examples is a parser-test fixture,
not a shipped built-in filter.

The command cache profiles use the tools' own cleaners on the exact measured
directory. They retain the default `checkouts: refuse` safety rule. Real uv
and npm caches can contain `.git` entries ([Liveness
contracts](#liveness-contracts)); those candidates report
`contains_checkout` and are left intact. The profile does not override that
check merely because the tool has a cleaner. A cache tool must be available
on `PATH` for its command profile to run.

**Built-in temp-prefix v1 preset.** `node-compile-cache`, `jest-cache`
and `pytest-tmp` opt in (`playwright-tmp` and `renode-tmp` did until their
withdrawal). Each base pattern (`node-compile-cache`, `jest_*`,
`pytest-of-*/pytest-*` respectively)
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
general process check for arbitrary Electron applications. Firefox
`cache2` needs a different guard and is outside this Chromium-family set.

The Electron application caches (TASK-78) use the same presence marker in
the app's own user-data directory. Electron writes `SingletonLock` there
only when the app calls `app.requestSingleInstanceLock()`, so each app is
admitted only after its own sources show that it takes the lock at
startup, before any window or session exists (Electron runs the main
script's top level before `ready`, before the GPU process and any
network context), quits when it does not get it, and never calls
`releaseSingleInstanceLock()` (Electron itself removes the lock at
`quit`, before the sessions close; see the residuals). Read 2026-11-02, all on Electron 43.6.0
from nixpkgs, whose binary carries Chromium's `SingletonLock`:

| App | Version read | Single-instance lock | Profile |
|---|---|---|---|
| teams-for-linux | 2.23.0 (`app.asar`) | at module load, after pinning `userData`; a second instance quits | `teams-for-linux-electron-cache` |
| figma-linux | 0.11.5 (`main/main.js`) | in the app object's constructor, whose manager arguments touch only settings, logs, themes and extensions, no session; a second instance quits | `figma-linux-electron-cache` |
| Cursor (and VS Code) | 3.1.15 (`out/main.js`) | none: single instance through VS Code's own IPC socket | none |
| draw.io | 31.1.8 (`app.asar`) | only for the GUI: `--export` runs a hidden window in the same user-data directory without it | none |
| Saleae Logic 2 | 2.4.44 (`app.asar`) | none | none |
| Claude desktop | not installed on the dev host | unverified | none |

They match only `Cache`, `Code Cache` and `GPUCache` (teams-for-linux
also in each persistent partition, `Partitions/*`); the user-data
directory, `Local Storage`, `IndexedDB`, `Service Worker` storage,
`Cookies` and the `Dawn*Cache` directories never match. A custom
`--user-data-dir`, a Flatpak or Snap install (another directory) and
another app version are outside what was read.

- `pytest-tmp` ships with the `pytest` liveness contract, one session per
  candidate (TASK-112 step 5). The former source (withdrawn after TASK-65)
  selected individual test directories: in a disposable container, pytest
  9.1.1 with `tmp_path_retention_policy=none` created
  `/tmp/pytest-of-root/pytest-0/test_active0` without a session `.lock`
  and, while the test slept, held no descriptor in it, so the process scan
  alone judged the live directory eligible; an idle filter cannot prove a
  session ended, and a lock-only guard misses that configuration. The
  contract keeps that session (`temp_root`: its root is `/tmp`, which any
  of the user's processes may use), and the counterexample is a vector and
  a container test.
- Each source's liveness class, residuals and verdict are its row in
  [Liveness contracts](#liveness-contracts). TASK-122 classified
  `claude-code-tmp`, `chromium-tmp`, `playwright-tmp`, `zephyr-tmp`,
  `renode-tmp` and `xdg-cache-entry-v2` as class *none*; they were
  withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). A user profile can still select
  such paths, at its author's risk: a live Claude Code session, Chromium
  or Electron application, Playwright browser or Renode emulation can use
  its directory without any visible evidence, and `zephyr-*` and
  `$XDG_CACHE_HOME` hold whatever their writers put there.
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
- `huggingface-hub-repos`, `cargo-registry-src`, `node-gyp-headers` and
  `electron-download-cache` (TASK-40; development library only, pending
  Mark's admission) use plain deletion because no cleaner of theirs takes
  the measured directory: `hf cache rm` takes repository ids and
  `huggingface-cli delete-cache` is interactive; `cargo clean gc` acts on
  all of `CARGO_HOME` by age and its size and age options are unstable;
  `node-gyp remove` takes a version, not a path; @electron/get has none.
  Each candidate is one unit the tool fetches or unpacks again (a hub
  repository, a crate source, a header version, a download). Idle measures
  changes, not reads: an entry fetched months ago and read every day is as
  idle as an abandoned one, and fetching it again can cost gigabytes, so
  choose a pressure tier for them. The one-hour floor keeps an entry a run
  is downloading or unpacking (fresh files); the residuals of a run that
  uses an older entry are in their [Liveness contracts](#liveness-contracts)
  rows. A deleted hub repository's empty lock directory under `.locks`
  stays; the hub creates it again on the next download. The profiles match only the default locations below a home root;
  they do not infer `HF_HOME`, `HF_HUB_CACHE`, `CARGO_HOME`, `--devdir` or
  `electron_config_cache`.
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
name (e.g. `pytest-tmp-2`). An old name is never redefined; a new config
`version` may retire it (using a retired name is then a load-time error). Narrowing is not safe either: a narrower profile protects
less where it is used in `!profile:` or `protect`. Each built-in profile has
a golden match set over fixtures; any change to it fails CI unless the
profile was renamed. Pending Mark's ruling (TASK-367): built-ins no
tagged release has shipped have been narrowed under their names
(`buildroot-output-anchored`, TASK-354; `cmake-build-anchored`,
`meson-build-anchored`, `autotools-build-anchored` and
`west-build-anchored`, TASK-367, before 0.2 admits them), as builtin.yaml's
comments allow for never-released definitions; whether this rule allows
that until a name's first tagged release is his to confirm.

### Liveness contracts

Every built-in source's liveness contract class and its verdict under the
admission rule of [safety.md](safety.md#producer-liveness), plus the
decided profiles that are not built-ins. A built-in source without a
sound, producer-equivalent, verified-heuristic, tolerant or delegated
contract is not admissible. The rule applies to every built-in, shipped
ones included (Mark, 2026-10-23); TASK-122 classified those that predate
it. This table is the single home for these decisions (TASK-112). Each
built-in's class is also data, its `liveness_class` in builtin.yaml, and
its `releases:` there lists the release catalogs it is in (`["0.1",
"0.2"]`); a
test keeps the table, the library and the catalogs in sync (every built-in
source in exactly one row, the same class, a release only for a Shipped
row). Rows marked *planned* name a contract that is designed but not
implemented.

Verdicts: *Shipped* (admitted; a verified heuristic only with a dated
admission in the row, "admitted by Mark, DATE" or, for a decision made on
his delegation, "admitted by mark-emulator for Mark, DATE", and its
container tests named, or "Container test: pending (TASK-N)"); *Decision pending* (a verified heuristic that awaits
Mark's decision; not admissible to a release until then); *Proposed
withdrawal* (class *none*, still in the development library until Mark
decides; never in a release catalog); any other verdict names no
built-in (excluded, withdrawn, planned, governed elsewhere). The
"Producer" column names the version the class was checked against and how
(source read, probe, host observation); TASK-122's probe commands are in
its task notes. A row naming
`candidate_catalog_project_tree_family_matches_the_development_build` was
also run through the release build's catalog path (gated, with the
sandbox clock offset), the profile in its catalog, through a test-only
candidate list when not yet admitted
([testing.md](testing.md#admission-evidence)); a row naming a
`release_candidate_*` test ran through the gate-lifted release-candidate
package itself.

| Profile | Producer (verified) | Contract (evidence) | Class | Residuals | Verdict |
|---|---|---|---|---|---|
| `nix-shell-rc` | legacy `nix-shell` and `nix develop` (Nix 2.34.8 on the host, 2.34.9 pinned by `nix_shell_rc_lifecycle_is_pinned`): the directory's pid is the shell's own for both, interactive and `-c`; `nix develop`'s directory is referenced by nothing after start; legacy `nix-shell`'s is the `TMPDIR` of the shell and its children and its rc trap removes it except after `SIGKILL` (safety.md) | `liveness_contract: nix-shell`, `min_idle_for: 1h` | sound, under Owner PID's preconditions | A consumer that outlives the shell and keeps only a path string (Owner PID's scope). `min_idle_for: 1h` narrows it and is not part of the contract's soundness: a detached child that wrote in the directory within the hour keeps it `not_idle`. | Shipped (TASK-112 step 4) after the lifecycle verification (both commands, interactive and `-c`, normal exit, `SIGHUP` and `SIGKILL`, a detached child outliving the shell, a shell in a private PID namespace sharing `/tmp`), with its vectors and container tests. Matching set as in the library row. Small in bytes, large in count. In the 0.1 and 0.2 catalogs (TASK-65). |
| `pytest-tmp` | pytest 9.1.1 (sources read; lifecycle pinned by `pytest_lifecycle_is_pinned`; the contract by `pytest_live_policy_none_session_in_shell_root_is_kept` and `pytest_live_basetemp_session_is_kept`), pytest-xdist 3.8.0 with execnet 2.1.2 (`pytest_orphan_xdist_worker_keeps_session`: a worker outlives its killed controller by about 15 s, then execnet ends it) | `liveness_contract: pytest`, `min_idle_for: 1h`, `checkouts: allow`; one candidate per session | verified heuristic: lock rows sound; the rank row producer-equivalent or stricter (it never permits a session pytest's own next cleanup, with the default retention, would keep; while a pytest of the same user runs it needs a session older than pytest's dead-lock age, Mark 2026-10-23); the unused-root row heuristic, and it does remove sessions above the rank that pytest keeps | Those of safety.md (pytest sessions), each with its vector. `min_idle_for: 1h` covers pytest's startup window between the session's `mkdir` and its `.lock` (microseconds), with a wide margin for a stalled start; it is not part of the contract's soundness. | Shipped (TASK-112 step 5), admitted by Mark, 2026-10-23. Matching set as in the library row. Every verdict of the decision table names its row in the detail; the checks before the table (safety.md) name none. Container tests: `pytest_lifecycle_is_pinned`, `pytest_live_policy_none_session_in_shell_root_is_kept`, `pytest_live_basetemp_session_is_kept`, `pytest_orphan_xdist_worker_keeps_session`. In the 0.1 and 0.2 catalogs (TASK-65, Mark 2026-10-24). |
| `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Chromium-family browsers, default user-data directory | `app_not_running` | sound (presence marker) | A browser that starts between the last lock check and the unlink finds its `Cache` or `Code Cache` missing, which Chromium treats as an empty cache. | Shipped (TASK-74). |
| `teams-for-linux-electron-cache`, `figma-linux-electron-cache` | teams-for-linux 2.23.0 and figma-linux 0.11.5 on Electron 43.6.0 (sources read, TASK-78: `app.requestSingleInstanceLock()` before `ready`, a second instance quits, no `releaseSingleInstanceLock()`), default user-data directory | `app_not_running` | sound (presence marker) | As the browsers': an app that starts between the last lock check and the unlink finds a cache missing, which Chromium treats as an empty cache. Electron removes the lock at `quit`, before its sessions close, so a cache can be deleted during an app's last moments of shutdown, with the same consequence; the in-use check narrows that window only while the closing app holds files in the cache open. Only for the versions read: a later version that stops taking the lock, or takes it after opening a session, falls outside the contract. | Shipped (TASK-78) to the development library only: in no catalog until Mark admits the app names. Container tests with the real apps: not yet written (a follow-up of TASK-78). |
| `cargo-target` | Cargo (1.90.0, 1.99.1, nightly-2026-10-16) | the kind's lock protocol, `checkouts: allow` | delegated (producer lock) | A program built in the target and running from it is protected by process heuristics only: Cargo holds no lock while it runs (PRD, Limits). | Shipped; in the 0.1 and 0.2 catalogs. |
| `docker-build-cache`, `docker-dangling`, `docker-unreferenced` | Docker Engine 29.8.2 and Buildx 0.37.2 in digest-pinned dind | the bound engine's own prune (`docker builder prune --all`, `docker image prune`, `docker image prune --all`); the daemon keeps what a build or container references | delegated | `docker-unreferenced` also removes tagged images that no container references; a later `docker run` pulls them again, or they are rebuilt when they were built locally and never pushed. | Shipped; not in 0.1 or 0.2 (Docker actions are outside their scope). |
| `podman-dangling`, `podman-unreferenced` | Podman 5.8.7: `image prune` in an isolated store (2026-10-20, TASK-93 notes); read-only on the development machine (2026-11-03) | the caller's local store's own prune (`podman --remote=false image prune --force`, with `--all`); Podman keeps every image a container, running or stopped, was created from, and (without `--external`) every image a Buildah working container uses | delegated | `podman-unreferenced` also removes tagged images no container uses, diskgc's own e2e images among them whenever their containers are gone; the harness reloads them from the Nix store (`podman load`), a pulled image is pulled again. A `podman run` between its image check and its container's creation can find its image gone and fail. `podman-unreferenced` also empties Podman's `RUN --mount=type=cache` caches without a lock: a build using one at that moment can fail or cache a partial result, and the next build downloads again. | Shipped in the development library only (class delegated, admissible by the rule); in no release catalog until Mark admits it (TASK-93); the `--all` prune not yet run in a container. Vectors: `podman-*` (backend). |
| `pip-cache` | pip 26.1.2 in the Nix container (TASK-351): installs from a local index fill `http-v2` (on plain HTTP pip caches only through a trusted host, probed); `pip cache purge` empties it and keeps its directories; the next install fetches again | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. pip takes no cache lock and holds no file in its cache between requests: an install in flight was purged under and completed, also when held at its only download, after its cache lookup, so that it only writes afterwards: pip creates the directories it writes into (unlike Jest, below) | delegated | No worse than the user running `pip cache purge`: an install in flight downloads again. Rebuilding needs the index. The wheel cache of sdist builds (`wheels/`) was not exercised. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-360: "only the lock-safe five"), its exact command listed in 0.2's release row (`command_actions`). Container tests: `pip_cache_real_pip_installs_reclaim_then_refetch_and_an_install_in_flight_completes`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path` and, through the gate-lifted package, `release_candidate_pip_cache_reclaim_then_refetch_and_an_install_in_flight_completes`. |
| `uv-cache` | uv 0.13.0 in the Nix container (TASK-351): every cache it creates holds `sdists-v10/.git`, an empty regular file (`sdists-v9/.git` on the development host, 2026-11-05); a `uv pip install` holds the cache's `.lock` open with a shared `flock` for its whole run; `uv cache clean` waits for that lock ("Cache is currently in-use") and then removes the whole directory | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse` tolerating only that empty file (`empty_checkout_markers: ["sdists-v*/.git"]`, Mark 2026-11-05, TASK-360; [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate), No source checkout); uv's own cache lock | delegated | A cache holding any other checkout marker is kept whole (`contains_checkout`): a `.git` directory, or a `.git` file with content, at `sdists-vN/.git`; a marker anywhere else, such as a git dependency's checkout below `git-v0/checkouts` (container test: checkout-shaped; not with a real `uv` git install, the containers have no network). An environment installed with the non-default `link-mode = "symlink"` points into the cache and breaks until reinstalled (the default hardlink mode survives the clean: container test). Rebuilding needs the network. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-360: "only the lock-safe five"), its exact command listed in 0.2's release row (`command_actions`). Container tests: `uv_cache_real_uv_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path` and, through the gate-lifted package, `release_candidate_uv_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept`. |
| `npm-cache` | npm 11.19.0 (Node.js 24.21.0) in the Nix container (TASK-351): installs from a local registry fill `_cacache`; every running npm holds its debug log `_logs/*-debug-0.log` open; `npm cache clean --force` removes `_cacache` and keeps `_logs` | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. npm takes no cache lock: its clean during an install in flight (which diskgc keeps `in_use` by the log) left the install to complete | delegated | A git dependency's leftover clone (`_cacache/tmp/git-clone*/.git`, seen on the development host 2026-11-05) keeps the whole cache `contains_checkout`. The clean removes `_cacache` only: npx's installs (`_npx`), `_prebuilds` and `_logs` stay (probed in the container), so the candidate's size overstates what it frees (on the development host, by its `_npx`). Rebuilding needs the network. | Shipped; not in 0.1 or 0.2: left out of 0.2 by Mark, 2026-11-05 (TASK-360: "only the lock-safe five"; TASK-351 recommended it only with a named residual: its clean frees less than the candidate's size, and a git dependency's leftover clone keeps the whole cache); 0.2's release row lists no command action for it, so the release catalog code path refuses it (`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`). Container tests: `npm_cache_real_npm_installs_reclaim_then_refetch_and_a_running_npm_keeps_it`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`. |
| `pnpm-store` | pnpm 12.9.0 in the Nix container (TASK-351): installs fill `v11` (an SQLite `index.db` and content files); an install holds `index.db` open and a shared `flock` on pnpm's store-operation lock in `/tmp/pnpm-store-operation-locks-UID` (fixed, whatever `TMPDIR` is); `pnpm store prune` takes it exclusively, so it waits for installs, and removes only packages no project references | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`; pnpm's store-operation lock | delegated | A process with a private `/tmp` (systemd `PrivateTmp`, a sandbox) does not share pnpm's lock. A package of a project pnpm no longer knows (moved, or removed by hand) is fetched again on its next install. Rebuilding needs the network. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-360: "only the lock-safe five"), its exact command listed in 0.2's release row (`command_actions`). Container tests: `pnpm_store_real_pnpm_prune_keeps_referenced_packages_and_an_install_in_flight`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path` and, through the gate-lifted package, `release_candidate_pnpm_store_prune_keeps_referenced_packages_and_an_install_in_flight`. |
| `yarn-cache` | Yarn Classic 1.22.22 in the Nix container (TASK-351): installs fill `v6`, an install writing each tarball it fetches into the cache (open while it downloads); `yarn cache clean` empties `v6` | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. Yarn Classic takes no cache lock: its clean during an install in flight (kept `in_use` by diskgc while a tarball was open) made the install fail (`ENOENT` linking a package it had fetched) | delegated | One failed, retryable `yarn install` when the clean lands while the install holds no file in the cache (between fetching and linking), where diskgc sees no use. Rebuilding needs the network. | Shipped; not in 0.1 or 0.2: left out of 0.2 by Mark, 2026-11-05 (TASK-360: "only the lock-safe five"; Yarn Classic takes no lock against its own clean, which made an install in flight fail); 0.2's release row lists no command action for it, so the release catalog code path refuses it (`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`). Container tests: `yarn_cache_real_yarn_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`. |
| `ccache` | ccache 4.14.1 in the Nix container (TASK-351): compiles fill `~/.cache/ccache` and `~/.ccache` (which ccache uses whenever it exists); a compile holds `tmp/inode-cache-*` open in its cache; `ccache --clear` removes every entry and keeps `stats` and `CACHEDIR.TAG` | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. ccache needs no lock against its clear: a clear during a compile in flight, held at the compiler's compile step (after ccache's cache lookup for it), left it to complete and store its result: ccache creates the directories it stores into (unlike Jest, below). The clear landing while ccache writes an entry was not exercised | delegated | Recompilation (time). Remote or secondary storage was not exercised. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-360: "only the lock-safe five"), its exact command listed in 0.2's release row (`command_actions`). Container tests: `ccache_real_compiles_reclaim_then_recompile_and_a_compile_in_flight_completes`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path` and, through the gate-lifted package, `release_candidate_ccache_reclaim_then_recompile_and_a_compile_in_flight_completes`. |
| `go-build-cache` | Go 1.26.8 in the Nix container (TASK-351): builds fill `XX/HASH-a` and `-d` entries; a build in flight had an entry mapped in the `go` command; `go clean -cache` removes every entry | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. Go takes no cache lock: its clean during a build in flight (kept `in_use` by diskgc through the mapping) made the build fail ("could not import ... no such file or directory") | delegated | One failed, retryable build when the clean lands while the `go` command holds no entry open or mapped (seen held at the compile of package main; not shown for every phase of a build). Test results cached there run again. | Shipped; not in 0.1 or 0.2: left out of 0.2 by Mark, 2026-11-05 (TASK-360: "only the lock-safe five"; Go takes no lock against its own clean, which made a build in flight fail); 0.2's release row lists no command action for it, so the release catalog code path refuses it (`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`). Container tests: `go_build_cache_real_go_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`. |
| `platformio-cache` | PlatformIO Core 6.1.19 in the Nix container (TASK-351): `pio pkg install` downloads into `.cache/downloads`, holding a `tmp*` file and a `.lock` open while it downloads; `pio system prune --cache` removes `.cache` and keeps installed packages; with telemetry on (its default, in diskgc's environment too) it writes `.cache/telemetry.json` again (probed) | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. The prune ignores PlatformIO's download lock: run during an install in flight (kept `in_use` by diskgc), it made the install fail (`FileNotFoundError`) | delegated | One failed, retryable install when the prune lands while the install holds no file in the cache (after its download, while it unpacks). The registry API cache was not exercised offline. Rebuilding needs the network. | Shipped; not in 0.1 or 0.2: left out of 0.2 by Mark, 2026-11-05 (TASK-360: "only the lock-safe five"; the prune ignores PlatformIO's download lock, which made an install in flight fail); 0.2's release row lists no command action for it, so the release catalog code path refuses it (`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`). Container tests: `platformio_cache_real_pio_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`. |
| `jest-cache` | Jest npm package 30.2.0 (`jest --version` prints 30.1.3) in the Nix container (TASK-351): runs fill `jest_<uid in base 36>` directly in `TMPDIR` (`/tmp` or a `nix-shell.*` one); a run holds no file there; `--clearCache` removes the measured directory | the tool's own cleaner on the measured directory (Built-in profile library); in use, `checkouts: refuse`. Jest takes no cache lock: a run in flight whose tests load no module after the clear (nothing recreates the directory) passes its tests and exits 1, failing to write its test timings (`perf-cache-*`, `ENOENT`) into the removed directory; one that loads a module afterwards recreates it and passes | delegated | One failed, retryable run (exit 1 although its tests passed) when the clear lands after the run's last module load. Transformation again (time). A cache at a custom `cacheDirectory` is not found. | Shipped; not in 0.1 or 0.2: admitted by Mark, 2026-11-05 (TASK-360: "only the lock-safe five"), then left out after a review reproduced the failed run; 0.2's release row lists no command action for it, so the release catalog code path refuses it (`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`). Container tests: `jest_cache_real_jest_runs_reclaim_then_rerun_and_a_run_in_flight_completes`, `jest_cache_clear_under_a_run_with_no_later_module_load_fails_the_run`, `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`. |
| `pytest-cache` | pytest 9.0.3 (cacheprovider source read, TASK-122); pytest 9.1.1 in the container (TASK-350: a session in flight whose cache is reclaimed ends with exit 0 and writes it again) | consumer tolerance: `Cache.get` returns the caller's default for a missing or unreadable key, `Cache.set` creates the directory and only warns when it cannot write | tolerant | The next `--lf`, `--ff` or `--sw` run has no record of earlier failures and runs every test (container test). | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-359). Container tests: `pytest_cache_real_pytest_reclaim_then_a_live_session_rewrites_it` and, the same scenario through the gate-lifted package, `release_candidate_pytest_cache_reclaim_then_a_live_session_rewrites_it`; `candidate_catalog_project_tree_family_matches_the_development_build`. |
| `python-pycache` | CPython 3.13.15 (importlib); CPython 3.14.7 and pytest 9.1.1's assertion rewriting in the container (TASK-350: a Python whose bytecode is reclaimed while it runs imports another module and caches it again) | consumer tolerance: a missing or stale `.pyc` is recompiled from its source, a failed bytecode write is ignored, and a `.pyc` in `__pycache__` is never imported without its source (PEP 3147), so the directory never holds code Python would import | tolerant | Recompilation on the next import; a read-only tree stays uncached (time only). A `__pycache__` whose `.pyc` files are tracked in Git (seen on the development host, 2026-11-05, some with no source beside them) is deleted too: `git status` then shows them deleted, recoverable from Git; an untracked `.pyc` whose source is gone would be the only copy of that compiled code. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-359, with the tracked-bytecode residual; a narrower successor is TASK-358's). Container tests: `python_pycache_real_imports_reclaim_then_a_live_importer_recompiles` and, through the gate-lifted package, `release_candidate_python_pycache_reclaim_then_a_live_importer_recompiles`; `candidate_catalog_project_tree_family_matches_the_development_build`. |
| `mypy-cache` | mypy 1.11.2 (cache layout); write paths read in mypy 1.9.0's source (TASK-122), not re-read for 1.11.2; mypy 2.1.0 in the container (TASK-350): its default cache is SQLite shards `<python version>/cache.N.db` behind `CACHEDIR.TAG`, every one of which a run was seen holding open while it parses a module | in use; consumer tolerance between runs: a module whose cache entry is missing is checked again; `CACHEDIR.TAG` required | verified heuristic | The next run is a full check (time). A run whose cache directory disappears while it writes stops with mypy's blocking "Error writing plugins snapshot" or fine-grained dependency error (read for 1.x's JSON cache; 2.1.0 with `sqlite_cache = false` not tested): one failed, retryable run (the in-use scan sees the run only while it holds a file there). With mypy 2.1.0's default SQLite cache a run was seen holding every shard open while it parses (container test), so the in-use scan sees it then; whether it holds them through its whole write phase was not tested. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), ratified by Mark, 2026-11-05 (TASK-359), with the residuals named here: tolerant between runs, not for a run in flight. In the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-359). Vector: `mypy-cache-run-in-flight-residual`. Container tests: `mypy_cache_real_mypy_reclaim_then_recheck_and_a_run_in_flight_is_kept` and, through the gate-lifted package, `release_candidate_mypy_cache_reclaim_and_a_run_in_flight_is_kept`; `candidate_catalog_project_tree_family_matches_the_development_build`. |
| `node-compile-cache` | Node.js 24.21.0 (probe, TASK-122: a cold cache, a warm cache with hits, the directory renamed away mid-run and a path below a regular file all exit 0; the mid-run write fails with "no such file or directory" and is ignored); the same Node in the container (TASK-350: caches of `module.enableCompileCache()` under `/tmp` and a `nix-shell.*` `TMPDIR`; a Node whose cache is reclaimed while it runs loads another module and exits 0) | consumer tolerance: entries are keyed by Node version and content hash, and every read or write failure is a miss | tolerant | Recompilation on the next start (time only). | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-359). Container tests: `node_compile_cache_real_node_reclaim_then_a_live_process_recompiles` and, through the gate-lifted package, `release_candidate_node_compile_cache_reclaim_then_a_live_process_recompiles`; `candidate_catalog_project_tree_family_matches_the_development_build`. |
| `nix-user-cache` | Nix 2.34.8 (probe, TASK-122, private `XDG_CACHE_HOME`: `tarball-cache-v2` moved away while `fetcher-cache-v4.sqlite` stayed; `fetchTarball` over HTTP downloaded again and returned the same store path, exit 0). Nix 2.34.9 in the Nix container (TASK-352, a local store below the home): `fetchGit` of a dumb-HTTP repository, `fetchTarball` and a flake attribute fill `gitv3`, `tarball-cache-v2` and `eval-cache-v6`; after their reclaim the same commands fill them again (`fetcher-cache-v4.sqlite` stays); a `fetchGit` in flight holds `gitv3/HASH.lock` open and runs Git with its working directory in `gitv3/HASH`. On the development host (2026-11-06) `eval-cache-v6` and `tarball-cache-v2` had changed minutes before a dry run: in use between evaluations, with no floor | in use (a reading process maps the Git cache's pack files); consumer tolerance between runs: Nix refetches or re-evaluates what its cache lacks | verified heuristic | A process between two reads of a Git cache can fail once (retryable). Refetching needs the network, so an offline build fails until the cache is repopulated. Not rebuildable: a source whose upstream is gone and whose store path was collected is lost with its cache copy. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the lost-upstream residual is a dated exception (safety.md, Producer liveness). Vector: `nix-user-cache-mapped-pack-in-use-and-run-between-reads-residual`. Container tests: `nix_user_cache_real_nix_fetches_reclaim_then_refetch_and_a_fetch_in_flight_is_kept` (a fetch in flight kept `in_use`, `cwd`), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352). In the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370: "go-mod, bun, nix-user"); through the release-candidate package: `release_candidate_nix_user_cache_reclaim_then_refetch_and_a_fetch_in_flight_is_kept`. |
| `go-mod-cache` | Go 1.26.7 (`go env GOMODCACHE`; Go's own `go clean -modcache` removes the same whole directory); Go 1.26.8 in the Nix container (TASK-352): a build downloads modules from a `GOPROXY` into read-only directories (reclaimed with `make_writable`) and downloads them again; the go command passes `GOMODCACHE` to every tool it runs, so a build in flight holds no file there but its compile names the cache in its initial environment | in use (for a build in flight, its subprocesses' initial environment); consumer tolerance between runs: a missing module is downloaded again and checked against `go.sum` | verified heuristic | A build reading module sources while they are deleted can fail once (retryable): the go command alone, between its subprocesses, names nothing (not reproduced). Needs the network: `GOPROXY=off` or offline builds fail until re-download. Not rebuildable: a module version neither the proxy nor its origin still serves. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the unserved-module residual is a dated exception (safety.md, Producer liveness). Vector: `go-mod-cache-build-in-flight-residual`. Container tests: `go_mod_cache_real_go_downloads_reclaim_then_redownload_and_a_build_in_flight_is_kept` (`GOPROXY=off` fails after the reclaim), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352). In the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370: "go-mod, bun, nix-user"; its `make_writable` delete is kind `fs`'s own action, which 0.2 admits); through the release-candidate package: `release_candidate_go_mod_cache_reclaim_then_redownload_and_a_build_in_flight_is_kept`. |
| `bun-cache` | Bun 1.3.3 (`bun pm cache`; Bun's own `bun pm cache rm` removes the same whole directory); Bun 1.4.2 in the Nix container (TASK-352): installs from a local registry fill both matched paths (the second through `BUN_INSTALL_CACHE_DIR`), hardlinked into `node_modules`, which still load after the reclaim; an install in flight holds the cache directory and its `.tmp` open. The symlink backend not probed | in use; consumer tolerance between runs: an install hardlinks or copies from the cache (Linux default), so installed `node_modules` keep their data; a missing package is downloaded again | verified heuristic | An install in flight can fail once (retryable). Needs the network. Projects installed with the non-default `--backend=symlink` point into the cache and break until reinstalled. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the backend claim is unprobed. Vector: `bun-cache-symlink-backend-and-install-in-flight-residual`. Container tests: `bun_cache_real_bun_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept` (an install in flight kept `in_use`, `fd`), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352). In the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370: "go-mod, bun, nix-user"); through the release-candidate package: `release_candidate_bun_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept`. |
| `cmake-build-anchored`, `meson-build-anchored`, `autotools-build-anchored`, `west-build-anchored` | CMake 3.30.5 (development host), and in the Nix container (TASK-354) CMake 4.4.3 with Ninja 1.13.2 and GNU Make 4.4.1, Meson 1.12.1, GNU Autoconf 2.73, West 1.5.0 with Zephyr 4.3.0 build directories; the producer (`cmake --build`, `ninja`, `make`, `west build`) runs with the build directory as its working directory for the whole build | in use (a running build's cwd, a running built program's executable or mappings), `min_idle_for: 3d`, the recorded source survives (source anchor), `checkouts: refuse` | verified heuristic | (1) Configure choices kept only in the build directory (`CMakeCache.txt` `-D` values, Meson options, `./configure` arguments in `config.status`) must be entered again. (2) Files a user put in a build directory by hand are deleted with it: a user's own work, outside the rule's loss bound (vector `cmake-build-anchored-hand-placed-file-residual`). (3) A dormant IDE or debugger configured on the directory finds it gone and rebuilds. (4) Package builds inside another build system's tree are not taken (TASK-367, Mark 2026-11-07; [safety.md](safety.md#builds-inside-another-build-systems-tree)): one near Buildroot's or OpenWrt's stamps (the parent and the three directories above it, within the root), one below a Buildroot output, Yocto TMPDIR or Bazel output base, and one below npm's `node_modules` (a cmake-js addon). Outside that guard: a meta-build system or package manager it does not know (dpkg-buildpackage, Spack, conda-build, a build below a Python `.venv`), stamps further up or above the root, and a build root reached through a bind mount; there the outer system's next rebuild of the package fails until it is cleaned. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122). Residuals (1) and (2) fall under the dated exception in safety.md (Source-anchored CMake output); remedy `.diskgc-keep` or `protect`. Vectors: `cmake-build-anchored-hand-placed-file-residual`, `cmake-build-anchored-configure-choices-and-dormant-ide-residual`. Container tests: `cmake_build_anchored_real_cmake_builds_reclaim_then_reconfigure_and_a_build_in_flight_is_kept`, `meson_build_anchored_real_meson_builds_reclaim_then_reconfigure_and_a_ninja_in_flight_is_kept`, `autotools_build_anchored_real_configure_builds_reclaim_then_reconfigure_and_a_make_in_flight_is_kept`, `west_build_anchored_real_west_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept`, `candidate_catalog_build_output_family_matches_the_development_build` (TASK-354: a build in flight keeps its output `in_use` by its working directory; the tool rebuilds the reclaimed output). In the 0.2 catalog, not 0.1's (Mark, 2026-11-07, TASK-370: "autotools, west"; cmake and meson with TASK-367's nested-build guard, his option "1"). Through the release-candidate package, the keep side of the three-day floor only (it reads no clock offset): `release_candidate_build_outputs_keep_builds_inside_the_3_day_floor` (CMake, Meson, Autotools, and an OpenWrt package build never planned), `release_candidate_west_build_keeps_a_build_inside_the_3_day_floor` (full suite only). |
| `buildroot-output-anchored` | Buildroot (master `Makefile`: `CONFIG_DIR` is the source top for the default `O=output` and the output itself otherwise; `support/scripts/br2-external` writes `.br2-external.mk`; sources read 2026-11-03; no Buildroot tree on the development host) | in use, `min_idle_for: 3d`, the Buildroot source with its `.config` survives and the output names no br2-external tree (source anchor `buildroot`), `checkouts: refuse` | verified heuristic | (1) `output/images` is deleted with the rest; it is rebuilt from the surviving source and `.config` at full-build cost, but an image built from an older source revision needs that revision. (2) Edits made directly in `output/build/<package>` (instead of `<PKG>_OVERRIDE_SRCDIR`) are lost: the user's own work; so is a configuration changed with `linux-menuconfig`, `busybox-menuconfig` and the like and not saved with their `update-defconfig` or `update-config` targets. (3) A source tarball missing from `dl/` is downloaded again, and one its upstream no longer serves fails the rebuild. (4) Buildroot runs its recipes from the top directory: a build is seen only through its recipe shells' exported `BASE_DIR`, `HOST_DIR` and the like (`environment`); while Make reads its makefiles (it runs the compiler then, to probe it: seen in the container, TASK-354) and between two steps, Make and its `$(shell)` children name nothing in `output/`, so a reclaim in that window deletes the output under a live build: a failed or full rebuild (not reproduced). | Decision pending (Mark's admission, TASK-101, including residual 2): in no catalog. Container tests: `buildroot_output_anchored_real_buildroot_reclaim_then_rebuild_and_a_build_in_flight_is_kept`, `candidate_catalog_build_output_family_matches_the_development_build` (TASK-354: Buildroot 2026.08.1 `defconfig` and local host packages; a recipe step in flight kept `in_use` by `BASE_DIR`). Vectors: `buildroot-output-anchored` (builtins). |
| `yocto-tmpdir-anchored` | BitBake with OpenEmbedded-Core (master `meta/lib/oe/sanity.py`: `saved_tmpdir` holds TMPDIR's path and a moved TMPDIR is refused, "delete it and rebuild"; `abi_version`; `bblayers.conf.sample`; sources read 2026-11-03; no Yocto tree on the development host) | in use (a running task's cwd, open files, mappings), `min_idle_for: 3d`, the build directory's `conf/` and every listed layer survive (source anchor `yocto`), `checkouts: refuse` | verified heuristic | (1) Images and packages in `tmp/deploy` are deleted; they are rebuilt from the shared-state cache (outside TMPDIR) or from source, but an image of an older layer revision needs that revision. (2) Edits made in `tmp/work` (a `devshell` session) and a configuration changed with `bitbake -c menuconfig` and not saved with `diffconfig` or `savedefconfig` are lost: the user's own work; `devtool`'s workspace lies outside TMPDIR. (3) The fetcher's git clones (`S = "${WORKDIR}/git"`) put `.git` in most real TMPDIRs, so `checkouts: refuse` keeps them; `profile_policies` `checkouts: allow` reclaims them at the cost of (2); seen with BitBake 2.20.0's Git fetcher in the container (TASK-354). | Decision pending (Mark's admission, TASK-101, including residual 2): in no catalog. Container tests: `yocto_tmpdir_anchored_real_bitbake_reclaim_then_rebuild_and_a_task_in_flight_is_kept`, `candidate_catalog_build_output_family_matches_the_development_build` (TASK-354: BitBake 2.20.0 with OE-Core yocto-6.1 run to a task after `do_unpack`, offline; a task in flight keeps TMPDIR by the FIFO BitBake makes in its `temp/` (`contains_ipc`), its working directory in `tmp/work` behind that). Vectors: `yocto-tmpdir-anchored` (builtins). |
| `bazel-output-base-anchored` | Bazel (master `BlazeWorkspace.java`: the output base's `README` and `DO_NOT_BUILD_HERE` record the workspace path; `bazel clean --expunge` removes the same directory; source read 2026-11-03; no Bazel output base on the development host) | in use (a running server holds its `java.log` and server files open), `min_idle_for: 3d`, the recorded workspace survives with a boundary file (source anchor `bazel`), `checkouts: refuse` | verified heuristic | (1) Repositories in `external/` are fetched again; the repository cache (`_bazel_*/cache`, excluded) keeps content-addressed downloads, and an upstream that no longer serves one fails the rebuild. (2) The workspace's `bazel-*` convenience links dangle until the next build. (3) A `git_repository` clone in `external/` makes the checkout guard keep the whole output base. (4) The output base of a Git workspace is always kept (`contains_checkout`): the execroot links every top-level entry of the workspace, `.git` included, and the guard counts a symlink named `.git` as a checkout (TASK-331, confirmed with Bazel 7.6.0, TASK-354); `checkouts: allow` reclaims it, and with it (3). | Decision pending (Mark's admission, TASK-101): in no catalog. Container tests: `bazel_output_base_anchored_real_bazel_reclaim_then_rebuild_and_a_live_server_is_kept`, `candidate_catalog_build_output_family_matches_the_development_build` (TASK-354: Bazel 7.6.0 genrules in WORKSPACE mode; an idle server and a build in flight keep their output bases `in_use`). Vectors: `bazel-output-base-anchored` (builtins). |
| `cursor-agent-versions`, `codex-releases`, `claude-code-versions`, `cursor-extensions` | Claude Code native installer (host 2026-10-24: three versions kept beside the link target, so it prunes to three itself), Cursor agent CLI (31 versions on the host: no pruning), Codex CLI (11 releases), Cursor 3.1.15 extensions (superseded versions stay until Cursor's own startup cleanup, not verified) | `keep_newest: 2`, the installed link targets, `current` and `extensions.json`; in use: a running version's executable or mapped files lie inside its candidate (vectors: exe, map, cwd, fd) | verified heuristic | A launcher or script that runs an older version by its full path fails to start until reinstalled; a process of an older version that reads its code files and closes them (the extension host of an open Cursor window still running a superseded extension beyond the two newest) fails on its next lazy load until reloaded. Versions are downloaded again. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122). Vectors: `claude-code-versions-running-old-version-exe-in-use` (the evidence), `claude-code-versions-launcher-by-full-path-residual`, `cursor-extensions-superseded-extension-lazy-load-residual`. Container test: pending (TASK-137). |
| `rustup-toolchains`, `elan-toolchains` | rustup 1.29.1 and elan 4.2.4 (sources read, TASK-94: name grammars, `settings.toml` version 12 and rustup's fallback, elan's `known-projects` and channel resolution, a stray update hash removed on the next install); both managers install a missing toolchain a project, default or override selects again (rustup's `auto-install`, on by default; elan always) | `protect_toolchains`, the link guard over `toolchains/` (and `.kani/*`), `min_idle_for: 1h`; in use (a running toolchain binary's executable or mappings, a toolchain `bin/` in a process's initial `PATH`) | verified heuristic | Those of safety.md (Toolchain pins): pins it does not read (outside the roots, deeper, on a network mount, on a command line, exported after a shell started, another root or mount namespace, other tools than Kani); the next use downloads the toolchain again and loses components and targets added by hand; a tool configured with a full path fails until reinstalled; rustup's `update-hashes/NAME` stays; stale overrides, old `.kani` installs, `known-projects` lines and elan channel pins keep more. | Decision pending (Mark's admission, TASK-94): in no catalog; container test pending (real rustup and elan on fixture homes, after admission). Vectors: `rustup-toolchains`, `elan-toolchains` (builtins), and in `liveness/` every `rustup-toolchains-*` and `elan-toolchains-*`. |
| `huggingface-hub-repos` | huggingface_hub 1.9.1 (source read, TASK-40: the hub cache is `HF_HOME/hub`, a repository's directory is `repo_folder_name`; a cached file is returned without a lock; a download writes `.incomplete` blobs under a file lock in `.locks`); host layout 2026-11-02; huggingface_hub 1.30.0 in the Nix container (TASK-352, a local hub endpoint): `hf_hub_download` fills `models--*` and `datasets--*` (blobs, snapshot links, `refs`) and `.locks`; a download in flight holds its `.incomplete` blob open; a process that mapped a snapshot file and closed it (as safetensors loads) holds only the mapping | in use (a mapped `safetensors` blob, a download's open `.incomplete`), `min_idle_for: 1h`; consumer tolerance between runs: a missing file is downloaded again | verified heuristic | (1) A run that resolved a cached repository and opens its files one at a time holds nothing between them; it fails once. (2) A symlink into the blobs from outside the cache (a `local_dir` made by huggingface_hub before 0.23, a model link another application or the user made) dangles until downloaded again. (3) Downloading again needs the network (`HF_HUB_OFFLINE` runs fail) and can be gigabytes. (4) Not rebuildable: a gated, private or removed repository or revision; the dated exception for an upstream that no longer serves a copy covers only `nix-user-cache` and `go-mod-cache` (safety.md, Producer liveness). (5) A file the user edited through a snapshot link changes the blob itself and is lost with it: the user's own work, outside the loss bound. | Decision pending (Mark's admission, TASK-40, including residuals 4 and 5): in no catalog. Container tests: `huggingface_hub_repos_real_hub_downloads_reclaim_then_redownload_and_runs_in_flight_are_kept` (a mapping process kept `in_use`, `map`; a download in flight kept, `fd`; residual 3 reproduced: `HF_HUB_OFFLINE` fails after the reclaim; residual 1 not reproduced), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352). Vectors: `huggingface-hub-repos` (builtins), `huggingface-hub-repos-run-between-files-and-local-dir-symlink-residual`. |
| `cargo-registry-src` | Cargo (source at commit 6238f6fd61bf read 2026-11-02, TASK-40: `unpack_package` writes `.cargo-ok` when an extraction completes and extracts a missing source again from its `.crate`; a build holds `CacheLockMode::Shared` on `$CARGO_HOME/.package-cache-mutate`, Cargo's own gc takes `MutateExclusive`; the global cache tracker drops the rows of removed sources); host registry layout; Cargo 1.99.1 in the Nix container (TASK-352, a local sparse registry of crates the real `cargo package` made): a build extracts its dependencies with `.cargo-ok`, `cargo build --offline` extracts reclaimed ones again from `registry/cache` without a download; a build in flight holds `.package-cache-mutate` shared for its whole run, and each `rustc` of a registry crate runs with its working directory in that crate's source (and `CARGO_MANIFEST_DIR` naming it) | in use (a `rustc` reading a source), `has: .cargo-ok`, `min_idle_for: 1h`; consumer tolerance between builds: Cargo extracts a missing source again | verified heuristic | (1) diskgc does not take Cargo's cache lock: a build between two `rustc` invocations holds only that lock, outside the candidate, and a `rustc` started on the deleted crate fails once. (2) Extracting again is offline only while the crate's `.crate` is still in `registry/cache` (Cargo's automatic gc removes long-unused ones); otherwise it is downloaded. (3) An editor or debugger showing a dependency's source finds it gone until the next build. (4) A delete stopped part-way after entries that sort before `.cargo-ok` (a `.cargo/` directory, dotfiles such as `.appveyor.yml`) leaves the marker, and Cargo builds from the incomplete source until the leftover is deleted: not one failed run. (5) A dependency source the user edited by hand (a debug print, a `[patch]` path into the registry) is the user's own work, outside the loss bound (remedy `.diskgc-keep` or `protect`). | Decision pending (Mark's admission, TASK-40): in no catalog. Not admissible as it stands: residual 1 has a known fix (take Cargo's `MutateExclusive` lock before deleting, a protocol of its own, not built, TASK-329), residual 4 needs the completion marker unlinked first (not built), and residual 5 needs Mark's decision. Container tests: `cargo_registry_src_real_cargo_extracts_reclaim_then_reextract_and_a_build_in_flight_fails_once` (residual 1 reproduced, TASK-329: the crate being compiled is kept `in_use`, `cwd`, while a crate compiled after it is deleted under the build, whose `rustc` then fails, "could not compile `top`", No such file or directory; the offline retry succeeds; residual 2's offline side shown), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352); residual 4 not reproduced. Vectors: `cargo-registry-src` (builtins), `cargo-registry-src-build-holding-cache-lock-residual`, `cargo-registry-src-stopped-delete-keeps-cargo-ok-residual`, `cargo-registry-src-hand-edited-source-residual`. |
| `node-gyp-headers` | node-gyp 11.5.0 (source read, TASK-40, bundled with npm of Node.js 22.23.3: the dev directory is `env-paths('node-gyp').cache`; `configure` runs `install --ensure`, which installs a version whose directory is missing and otherwise checks only its `installVersion`; the install copies `installVersion` with the headers, in directory order); node-gyp 12.4.0 (npm of Node.js 24.21.0) in the Nix container (TASK-352): `install --target` from a local dist fills `.cache/node-gyp/VERSION`, and a build for a missing version installs it again (a Node whose own headers its prefix holds, as Nix's does, never uses the cache for its own version: `use_prefix_to_find_headers`); a build between two compiles holds nothing there | in use (a compiler reading a header), `has: installVersion`, `min_idle_for: 1h`; consumer tolerance between builds: node-gyp installs a missing version again | verified heuristic | (1) A native build between two compiler invocations holds nothing there; the next compile fails once. (2) Installing again downloads the headers (unless `--nodedir` is given); offline builds fail until then. (3) Not rebuildable: headers installed from a `--tarball` or a custom `--dist-url` that is gone. (4) A delete stopped part-way leaves `installVersion` (it sorts after `include/`) with headers missing; node-gyp takes the version as installed, and every native build for it fails until the leftover is deleted: not one failed run. | Decision pending (Mark's admission, TASK-40): in no catalog. Not admissible as it stands: residual 4 needs the completion marker unlinked first (not built), and residual 3 needs Mark's decision. Container tests: `node_gyp_headers_real_node_gyp_install_reclaim_then_reinstall_and_a_build_between_compiles_fails_once` (residual 1 reproduced: the second compile fails, the retry installs and builds; fresh headers `not_idle`), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352); residual 4 not reproduced. Vectors: `node-gyp-headers` (builtins), `node-gyp-headers-build-between-reads-residual`, `node-gyp-headers-stopped-delete-keeps-install-version-residual`. |
| `electron-download-cache` | @electron/get 5.1.0 (source read, TASK-40; the host's Electron 33.4.5 zip came from @electron/get 2.x, same layout: cache root `env-paths('electron').cache`, one directory per SHA-256 of the download URL's directory; on a hit it returns the cached path, a new download is renamed in); host layout (an Electron 33.4.5 zip); @electron/get 5.1.0 with extract-zip 2.0.1 in the Nix container (TASK-352, the electron package's install steps from a local mirror; the library reads no variable, the electron package's script passes `electron_config_cache` as its `cacheRoot`): an extraction in flight holds the zip open, an install between its cache hit and the extraction holds nothing | in use (an install extracting the zip), `min_idle_for: 1h`; consumer tolerance between installs: a missing zip is downloaded again | verified heuristic | (1) An install between its cache hit and opening the zip holds nothing; it fails once. (2) Downloading again needs the network. (3) Not rebuildable: a zip from a custom `ELECTRON_MIRROR` or a fork's build that no longer serves it. (4) No marker shows a directory is @electron/get's: whatever else lies directly in `.cache/electron` is deleted with it. | Decision pending (Mark's admission, TASK-40, including residual 3): in no catalog. Container tests: `electron_download_cache_real_electron_get_reclaim_then_redownload_and_an_extraction_in_flight_is_kept` (an extraction in flight kept `in_use`, `fd`; residual 1 reproduced: `ENOENT` on the zip, the retry downloads again; residual 4 reproduced: a hand-made directory in `.cache/electron` is deleted), `candidate_catalog_runtime_cache_family_matches_the_development_build` (TASK-352). Vectors: `electron-download-cache` (builtins), `electron-download-cache-install-between-hit-and-extract-residual`. |
| `claude-code-tmp` | Claude Code 2.1.280 (host 2026-10-24: each of 18 live interactive sessions held its own `<session>/tasks` directory open; on 2026-10-18, TASK-68, idle sessions held nothing in their directory: undocumented, version-dependent behaviour) | in use only; no floor (the tier's idle filter) | none | Not a built-in. Was: A session that ended and is resumed later (`--resume`) finds its scratchpad and task output gone; under a version that holds nothing, a live session's directory too. The loss is the agent's scratch files and background-task output, which can hold a user's work (scripts, patches, reports). Counterexample vector (user probe `claude-code-tmp-probe`) `claude-code-tmp-live-session-without-held-file-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). Re-admission only under a new name, with the held directory pinned per version, a floor, and Mark's decision on resumed sessions. |
| `chromium-tmp` | every Chromium-based browser and Electron application: the `org.chromium.Chromium` temporary-name prefix belongs to Chromium's base library, shared by all builds not branded Chrome (host 2026-10-24: 312 directories, of them 245 component-updater URL fetcher and 5 unpacker scratch of Brave and Chromium, 62 unnamed; 8 files; live Brave and Chromium) | in use only; no floor; no presence marker covers every producer | none | Not a built-in. Was: Not all can be named: a live application's temporary files and directories under the shared prefix, whose uses were not enumerated. For the updater scratch alone, one failed component update, retried by the updater. Counterexample vector (user probe `chromium-tmp-probe`) `chromium-tmp-live-updater-scratch-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). A narrower name for the updater scratch could be a verified heuristic. |
| `playwright-tmp` | Playwright 1.56.1 (`verified_with`); 1.59.1 running on the host 2026-10-24: a 5-day-old driver with headless Chromium held its `playwright_chromiumdev_profile-*` (descriptors and mappings), while its `playwright-artifacts-qHgpwa`, created in the same second, was held by no process | in use only; no floor | none | Not a built-in. Was: A live session's later download, trace or video fails until its browser is relaunched; the glob `playwright*` also matches whatever else a tool names so. The loss is not a cache miss, and a live producer leaves no evidence. Counterexample vector (user probe `playwright-tmp-probe`) `playwright-tmp-live-artifacts-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `zephyr-tmp` | no tool: on the host, a project script (`mktemp -d /tmp/zephyr-h5e5-run.XXXXXXXX`; 2,578 such and 213 `zephyr-h5-run.*`) removes its run directory on success and keeps it on failure, printing its Renode and UART log paths for the user; hand-made directories (`zephyr-h5-clock-patch`, `zephyr-usart-patch.*`, without `.git`) and Git checkouts match too | in use only; no floor | none | Not a built-in. Was: Failure diagnostics the user was pointed to; hand-made work without a checkout; a live run's directory between two steps. Counterexample vector (user probe `zephyr-tmp-probe`) `zephyr-tmp-kept-failure-diagnostics-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `renode-tmp` | Renode 1.17.0 TemporaryFilesManager (source read, renode-infrastructure `f3d5d7b7d`): one `renode-<pid>` per process; `GetTemporaryFile` creates each file and closes it; at start and exit Renode removes `renode-<pid>` directories of pids that are not alive, never `*-crash` (its crash reports) and nothing under `--keep-temporary-files` | in use only; no floor | none | Not a built-in. Was: A live Renode's directory holds no descriptor between uses, so a running emulation loses the files it created; crash reports (`renode-<pid>-crash`, 29 on the host) and directories kept on request are deleted. Counterexample vector (user probe `renode-tmp-probe`) `renode-tmp-live-emulation-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). Re-admission: a new name over `renode-<pid>` only, without `-crash`, under an Owner PID contract (Renode's own rule). |
| `xdg-cache-entry-v2` | any application under `$XDG_CACHE_HOME` (XDG Base Directory Specification 0.8 calls it non-essential data; the profile's own exclusion list records applications that keep sessions, credentials, models and backups there) | in use, `min_idle_for: 30d`, `checkouts: refuse`, exact-name exclusions | none | Not a built-in. Was: Not all can be named: any unknown application's state, or a cache that is the only copy; a dormant application that reads its cache only when it starts. Counterexample vector (user probe `xdg-cache-entry-v2-probe`) `xdg-cache-entry-v2-unknown-app-state-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `node-modules` | npm 11.19.0 with Node.js 24.21.0 (the development host's install layout, read-only dry runs 2026-11-02 and 2026-11-05; in the container, TASK-350: real installs of a project and a workspace, restored by `npm ci`); TASK-79's container probe: reading `node_modules` changed neither mtime nor ctime. pnpm (its store links: vector `node-project-pnpm-layout-eligible`), Yarn and Bun: not verified; consumers are `node` and editors | `liveness_contract: node-project`, `min_idle_for: 30d`, `checkouts: refuse`: no change in the project, no read in its `node_modules` directories (project-wide when one links out; this narrowing is PENDING Mark's confirmation), no visible use (identity, held paths, environment, command line); kept while an outer `node_modules` exists | verified heuristic: project activity | Those named in safety.md, Project dependency directories (Residuals), among them a cached consumer launched through a path outside the project, a removal-only change, and under the pending narrowing a project whose own code but no dependency was read; plus safety.md, A process's paths (a mapping is matched by identity only). The loss in a residual is a reinstall, except hand-edited dependency files, a dated exception to the admission rule (Mark, 2026-10-23; remedy `.diskgc-keep` or `protect`). False positive, accepted by Mark with "admit current as-is" for its 0.2 admission (2026-11-05), not in effect while he holds the profile: a `node_modules` beside a `package.json` with no lockfile and no `node_modules/.package-lock.json`, such as an extracted application bundle or a vendored dependency tree (seen on the development host, 2026-11-05), matches, and once its project is idle for 30 days it is deleted although npm cannot reinstall it; a successor requiring a lockfile is TASK-357's. A package installed into it by hand without being recorded in the manifest (`npm install --no-save`) is not restored by a reinstall either. | Shipped, admitted by Mark, 2026-10-23 (TASK-79): development library only, in no release catalog: Mark held it out of 0.2 on the `npm install --no-save` residual ("No, hold node-modules", 2026-11-06, TASK-365, withdrawing its admission to 0.2 of 2026-11-05, TASK-359), until a successor (TASK-357's, which also targets the no-lockfile false positive) handles that residual or he decides on it. Rule 2's narrowed scope for `node-project` is the orchestrator's, PENDING Mark's confirmation. Nested dependency directories, and one an outer `node_modules` would replace, are never candidates, so a removal cannot silently switch Node to a hoisted copy. Vectors: `node-project-*` in crates/diskgc-model/tests/vectors/liveness, `node-modules` in vectors/builtins. Container tests: `node_modules_real_npm_projects_reclaim_only_idle_projects_then_npm_ci_restores`, `node_modules_live_node_processes_keep_their_projects`, `node_modules_home_level_install_filters_every_project_below_it`, `candidate_catalog_project_tree_family_matches_the_development_build` (the release build's catalog path, with `node-modules` on the test-only candidate list and the clock offset, so past the floor); the gate-lifted package refuses it (`release_candidate_catalog_refuses_every_excluded_builtin`). A home-level install (`~/package.json` and `~/node_modules`, as on the development host) keeps every project below it `filtered`, so the profile reclaims nothing there. |
| `python-venv` | CPython 3.13.15 `venv` (the development host's layout, read-only dry runs 2026-11-02 and 2026-11-05); CPython 3.14.7 `venv` and uv 0.13.0 `uv venv` in the container (TASK-350); TASK-79's container probe: reading `.venv` changed neither mtime nor ctime. virtualenv: not verified; consumers are `python` and editors | `liveness_contract: python-project`, `min_idle_for: 30d`, `checkouts: refuse`: no change in the project, no read in its `.venv` directories (project-wide with an editable install or a link out; this narrowing accepted for `python-venv` with its admission, Mark 2026-11-05), no visible use (identity, held paths, environment, command line) | verified heuristic: project activity | Those named in safety.md, Project dependency directories (Residuals), among them a cached consumer launched through a path outside the project, a removal-only change, and under the narrowing a project whose own code but no dependency was read; plus safety.md, A process's paths (a mapping is matched by identity only). A shell that sourced `activate` and then left the project names nothing in it (`VIRTUAL_ENV` is exported after `exec`), so its environment can be reclaimed while it runs and its next `python` is not found (container test). The loss in a residual is a reinstall, except two dated exceptions to the admission rule: hand-edited dependency files (Mark, 2026-10-23) and packages installed by hand into the environment without being recorded in the manifest, which a reinstall from it does not bring back (Mark, 2026-11-05); remedy for both `.diskgc-keep` or `protect`. | Shipped, admitted by Mark, 2026-10-23 (TASK-79); in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-05, TASK-359, with rule 2's narrowed scope and the residuals named here). A `.venv` inside another dependency directory, or without `pyvenv.cfg` or a manifest beside it, is never a candidate; an editable install's checkout keeps it (`contains_checkout`). Vectors: `python-project-*` in crates/diskgc-model/tests/vectors/liveness, `python-venv` in vectors/builtins. Container tests: `python_venv_real_venv_and_uv_projects_reclaim_only_idle_projects_then_recreate`, `python_venv_live_interpreter_and_activated_shell_keep_their_projects`, `python_venv_activated_shell_that_left_its_project_is_a_named_residual` (the residual of a consumer that names nothing in the project: an activated shell that left it), `candidate_catalog_project_tree_family_matches_the_development_build` (the release build's catalog path with the clock offset, so past the floor). Through the gate-lifted package, which has no clock offset, only the keep side of the floor: `release_candidate_python_venv_keeps_projects_inside_the_30_day_floor`. |
| `nix-shell-tmpdir` | `nix develop`'s rc runs `mktemp -d -t nix-shell.XXXXXX` and exports it after `exec`; no exit cleanup (left after a normal exit, Nix 2.34.8) | none: the name records no owner; a dormant shell shows no use | none | Not a built-in. | Excluded. Counterexample: the dormant-temp-users container test. The class is fixed at the producer, upstream (TASK-123). |
| `mktemp-dir` | any program | none | none | Not a built-in. | Excluded. Counterexample: a shell that exports `TMPDIR=$(mktemp -d)` after `exec`. |
| `tmp-entry` | any program | none | none | Not a built-in. | Excluded for good: there is no one producer, and every counterexample above applies. |
| `containers-image-tmp` | containers/image and Buildah spools in `/var/tmp`; the real producers' lifecycle is unverified | `min_idle_for: 3d` only | none | Not a built-in. | Withdrawn (Mark, 2026-10-23). Counterexample: `vm_containers_image_dormant_spool_exposes_liveness_gap`. Re-admitted only under a new name after TASK-125 verifies the real lifecycle in a container test. |
| `gradle-caches` | Gradle; its daemons lock cache entries only on demand | none for diskgc: Gradle's own retention-based cleanup owns the directory, but there is no cleaner diskgc could invoke on a measured directory, so it is not *delegated* | none | Not a built-in. | Excluded; tune Gradle's cache retention instead. |
| `maven-repo` | Maven, `mvnd`, IDEs | none: a JVM holds a jar open only while it is loaded; the repository has no lock by default | none | Not a built-in. | Excluded. |
| `nix-garbage` | the system Nix daemon's own garbage collector, CppNix 2.34.8 (TASK-36 container scenarios) | Nix's GC roots, temporary and runtime roots, and its GC lock (kind `nix-store`), after diskgc's binding and root check ([safety.md](safety.md#nix-store)), not a `where` predicate | delegated | Nix collects what its own roots do not reach, by its own rules; a root database other than the bound daemon's, and processes outside the daemon's PID namespace, are not seen (safety.md, nix-store: What it does not prove). A GC's freed space can appear only at its end in an optimised store. | Shipped; in the 0.2 catalog, not 0.1's (admitted by Mark, 2026-11-04, TASK-335). Vectors: `nix-*` (backend). Container tests: the Nix GC scenarios (testing.md), the release package's own `release_candidate_nix_garbage_dry_run_then_bounded_gc`. |

### Release notes (next release, draft)

**Status: draft** for 0.2, whose scope Mark decided in TASK-276 (the
descriptor's 0.2 row with a reclaiming `watch`, 2026-11-05; the NixOS
module, 2026-11-06). This describes the `release-candidate` package as master
builds it ([Built-in profile library](#built-in-profile-library)): the
package ships this section as `RELEASE-NOTES.md`, under a header that
lists what it runs and installs. 0.1.0's own notes are frozen at its tag
(v0.1.0 on `release-0.1`, af3d503): `git show v0.1.0:docs/reference.md`.
The default development package stays sandbox gated.

**Changes since 0.1.0** (on master, in no release yet). Each ends with
whether 0.1.0 has the flaw or the old behaviour, and why, from reading
0.1.0's code (`git show v0.1.0:FILE`), not from running it, unless a
line says it was reproduced.

- TASK-242: `cargo-target` discovery no longer descends into rootless
  Podman or Docker image layers. 0.1.0: yes; it has no container-storage
  check, only the layers' permission bits kept a Cargo target inside one
  from being planned for deletion (static reading).
- TASK-260: `protect` and `!profile` also cover a candidate reached
  through a bind mount of a subdirectory of the protected or negated tree.
  0.1.0: yes for a bind of a subdirectory of the match. It compared a
  candidate with a match by path, then by the identities of their
  ancestors, so a bind of a directory both lie under was seen; the bind's
  source, a directory below the match, was never compared (static
  reading).
- TASK-44: `--allow-root` is refused unless diskgc runs as root, and with
  `CAP_SYS_PTRACE` a process the kernel refused to show (an LSM) keeps its
  candidate instead of being excused. 0.1.0: yes; it accepted
  `--allow-root` without root and without `CAP_SYS_PTRACE`, and excused
  such a refused process whatever its capabilities (static reading).
- TASK-277: without the provisioned host lock `/run/diskgc/lock`, a
  non-root reclaim takes a per-user lock (Install, item 2). 0.1.0: exits 2
  (`cause: provisioning`); it has no per-user lock (static reading).
- TASK-268: an overlay, an `ecryptfs`, `aufs` or `shiftfs` mount, or a
  FUSE, network or `autofs` mount inside a `protect` or `!profile` match
  (a Cargo target, or through a bind) stops the run while it is mounted:
  the directories it stores its data in may hold the candidate
  ([safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate)).
  On master before this change such an overlay escaped the overlay
  refusal (reproduced in the model and in a container). 0.1.0: yes, more
  broadly; it refused no overlay with a `protect` or `!profile` match at
  all, which came with TASK-260's review (static reading).
- TASK-268 review: a candidate on a loop-mounted image is kept when the
  image file is protected or negated (and the reverse), read from the
  loop device's `backing_file`; while any match exists, an unreadable one
  is an error. 0.1.0: yes; it read no loop device (static reading).
- TASK-268 review (8edb9f5): a Cargo target in a `protect` or `!profile`
  match whose entries vanish while it is inspected is inspected again
  (three times in all) and, if it never settles, stops the run; before,
  it dropped out of the cached matches and an old candidate inside it
  could be deleted (reproduced on master before the fix, in the model).
  0.1.0: yes; it has the
  same cached enumeration and dropped such a target the same way (static
  reading).
- TASK-268 review (63c49aa): a `CACHEDIR.TAG` made after the enumeration
  in a directory on a candidate's way down from a Cargo `protect` or
  `!profile` root makes the candidate's check enumerate the matches again;
  before, that new Cargo target's old entries could be deleted
  (reproduced on master before the fix, in the model). 0.1.0: yes; its
  cached matches had the same
  blind spot, which its own documentation named (static reading).
- TASK-263 (pending Mark's confirmation, TASK-296; TASK-298): whose
  tiers rely on the in-use check (`fs`, `cargo-target`), `ensure` below
  `--min-free`,
  its dry run and a reclaiming `watch` are refused (exit 2, `cause:
  safety`) under `hidepid` or in a PID namespace other than the initial
  one, so in a container: the in-use check cannot see every process
  there ([safety.md](safety.md#in-use), Visibility). An `ensure` whose
  space is already there still exits 0 (`met`). A CI hook that ran 0.1.0 inside its job container
  must run diskgc on the runner host to reclaim. 0.1.0: yes; it ran there and judged candidates by the
  processes it could see, so a Cargo target whose program ran outside the
  container (Cargo holds no lock while it runs) could be deleted (static
  reading).
- TASK-169 (the case where diskgc cannot ask the device is pending
  Mark's confirmation, TASK-296): a candidate holding the backing file
  of a configured loop device (`udisksctl loop-setup`, `losetup`,
  mounted or not) is `in_use` where diskgc can ask the device (root,
  the `disk` group); where it
  cannot, a device whose file may be the user's keeps every candidate
  `in_use_unknown` (a deleted one: those on its filesystem) until it is
  detached ([safety.md](safety.md#in-use), Loop devices). 0.1.0: yes; it
  read no loop device, so it deleted such an image, whose data was gone
  once the device detached (static reading).
- TASK-299: three holes in the loop rules close (static configurations,
  found by review, reproduced in the model). A root-owned image that an
  idmapped mount of its own filesystem may show as the user's no longer
  clears candidates on that mount; a device opened through an overlay
  (by a deleted name, or as the device's own identity) keeps candidates
  off that overlay `in_use_unknown`, not only those on the overlay's
  device; and a `protect` or `!profile` check reads every member of a
  btrfs on loop devices, seeds included, not only the one mountinfo
  names ([safety.md](safety.md#in-use), Loop devices, with the
  configurations it does not see listed as limits). 0.1.0: yes, more
  broadly; it read no loop device (static reading). Seen on a btrfs:
  - the snapshot key (`ensure` and `watch`) becomes `uuid-<fsid>` where
    it was `dev-0-<N>`: one run starts cold, and the old `index-dev-*`
    files are left in the state directory;
  - a state directory on a btrfs now writes its snapshot where it was
    skipped ("cannot tell whether ... is on a loop device");
  - a `protect` or `!profile` check now stops the run where a btrfs's
    mount root cannot be reached, a btrfs below a match is covered by
    another mount, or its members cannot be read.
  These follow from reading the btrfs id at all: every descriptor diskgc
  held was `O_PATH`, on which the ioctl fails (`EBADF`), so the id was
  never read before.
- TASK-263 review: `skipped[].unknown_cause` says why an
  `in_use_unknown` could not be judged (Output). 0.1.0: no such field
  (static reading).
- TASK-269: a `protect` or `!profile` match removed after the tier
  enumerated it (a `cargo clean` of a protected target) no longer stops
  the run or the dry run (exit 3) at a candidate's first check: the
  matches are enumerated again, then the candidate is judged. 0.1.0:
  partly. Its later checks under the candidate's lock already enumerated
  again on such an error (`DeletePolicy::skip`), but its first check of a
  candidate against the `!profile` matches (Cargo and `fs` tiers) stopped
  the run with exit 3, deleting nothing more (static reading).
- TASK-267: a root that is a bind mount of a directory inside container
  storage (or lies below one) is refused like a root named inside it: the
  directories above its place in its filesystem are asked too, where the
  mount table shows them (only up to the namespace's root, never where a
  mount is attached: a snapper `/.snapshots` is not looked at). Reproduced
  in the model and in a container. 0.1.0: yes, more broadly; it has no
  container-storage check at all (TASK-242), bound or not (static
  reading).
- TASK-290: a candidate holding a mount point only by place (reached
  through a non-recursive or private bind of its parent) is
  `contains_mount`; before, its other entries were deleted and the mount
  point's directory failed with `EBUSY`; a Cargo profile directory that
  is a mount point by place is `contains_mount` too, where its covered
  entries were deleted (both reproduced in the model and in a container).
  0.1.0: yes; it has the same path-only mount test (static reading).
- TASK-286: a directory on another device than the one it is in, with
  no mount between (a nested btrfs subvolume or snapshot), keeps its
  candidate or Cargo target (`contains_mount`), and the tally does not
  enter it; another subvolume of `PATH`'s btrfs is `PATH`'s filesystem
  (Filesystem matching). 0.1.0: yes; its walk compared mount ids only,
  so it emptied a nested subvolume inside a candidate and removed its
  top (static reading).
- TASK-306: as root (`--allow-root`), a kernel thread's environment
  read that says `ESRCH` (Linux 6.18) is its answer in one round. Before,
  each was retried for 16 rounds, the scan outlasted its one-second
  freshness bound, and every root reclaim ended `in_use_unknown`
  (measured in the `vm-real-kernel` check). 0.1.0: yes on such a kernel;
  it has the same read rounds (static reading).
- TASK-307: a nested bcachefs subvolume (same device as its parent) and
  a subvolume in an overlay's btrfs upper layer keep their candidate
  (`contains_mount`), by `stx_subvol` and in the upper layer; on bcachefs
  without `stx_subvol` (before Linux 6.10) every directory is
  `unsupported_fs`. 0.1.0: yes; it compared mount ids only (static
  reading). Reproduced on master before the fix: the bcachefs case in
  the `vm-real-kernel` check and the model, the overlay case in the
  model.
- TASK-285: a delete action's cached `protect` and `!profile` matches are
  enumerated again when the mount table differs in anything from the one
  read before the enumeration (a rename above a bind's source, a bind
  mounted at a protected root), and a `CACHEDIR.TAG` above a Cargo
  source's root is looked up by place as well as by path. Before, a
  candidate could be deleted after such a change made it protected
  (reproduced in the model only). 0.1.0: yes. Its cached matches
  (TASK-200) were enumerated again only after a change inside the
  candidate, never on a mount-table change, and its overlap test missed a
  bind of a subdirectory of the match anyway (TASK-260) (static reading).
- TASK-270: an `ensure` that tallied writes its snapshot on every exit
  after the tally (not after a host lock held to the deadline, exit 75),
  and one whose `protect` could not be enumerated exits 0 when the final
  sample meets `--min-free` (the error stays in `errors`). 0.1.0: the
  snapshot part does not apply (it has no tally or snapshot); it exited 3
  in that `protect` case (exit code only; nothing is deleted either way)
  (static reading).
- TASK-272: `ensure` and `watch` check the configuration before they
  start the host (`/proc`, the kernel), so a bad configuration exits 2
  however broken `/proc` is. 0.1.0: yes; it exited 3 (`cause: host`) for
  a bad configuration with an unreadable `/proc` (static reading).
- TASK-280: `diskgc --version` names the commit of every build that is
  not a tagged release (`diskgc 0.2.0-dev (COMMIT)`). 0.1.0: not
  affected; it is the tag and prints `diskgc 0.1.0`.
- TASK-289: the package ships the two `watch` unit templates (Install,
  item 5), and `--help` shows only the commands this package runs.
  0.1.0: not affected; it ran no `watch` and shipped no templates.
- TASK-362: the package runs `watch`, reclaiming and `--tally-only`
  (What it does; its limits under Known limits), and the flake's NixOS
  module ships with it (Install, item 6). 0.1.0: not affected; it ran no
  `watch` and had no module.
- TASK-335: the catalog adds `nix-garbage`, a garbage collection of the
  system Nix store (What it does; its limits under Known limits). 0.1.0:
  not affected; it shipped no Nix kind.
- TASK-359: the catalog adds five project-tree sources, `pytest-cache`,
  `python-pycache`, `mypy-cache`, `python-venv` and `node-compile-cache`
  (What it does; their limits under Known limits). 0.1.0: not affected;
  it shipped none of them.
- TASK-360: the catalog adds four package-manager caches cleaned by the
  tool's own cleaner, `pip-cache`, `uv-cache`, `pnpm-store` and `ccache`
  (What it does; their limits under Known limits); the package runs
  exactly their four commands and no other command action.
  The checkout guard tolerates the empty `sdists-v*/.git` file every uv
  cache holds, and only that. 0.1.0: not affected; it ran no command
  action.
- TASK-370: the catalog adds three caches deleted whole, `go-mod-cache`,
  `bun-cache` and `nix-user-cache`, and four build-output sources,
  `cmake-build-anchored`, `meson-build-anchored`,
  `autotools-build-anchored` and `west-build-anchored` (What it does;
  their limits under Known limits). TASK-367: those four never take a
  build directory they recognise as inside another build system's tree
  (near Buildroot's or OpenWrt's stamps, or below a Buildroot output,
  Yocto TMPDIR, Bazel output base or npm's `node_modules`). 0.1.0: not
  affected; it shipped none of them.
- TASK-291: a command line that is not understood (a mistyped flag) is
  exit 2 naming it before anything on the machine is read, as TASK-272
  made a bad configuration. 0.1.0: yes; it ran the host's startup first,
  so with an unreadable `/proc` a typo exited 3 (`cause: host`), and with
  a kernel older than 5.8 it named the kernel instead of the typo (exit 2
  either way) (static reading; reproduced on master before this change
  with a tmpfs over `/proc`).
- TASK-340: after three candidates in a row of a tier end `in_use_unknown`
  on a busy host (stale or unsettled in-use scans), the tier backs off and
  checks again within its deadline instead of skipping its rest
  unchecked; a storm that lasts until the deadline ends the run
  `deadline` (124), no longer `exhausted` (1) with every remaining
  candidate counted `kept` (dogfood 2026-11-04: 146,467 `nix-shell-rc`
  candidates given up after 159 s of a 30 h run;
  [safety.md](safety.md#in-use), Freshness). Three kept by a process
  diskgc cannot inspect still skip the tier's rest, which now names that
  process (`unknown_cause: unreadable_process`), not a busy host. 0.1.0:
  yes; it has the same give-up after three (static reading; reproduced on
  master before this change in the model).
- TASK-348: a process whose reads did not settle (its tasks exiting or
  changing under every round: "no trustworthy read in 16 rounds") is a
  busy host's (`unknown_cause: busy`), so three in a row make the tier
  back off instead of skipping its rest; only a process that can stay
  uninspectable while it runs (`unreadable_process`) still skips it, and
  the human summary no longer tells the user to run again when the host
  is less busy for those (dogfood 2026-11-05: an exiting `rust-lld` of a
  concurrent build made a `nix-shell-rc` tier skip 127,645 candidates
  unchecked; [safety.md](safety.md#in-use), Freshness). 0.1.0: yes; it
  classes such a process as one that cannot be inspected (static
  reading; reproduced on master before this change in the model).
- TASK-364: a scan that judges nothing on a busy host (its process list
  kept growing, a process's reads did not hold still) no longer costs the
  candidate it was made for after one 20 ms retry: the reclaim walk waits
  it out (TASK-340's pauses, cut at the deadline, after the candidate's
  locks are released) and checks the candidate again, whatever the size
  of the tier, and a dry run's plan waits before judging. A storm until
  the deadline ends `deadline` (124), the candidate waited for kept
  `busy`; before, a tier of one or two candidates, and every dry run,
  ended `exhausted` (1) or planned `in_use_unknown` with the deadline
  unused (TASK-317's gate: `release_candidate_pip_cache_*`, ~119 s of
  120 s left). A process that has not held still for 30 s is taken to be
  stuck: `unreadable_process`, naming it, instead of holding the tier to
  its deadline. Stale scans keep TASK-340's three in a row. 0.1.0: yes;
  it has the one retry (static reading; reproduced on this change's base
  in the model).
- TASK-366: for the project contracts (`node-modules`, `python-venv`), a
  process whose command line did not hold still to be read (exiting
  while it was read) is a busy host's, waited out as above, no longer
  "its command line could not be read, so it may name the project" for
  every project of the run; a command line that cannot be read for a
  lasting cause, or one stuck for 30 s, still keeps every project. 0.1.0:
  no (it has no project contracts).

**What it does.** One command frees space once, a service keeps it
free, two read-only ones explain it:

- `diskgc ensure --min-free SIZE PATH` brings the filesystem of `PATH` up
  to a free-space goal, once, and exits with a code a CI pre-job hook or
  an agent harness branches on ([Exit codes](#exit-codes-ensure)); with
  `--dry-run` it only plans, with `--json` it prints one report object.
  It runs a configuration's tier groups, or one `--select` expression
  with `--roots`, under the host lock, with a cooperative `--timeout`.
- `ensure` below `--min-free` first tallies what its tiers find (a
  synchronous tally, bounded by `--tally-budget`) and saves that index as
  a snapshot in the user's state directory, which its next run starts
  from; `--no-index` neither reads nor writes it ([`ensure`](#ensure)).
- `diskgc watch --min-free SIZE --target-free SIZE PATH` (new in 0.2) is
  the long-running form of `ensure`, run as a service (Install, items 5
  and 6): it samples `PATH`'s filesystem every `--interval` (default
  `10s`), and when available space falls below `--min-free` it runs one
  reclaim cycle, the same walk with the same checks as `ensure`, up to
  `--target-free`; between the two it deletes nothing. Each cycle has its
  own cooperative `--timeout` (default `10m`) and backs off after a cycle
  that ends below `--min-free` (a minute, doubling, after one the host
  may recover from; 15 minutes after one that took everything it could).
  A background tally at idle priority keeps its index warm and writes the
  snapshot `ensure` starts from. It stops on SIGTERM, SIGINT or SIGHUP
  with exit 0; with `--json` it writes one JSON line per cycle
  ([`watch`](#watch)). `diskgc watch --tally-only PATH` only keeps the
  snapshot warm and never deletes ([Tally only](#tally-only)).
- `diskgc check` validates a configuration without touching the machine;
  `diskgc profiles list|show|test` shows the built-in catalog.
- The catalog is twenty sources and the seven idle filters (`idle-1h` to
  `idle-30d`): `cargo-target` (Cargo `target/` directories under supplied
  roots, reclaimed by Cargo's lock protocol), `nix-shell-rc` (rc
  directories of ended `nix-shell` and `nix develop` shells directly in
  `/tmp`; Owner PID), `pytest-tmp` (pytest sessions the pytest
  contract permits), `nix-garbage` (new in 0.2: the dead paths of a
  multi-user `/nix/store`, collected by one `nix-store --gc --max-freed
  NEED` through the system Nix daemon as your user, where NEED is what
  is still missing; Nix decides what is dead, and GC roots, profiles and
  generations are never removed) and five project-tree sources, also new
  in 0.2: `pytest-cache` (`.pytest_cache` up to three levels below a
  supplied root), `python-pycache` (`__pycache__` through five levels
  below a supplied root), `mypy-cache` (`.mypy_cache` through five
  levels, only with mypy's `CACHEDIR.TAG` in it), `python-venv` (a
  `.venv` holding `pyvenv.cfg` beside a Python project manifest, through
  five levels and outside hidden directories, once its whole project
  shows no change, no read in its environments and no visible use for
  30 days) and `node-compile-cache` (Node's compile cache in `/tmp` and
  in the shell temp directories below it). Those five are deleted only
  after diskgc's own checks: never while a process it can see holds a
  file in them, and never with a version-control checkout inside.
  Also new in 0.2, four package-manager caches that diskgc does not
  delete itself: it runs the tool's own cleaner on the cache it measured,
  after the same checks. For these four a run of the tool in flight
  survives its own cleaner, in the tests that held one (uv and pnpm make
  it wait on their lock; pip and ccache create again whatever directory
  they write into, so such a run downloads or compiles again at most):
  `pip-cache` (`pip cache purge` on `.cache/pip`), `uv-cache` (`uv cache
  clean` on `.cache/uv`), `pnpm-store` (`pnpm store prune` on
  `.local/share/pnpm/store`) and `ccache` (`ccache --clear` on
  `.cache/ccache` and `.ccache`), each below a supplied home root. The
  cleaner must be on `PATH`: a tier whose tool is not installed is
  skipped (`missing`), and whatever version is first on `PATH` runs
  (diskgc checks no version). Like every command action, never as root.
  Also new in 0.2, three caches diskgc deletes whole, each below a
  supplied home root: `go-mod-cache` (Go's module cache `go/pkg/mod`,
  whose read-only directories diskgc makes writable to delete them),
  `bun-cache` (`.bun/install/cache`, or `.cache/.bun/install/cache`) and
  `nix-user-cache` (Nix's Git, tarball and evaluation caches in
  `.cache/nix`); the tool downloads or evaluates again what it misses.
  And four kinds of build directory, through five levels below a
  supplied project root, each only while the source it was configured
  from still exists above it, and not one diskgc recognises as inside
  another build system's tree (a Buildroot, OpenWrt or Yocto package
  build, a Bazel output base, an addon in npm's `node_modules`; Known
  limits names the rest):
  `cmake-build-anchored` (a CMake build directory of any name),
  `meson-build-anchored` (a Meson build directory),
  `autotools-build-anchored` (an out-of-source `configure` build) and
  `west-build-anchored` (a Zephyr `build/` directory). `nix-shell-rc` and
  `pytest-tmp` keep anything changed in the last hour (`min_idle_for:
  1h`), the four build sources anything changed in the last three days
  (`min_idle_for: 3d`); the three caches have no such floor. The limits
  of each source are its row in
  [Liveness contracts](#liveness-contracts), and those of `nix-garbage`,
  the project-tree sources and the 0.2 caches and builds also under
  Known limits. Select
  `nix-garbage` on its own (`--select profile:nix-garbage /nix/store`, or
  a tier of its own): it takes no filter and needs no `--roots`. The
  project-tree sources but `node-compile-cache` need the directories to
  look in, as `--roots` or a tier's `roots:` (your projects' parent, such
  as `--select profile:python-venv --roots ~/src ~`); the cache sources
  need the home directory as their root (`--select
  profile:uv-cache --roots ~ ~`), the build sources the directories
  holding your projects (`--select profile:cmake-build-anchored --roots
  ~/src ~`). A configuration may
  use `profile_policies` only to narrow a built-in's checkout guard; user
  profiles and overrides are refused.

**Install.** The package holds:

1. `bin/diskgc`, a static musl binary with no run-time dependencies: put
   it on `PATH`.
2. `lib/tmpfiles.d/diskgc.conf`, which creates the host lock
   `/run/diskgc/lock`: as root, `install -Dm0644 lib/tmpfiles.d/diskgc.conf
   /etc/tmpfiles.d/diskgc.conf` (a minimal system may have no
   `/etc/tmpfiles.d` yet) and run
   `systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf` (without
   systemd, create the directory and file it describes at every boot).
   Needed for root and the system service; without it a user's reclaim
   takes a per-user lock instead, which serialises only that user's
   reclaims ([safety.md](safety.md#host-lock)). As root without the lock,
   a run that needs to reclaim exits 2 with `cause: provisioning` and
   prints these commands; a run whose goal is already met only warns.
3. `share/doc/diskgc/examples/example.yaml`: copy it to
   `~/.config/diskgc/config.yaml` or `/etc/diskgc/config.yaml`, replace
   its Cargo root with directories you own, and run `diskgc check`. It is
   not installed as an active default.
4. `share/doc/diskgc/`: a README (install and first run), the MIT
   `LICENSE`, these release notes (`RELEASE-NOTES.md`, cut from
   `reference.md` at build time),
   `reference.md` and [safety.md](safety.md), so that the notes' links
   resolve (links into the design documents do not).
5. `lib/systemd/user/diskgc-watch@.service` and
   `lib/systemd/system/diskgc-watch@.service`, the `watch` unit
   templates, one instance per filesystem
   ([Watch as a systemd service](#watch-as-a-systemd-service)): the
   thresholds go in the instance's environment file
   (`DISKGC_WATCH_ARGS=--min-free 30G --target-free 60G`; a CI runner's
   warm index, `--tally-only --min-free 50G`). The user template needs
   no root; the system template needs the host lock (item 2) and a
   `User=` drop-in. `Restart=always` restarts an instance 30 s after any
   end but `systemctl stop` (a SIGHUP applies an edited configuration
   that way), except exit 2, a refusal at start, which stays failed;
   diskgc's message is in the journal (`systemctl status` usually shows
   it; a process that exits at once can lose its unit field to a
   journald race, measured in the VM test).
6. On NixOS, the flake's `nixosModules.default` (`services.diskgc`) at
   this release's tag, whose default `package` is this package: it
   renders the configuration, checks it with this binary's `diskgc check`
   when the system is built (so a profile or tier 0.2 refuses fails the
   build), provisions the host lock and starts the instances (its
   options: the source tree's docs/architecture.md, NixOS module). From
   master the
   module's default is the development package, which does not delete.

Start with a dry run (`diskgc ensure --dry-run --min-free 50G PATH`); it
reads only and lists what a reclaim would take and why the rest is kept.
`--min-free` must be below the filesystem's size, or the run exits 2.
The example's tiers take only `/tmp` leftovers idle for a day and Cargo
targets idle for 30 days, so on a freshly set-up machine the dry run keeps
everything as `not_idle`; `--select 'profile:cargo-target' --roots DIR`
without a filter shows what a reclaim of Cargo targets would take now.
A reclaimed Cargo target keeps its directory with `CACHEDIR.TAG`,
`.rustc_info.json` and Cargo's lock files (a few KiB): diskgc never deletes
a Cargo lock file, and Cargo builds into it again as usual.
Measured for nightly Cargo's `-Zfine-grain-locking` (2026-10-16 nightly):
it adds a lock file per unit, `<profile>/build/<package>/<hash>/.lock`, so
diskgc never reclaims a target such a Cargo built (`contains_special`), and
skips it as `locked` while such a build runs ([safety.md](safety.md),
`cargo-target`, Lock files).

**What `pytest-tmp` frees on a laptop.** Less than the size of
`/tmp/pytest-of-<user>` suggests (TASK-65 walk, TASK-157):

- Sessions directly in `/tmp/pytest-of-<user>` within pytest's own newest
  three are kept while any process of the same user has `TMPDIR` unset:
  such a process may run pytest with `/tmp` as its temp root, and the
  contract's unused-root row cannot rule that out. On a desktop session
  that is nearly always. So it frees mostly sessions under the temp
  roots of ended `nix-shell.*` shells, and sessions older than pytest's
  own retention of three (which pytest's next run in the same temp root
  would also remove).
- After a reclaim, empty `pytest-of-<user>` directories, dangling
  `pytest-current` links and the ended `nix-shell.*` roots themselves
  remain: they are not candidates (`nix-shell-tmpdir` is excluded on
  purpose: a dormant shell can use its `TMPDIR` without visible
  evidence).
- How long it takes depends mostly on whether `/tmp` metadata is in
  memory. On the development laptop (14 cores, ext4 on LUKS, ~620
  processes, load 3.5 to 7, release candidate 0.1.0 from 29810c5), whose
  `/tmp` held 193,000 entries including 54,600 ended `nix-shell.*` roots,
  a dry run of `--select profile:pytest-tmp` judged 6,401 sessions
  (6,052 eligible, about 17 GiB) in about 7 minutes (402 s), and in about
  10 minutes under `nice -n 19 ionice -c 3`; it waits on the disk reading
  directories and inodes, not on the CPU. So give a first run on a large
  `/tmp` a `--timeout` above the default `10m`: one of the two niced dry
  runs hit it and exited 124 with an empty plan (since TASK-227 such a run
  reports every session it judged before the deadline). A dry run of
  `nix-shell-rc` on the same `/tmp` judged 128,892 rc directories in 34
  to 62 s. With `/tmp` in memory (a tmpfs, in a container sharing the
  host's PID namespace beside ~290 load processes, load about 4), a
  reclaim deleted 300 ended sessions (300 MiB) in 0.64 s, the first
  about 0.5 s after start, and 8,005 `nix-shell-rc` directories in 6.5 s.
  That container could not inspect the host's processes, so its scans
  were cheaper than a laptop's own run (Known limits, Visibility without
  root).

**Known limits.**

- **Seccomp in old container runtimes.** diskgc needs `pidfd_open` and
  `pidfd_send_signal`. Where a container's seccomp profile refuses them
  (measured: Podman 2.1.1's 2020 default profile), diskgc refuses to
  start, fail closed. Which current runtimes do is not measured yet
  (TASK-126); run `ensure` on the host or under a profile that allows
  both calls.
- **Visibility without root.** diskgc sees only the processes of the user
  it runs as, and not those the kernel hides from it (setuid, more
  capable or non-dumpable ones such as `sudo`, `systemd --user`,
  `ssh-agent`); their use of a candidate is not seen
  ([safety.md](safety.md#in-use), Visibility). Under `hidepid` it hides
  more, and diskgc does not reclaim there ([safety.md](safety.md#in-use),
  Visibility: a process list that cannot be complete).
- **In-use blind spots under review (TASK-303).** Kernel-held references
  without a `/proc` descriptor or mapping (io_uring registered files,
  overlay upper and work directories; a loop device's backing file is
  seen, [safety.md](safety.md#in-use), Loop devices),
  a mount point that exists only in another mount namespace, Cargo locks
  taken through an overlay, and a project directory shared over NFS or
  virtiofs with another kernel are not seen or not fully handled (a
  nested btrfs subvolume inside a candidate keeps it, TASK-286:
  [architecture.md](architecture.md#filesystem-matching)). None is reproduced yet; the 0.1 `/tmp` built-ins
  are less exposed (their contracts keep a session or rc directory on
  their own evidence). A nested subvolume keeps its candidate on btrfs
  by its device and on bcachefs by its `stx_subvol` (TASK-307), and in an
  overlay's upper layer, proved there; before Linux 6.10 the kernel
  reports no `stx_subvol`, so every directory on a bcachefs is refused
  (`unsupported_fs`), and on an overlay whose upper layer cannot be
  reached a nested btrfs subvolume there is not seen.
- **diskgc in its own PID namespace** (most containers, `PrivatePIDs=`)
  refuses to run `ensure`, its dry run and a reclaiming `watch` with an
  `fs` or `cargo-target` tier (exit 2,
  `safety`): the in-use check cannot see the processes outside it
  ([safety.md](safety.md#in-use), Visibility: a process list that cannot be
  complete). Run it on the host. (In 0.1.0 it ran, and kept every
  `nix-shell-rc` and most `pytest-tmp` candidates as `producer_unknown`,
  but judged Cargo targets by the processes of its own namespace only.)
- **Nix GC (`nix-garbage`).** Only CppNix 2.34.8, client and daemon,
  is admitted ([Backend version allowlist](#backend-version-allowlist));
  any other version, Lix or Determinate Nix, a single-user install, a
  store the user's Nix points elsewhere and `--allow-root` are skipped
  or refused before any GC ([safety.md](safety.md#nix-store), Failure
  modes). A dry run cannot size the garbage: its plan row is `/nix/store`
  with `estimated_bytes: null`, and it does not run the root check, so a
  reclaim can still skip a tier the dry run showed eligible. What diskgc
  binds is the system daemon and its root database; it cannot see a
  second root database over the same physical store (a container's own
  daemon sharing it, a `local?state=…` store), whose roots that GC
  collects as `nix-collect-garbage` would, nor processes outside the
  daemon's PID namespace using a store path without a root (Nix's own
  limit): keep one root database per store
  ([safety.md](safety.md#nix-store), What it does not prove). Another
  user's profiles can stop the tier for everyone: more than 1,024
  generation links in all is `unsupported_backend`, and a generation
  whose store path is gone is an error, until those generations are
  deleted (diskgc never deletes one). The GC frees at least the need by
  Nix's own count, often more (the rest of the last path), and Nix's
  count is not free space: the measured sample decides, and an
  undershoot moves on to the next tier. Nix's root discovery removes
  stale `gcroots/auto` registrations whose out-link is gone. A GC can
  take minutes under the host lock, and with `auto-optimise-store` the
  space appears only at its end.
- **`pytest-cache`.** A cache is deleted even while a pytest session
  runs in its project: pytest holds no file in it and writes it again
  when the session ends. The next `--lf`, `--ff` or `--sw` run has no
  record of earlier failures and runs every test.
- **`python-pycache`.** Python compiles a module again on its next
  import (time only), and a read-only tree stays uncached. A
  `__pycache__` whose `.pyc` files are tracked in Git is deleted too:
  `git status` then shows them deleted, and Git restores them. A `.pyc`
  whose source file is gone may be the only copy of that code: if you
  keep such files, put a `.diskgc-keep` file in the directory, or keep
  that tree out of the supplied roots.
- **`mypy-cache`.** The next run is a full check (time). diskgc sees a
  mypy run only while it holds a file in its cache: a run whose cache is
  deleted while it writes can stop with an error ("Error writing plugins
  snapshot" or a fine-grained dependency error) and has to be run again.
  mypy 2.1's default SQLite cache was seen held open while a run parses;
  that it stays held through the whole write phase, and mypy's JSON cache
  (`sqlite_cache = false`), were not tested.
- **`python-venv`.** An environment (a `.venv`) is kept while its
  project shows, within 30 days, a change or a read of a file in its
  environments, or while a process uses it at the moment diskgc checks.
  The project is the nearest enclosing repository, else the directory
  holding the `.venv`; a repository above the supplied root (a dotfiles
  `~/.git` with `--roots ~/src`) keeps every environment below it that
  has none of its own (`producer_unknown`). A read is the access time
  the filesystem records, not the read itself: under the default
  `relatime` mount option it can lag the last real read by up to about a
  day, so an environment last read just over 29 days ago can already
  look 30 days old, and on a filesystem mounted `noatime` reads are not
  seen at all. What diskgc cannot see, it can delete:
  - a project used without any change, recorded read or visible process
    at that moment: a tool run from a timer, a project you return to
    after a month;
  - activity only in the project's version-control metadata (a `git
    fetch`, commits, a branch switch that changes no file outside
    `.git`), or a change that only removed files;
  - a running program that names nothing in the project, or names it
    only through a path outside it (a symlink such as `~/bin/tool` to
    `.venv/bin/tool`, a script given on standard input, a module imported
    through a linked path), or only as its own container or `chroot` sees
    it (its command line and environment are compared as text, and a path
    in another mount namespace is another text), or that holds nothing in
    the project but a memory-mapped file in what diskgc's walk of the
    project leaves out (its version-control metadata, a mount below it);
    among these, a shell that ran `source .venv/bin/activate` and then
    left the project with `cd`: its environment can be deleted while the
    shell runs, its next `python` then runs another interpreter on `PATH`
    (or is not found), and late imports fail;
  - a project whose own code was read but none of its environment's
    files: reads elsewhere in the project count only when the
    environment links out of itself (an editable install, a symlink to a
    package outside it).

  The loss is a reinstall from the project's manifest, which can resolve
  other versions where no lockfile pins them. Packages you installed by
  hand without recording them in the manifest, and files you edited
  inside `.venv`, do not come back: keep such an environment with a
  `.diskgc-keep` file in it or a `protect` entry.
- **`node-compile-cache`.** Node compiles again on its next start (time
  only); a running Node's writes to a deleted cache fail silently. A
  cache at a `NODE_COMPILE_CACHE` path outside `/tmp` and the shell temp
  directories below it is not found.
- **The four package-manager caches** (`pip-cache`, `uv-cache`,
  `pnpm-store`, `ccache`). diskgc runs the program of the
  tool's name first on its `PATH` (it logs the path it resolved and checks
  no version), as your user, on the cache directory it measured, and
  cannot limit what that program deletes. A cache at a non-default place
  (the tool's own variable or option) is not found. Rebuilding pip's, uv's
  and pnpm's caches needs the package index or the network.
- **`pip-cache`.** `pip cache purge` empties pip's HTTP cache; the next
  install downloads every package again, as does an install that was
  running during the purge (pip holds no lock and no file there between
  downloads). The cache of wheels pip builds from source was not tested.
- **`uv-cache`.** `uv cache clean` removes the whole cache directory; the
  next install downloads again. While a uv command holds the cache, diskgc
  keeps it (`in_use`), and a clean that starts anyway waits for uv's lock.
  The cache is kept whole while it holds anything that looks like a
  version-control checkout other than the empty `sdists-vN/.git` file
  uv itself writes (a `.git` directory or a non-empty `.git` there, or a
  `.git` anywhere else, such as a package installed from a Git
  repository). An environment made with `link-mode = "symlink"` points
  into the cache and breaks until reinstalled; the default modes do not.
- **`pnpm-store`.** `pnpm store prune` removes only packages that no
  project pnpm knows of references; a project pnpm no longer knows (moved
  or deleted by hand) downloads its packages again on its next install.
  pnpm's lock is in `/tmp`, so a pnpm with a private `/tmp` (systemd
  `PrivateTmp`, a sandbox) is not held off by it.
- **`ccache`.** The next compiles miss and run the compiler again (time).
  Both `~/.cache/ccache` and `~/.ccache` are found. Remote or secondary
  storage was not tested.
- **`watch`.** It protects against running out only as fast as it
  samples and reclaims: a burst that writes more than the gap below
  `--min-free` within one `--interval` plus the slowest tier fills the
  disk anyway, so leave that gap above write rate × (interval + slowest
  tier). It reads its configuration once: an edit takes effect at a
  restart (`systemctl restart`, or a SIGHUP under `Restart=always`, 30 s
  later). One `watch` per filesystem and state directory: a second exits
  2. A cycle on a busy host waits holding the host lock (the background
  tally paused) until the host settles or the cycle's `--timeout`; an
  `ensure` meanwhile waits for the lock and can exit 75, and a cycle
  waiting behind another reclaim for longer than its `--timeout` ends
  `lock_timeout`; either ends in a `retry` back-off of 1, 2, 4, 8 minutes
  (at most 15), so the next cycle comes later, not sooner. After a cycle
  that took everything it could (`exhausted`), the next waits 15 minutes,
  or until available space falls by `max(1 GiB, 1 %)` more (tentative
  constants: [The reclaim cycle](#the-reclaim-cycle)). Percentages
  are resolved against the filesystem's size at start. The background
  tally's pressure brake needs the kernel's PSI and reacts to its
  10-second average; it does not limit the reclaim itself, which runs at
  the unit's priority. A failed write of a `--json` record ends `watch`
  with exit 3 (the unit restarts it). Not shown by a test yet: a
  root instance (`--allow-root`) that reclaims (its capability set is
  only checked to be applied), a `watch` cycle running the Nix GC, and a
  watcher's memory over days (TASK-134).
- **`go-mod-cache`, `bun-cache` and `nix-user-cache`** are deleted whole,
  with no age floor: a cache the tool used a minute ago is a candidate.
  diskgc keeps one while a process it can see holds a file in it or, for
  Go, names it in its starting environment (a `go build` in flight), but
  a tool between two reads can fail once (retry it). Each tool fetches
  again what it misses, so a build afterwards needs the network (Go with
  `GOPROXY=off` and offline Nix builds fail until it is back), and a
  module, package or source its upstream no longer serves is lost with
  the cache. A cache at a non-default place (`GOMODCACHE`,
  `BUN_INSTALL_CACHE_DIR` outside `.cache/.bun`, `XDG_CACHE_HOME`) is
  not found.
- **`go-mod-cache`.** Go makes its module directories read-only; diskgc
  adds write permission to each directory as it deletes it, only inside
  the cache it checked.
- **`bun-cache`.** Projects installed with `bun install
  --backend=symlink` point into the cache and break until reinstalled;
  the default (hardlinks or copies) keeps installed packages.
- **`nix-user-cache`.** Nix uses these caches between evaluations
  without holding a file open (seen: an evaluation cache changed minutes
  before a dry run), and nothing keeps a recently used cache: the next
  `nix` command fetches or evaluates again (time and network). It is
  not the Nix store; use `nix-garbage` for that.
- **The four build sources** (`cmake-build-anchored`,
  `meson-build-anchored`, `autotools-build-anchored`,
  `west-build-anchored`). A build directory unchanged for three days is
  deleted whole once its source is found above it, including what only
  it held: configure choices (`-D` values, Meson options, `./configure`
  arguments), which you must enter again, and files you put there by
  hand. Keep such a directory with a `.diskgc-keep` file in it or a
  `protect` entry. A build in progress is kept while a process it runs
  has its working directory or a file there; an IDE or debugger
  configured on the directory but not running finds it gone. diskgc
  checks that the source's build description still exists, not that
  every dependency can still be fetched. A build inside another build
  system's tree is not taken when Buildroot's or OpenWrt's stamps are in
  its parent or the three directories above it (within the root), when a
  Buildroot output, Yocto TMPDIR or Bazel output base lies above it, or
  when it is below npm's `node_modules` (a cmake-js addon). Other
  systems' package builds (Debian's `dpkg-buildpackage`, Spack,
  conda-build, a build below a Python `.venv`) are not recognised, nor a
  build tree reached through a bind mount of a directory inside it, and
  deleting such a build leaves that system believing the package is
  built until you clean it there.
- **Deadlines are cooperative.** `--timeout` is checked between
  operations; one filesystem operation blocked in the kernel (a dead
  network mount, a slow disk) overruns it, and the overrun is reported
  ([safety.md](safety.md#deadlines)).
- **Mounts diskgc refuses.** A root on or below an untriggered automount
  point (autofs) is refused without being looked up, so diskgc never
  mounts it; FUSE and network filesystems are refused as roots; a mount
  inside a candidate keeps it (`contains_mount`).
- **A very busy host ends candidates `in_use_unknown`.** A deletion needs
  a process scan less than a second old. On a host where one scan takes
  longer (hundreds of processes holding thousands of descriptors; the
  static musl binary scans slower than a glibc build), a candidate whose
  three attempts all see a stale scan is kept `in_use_unknown`, and after
  three in a row the tier backs off before each further check. A scan
  whose process list keeps growing (a fork storm), or that meets a
  process whose reads (or, for the project contracts, whose command line)
  do not hold still (exiting or changing, such as a concurrent build's
  linker), is waited out for the candidate it was made for, which is then
  checked again ([safety.md](safety.md#in-use), Freshness, Busy host); a
  dry run waits the same way. A storm that lasts until the deadline ends
  the run `deadline` (124), the candidate waited for kept `busy` and the
  rest of the tier not judged. A candidate a process of the user diskgc
  cannot inspect could be using is kept at once, as is one a process
  that has not held still for 30 s could be using (taken to be stuck);
  three of them in a row skip the rest of the tier, naming it. Nothing is
  deleted on a stale or unsettled scan, and the summary says to run again
  when the host is less busy, or once the process that cannot be
  inspected has exited. With `--timeout 0` (no deadline) a host that
  never settles keeps the run waiting, holding the lock, until a signal
  stops it.
- A program running from a Cargo target is protected by process
  heuristics only (Cargo holds no lock while it runs), and Linux has no
  unlink-by-inode: diskgc is not a defence against a hostile local user
  racing it (PRD, Safety boundaries).

**JSON contract.** The report's field names and enum values, the exit
codes and their precedence, and the one error envelope every command
uses under `--json` were frozen for 0.1 by the contract review (TASK-26)
and the 0.1 JSON pass (TASK-192): not applicable is `null`, never an empty
string; `--select` runs are named `select`; `find.roots` is always a
list, with `find.backend` beside it. Scripts match `status`, `cause`,
`reason` and the exit code; message text and human output may change.
`index` is still always `null` (0.1.0 had no index; this package has
one, and the report's `index` block is still to come, TASK-244).
`watch`'s JSON lines (one `ensure`-shaped record per cycle, plus its
`watch` block: [Watch JSON lines](#watch-json-lines)), its exit codes and
the module's options are new in 0.2, and their contract review is
pending (Mark). The
command-action and
Docker parts of the profile schema stay provisional until the v1
release review (task-49); the `nix-store` profile shape is final
([Profiles](#profiles)). `just docs-vs-binary BIN` runs what this
reference documents against a build and reports any drift.

**Not in the release catalog.** `survey` (exit 2), the Docker kinds, every command action
but the four caches' cleaners (`npm-cache`, `go-build-cache`, `yarn-cache`,
`platformio-cache` and `jest-cache` stay out, TASK-360; each one's reason
is in its row of [Liveness contracts](#liveness-contracts)), user profiles and overrides, `ipc: live-only` in `ensure`, and every other
built-in in the [library](#built-in-profile-library) (`node-modules`
among them, held by Mark, TASK-365; and `huggingface-hub-repos`,
`electron-download-cache`, `node-gyp-headers`, `cargo-registry-src`,
`buildroot-output-anchored`, `yocto-tmpdir-anchored` and
`bazel-output-base-anchored`, not admitted, TASK-370), including the other
`/tmp` entries (`nix-shell.*` TMPDIRs themselves, `tmp.*`, anything
else). The `docs/ci-pre-job.sh` example stays in the source tree; the
package does not install it.

Evidence for the candidate package, and what it does not cover:

| Candidate deployment question | Evidence and current limit |
|---|---|
| Package contents | The actual `release-candidate` derivation passes the static ELF and installed-file check (the binary is byte-identical to its compile; the file list is exactly its release row's, each copy byte for byte; the notes' header and `--help`'s `watch` lines follow the row); its tmpfiles rule and example config are exercised in disposable Nix and Debian containers, and its `watch --tally-only` starts, writes its snapshot, stops on SIGTERM with exit 0 and deletes nothing there. Its unit templates are checked by `systemd-analyze verify` as the development package's (byte-identical sources) and in the module's drop-in verification, and run under a live `systemd` (below). |
| Reclaiming `watch` | In disposable containers sharing the host's PID namespace, the packaged binary's `watch` reclaims `pytest-cache` candidates below `--min-free` up to `--target-free` and stops there, keeps one a process holds open (`in_use`), writes no record between the thresholds, backs off `hold` after an `exhausted` cycle, and stops on SIGHUP and SIGINT with exit 0; its cycle and an `ensure` take turns through the host lock (the `ensure` exits 75 `lock_timeout` while the cycle's cleaner holds it; a cycle waiting behind an `ensure` ends `interrupted` at SIGTERM); a user profile or a built-in 0.2 does not ship is refused at start. In a NixOS VM (`vm-release-watch`), the package's files installed as the generic-Linux steps say: the user template as the user, the system template with a `User=` drop-in, each watching a small ext4 that a fill drives below `--min-free`; each instance reclaims real pytest caches up to `--target-free`, the user instance keeping one a process holds; the background tally brakes under a fork storm; a cycle on a busy host (a fork storm beside 1,000 idle processes of the user) waits it out instead of giving its candidate up, then reclaims; a cycle behind an `ensure` whose cleaner holds the lock ends `lock_timeout`, one whose own cleaner outlasts its `--timeout` ends `deadline`, and each is followed one minute later by a cycle that reclaims; SIGHUP restarts the user instance after 30 s from its snapshot, an exit 2 is not restarted, and `systemctl stop` ends the system instance with exit 0. Not shown: an `--allow-root` instance that reclaims, `nix-garbage` or a cleaner in a `watch` cycle on a VM, a watcher over days. |
| NixOS module | With this package as `services.diskgc.package`, the module's build-time check refuses a user profile and a built-in 0.2 does not ship (the system build fails: `nixos-module-bad-config`); in a NixOS VM (`vm-nixos-module`), a reclaiming user instance reclaims a filled filesystem to its `--target-free` and keeps a held cache, and tally-only, root and refused instances behave as on the development package. |
| Cargo operator journey | Those containers run the packaged, gate-lifted binary against real Cargo targets: dry run, busy-lock preservation, eligible deletion judged by the filesystem oracle, and a successful rebuild. Default and explicit `checkouts: allow`, and explicit `checkouts: refuse`, are checked on vendored checkout content. |
| Catalog and excluded actions | The packaged binary exposes only `cargo-target`, `nix-shell-rc`, `pytest-tmp`, `nix-garbage`, `pytest-cache`, `python-pycache`, `mypy-cache`, `python-venv`, `node-compile-cache`, `pip-cache`, `uv-cache`, `pnpm-store`, `ccache`, `go-mod-cache`, `bun-cache`, `nix-user-cache`, `cmake-build-anchored`, `meson-build-anchored`, `autotools-build-anchored`, `west-build-anchored` and the seven idle filters. Excluded built-ins (the other five cleaners and `node-modules` among them) and operator-authored command/delete policies are rejected before execution; backend spy files and the deletion oracle remain untouched. |
| Package-manager caches | In disposable Nix containers sharing the host's PID namespace, with the pinned real tools (pip 26.1.2, uv 0.13.0, pnpm 12.9.0, ccache 4.14.1) and a registry the test serves, the packaged binary runs the development build's scenarios: the tool fills its cache, a dry run plans exactly it with the profile's command and no lookalike or cache at a non-default place, a reclaim under the deletion oracle runs the cleaner, and the tool fetches or builds again; a run held in flight is kept `in_use` (uv, pnpm) or survives the cleaner (pip, ccache; each also held after its cache lookup, so that it only writes afterwards); for `uv-cache`, dry runs with a `.git` directory, a non-empty `.git` file, an empty `.git` elsewhere and a checkout-shaped `.git` below `git-v0` keep the cache. Not run through the package: for uv and pnpm, the tool's own cleaner run directly against the install in flight and its lock wait (the release variants stop at diskgc's `in_use` verdict); the missing-tool and any-version-on-`PATH` scenario (development build only); a real `git` dependency in a uv cache (no network, no git in the image). |
| Nix GC journey | In the Nix GC image (a container's own CppNix 2.34.8 store and daemon), the packaged binary plans `nix-garbage` in a dry run that deletes nothing, then runs one GC bounded by the asked need: a direct root, an out-link, system and per-user generations, a running build's input and a store file a process holds open all survive, and the dead paths go, judged by the deletion oracle. Every other Nix GC scenario (refusals, signals, the pinned socket) runs the e2e harness's development build, which shares the kind's code. |
| Project-tree caches | In disposable Nix containers sharing the host's PID namespace, with the pinned real tools (pytest 9.1.1, CPython 3.14.7, mypy 2.1.0, Node.js 24.21.0, uv 0.13.0), the packaged binary runs the development build's scenarios for `pytest-cache`, `python-pycache`, `mypy-cache` and `node-compile-cache`: the tool makes the caches, a dry run plans exactly them and no lookalike, and a reclaim under the deletion oracle deletes exactly those it may while a pytest session, an importing Python and a Node keep running (each finishes and writes its cache again), and keeps a paused mypy run's cache `in_use`; the tools then rebuild. For `python-venv` it shows only the keep side of the 30-day floor: environments made by `python -m venv` and `uv venv` are kept `not_idle` and nothing is deleted. Their reclaim after the floor, the live-use and residual scenarios ran the development build, and the release build's catalog path only gated with a test clock offset; the gate-lifted binary has no clock offset, and no guest with a held-back clock has run it. |
| Runtime caches | In disposable Nix containers sharing the host's PID namespace, with the pinned real tools (Go 1.26.8, Bun 1.4.2, Nix 2.34.9) and a registry or module proxy the test serves, the packaged binary runs the development build's scenarios for `go-mod-cache`, `bun-cache` and `nix-user-cache`: the tool fills its cache, a dry run plans exactly it and no lookalike or cache at a non-default place, a reclaim under the deletion oracle deletes it (Go's read-only module directories through `make_writable`), and the tool fetches again; a `go build`, `bun install` and `fetchGit` held in flight keep their cache `in_use`, and each then completes. |
| Build outputs | Only the keep side of the three-day floor runs through the packaged binary: real CMake, Meson and Autoconf builds (and, in the full suite only, a West build) made now, which the gated development package plans reclaimable past the floor, are kept `not_idle` and nothing is deleted; a CMake package build beside OpenWrt's stamps is planned by neither. Their reclaims past the floor, the builds in flight, the moved and copied builds, the reconfigure and rebuild, and the nested-build guard on Buildroot's real package directories ran the development build, and the reclaims also the release build's catalog path gated with a test clock offset. |
| Host and filesystem deployment | The candidate has no approval for an operator host or its filesystem, mount, permissions and running-work mix. Container results establish behavior only in those disposable fixtures. |
| Race and temporary paths | Cargo race protocol tests use a separate development test driver, so they do not establish packaged-binary race behavior. The `nix-shell-rc` and `pytest-tmp` contracts' container tests run the development binary; their packaged binary's `/tmp` evidence is the release vectors (ModelHost) and a recorded walk in a disposable container (TASK-65), not a container suite, because `min_idle_for: 1h` meets the kernel's ctime and the gate-lifted binary has no test clock offset. |
| Operator policy and rollout | Operator-authored `fs`/delete and command actions are excluded; the final policy contract, installation environment and rollout evidence are unresolved. Do not deploy the candidate built from master. |

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
