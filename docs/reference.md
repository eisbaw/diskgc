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

`watch` and `survey` are planned commands. This version returns exit 2
for either, naming the commands it has; their interfaces are described
below for implementation (`watch`'s is settled, `survey`'s is intended),
not as available CLI behavior.

A usage error (an unknown command or flag, a missing or malformed
argument) exits 2 and prints, on stderr, `diskgc: error: MESSAGE`, then
the usage lines of the command given (of every command when there is
none) and one line pointing to `diskgc --help`; it does not print the
whole help. A mistyped command, flag or `profiles` subcommand names the
closest one (`unknown flag --min-fre; did you mean --min-free?`). With
`--json`, the one JSON object carries the same message.

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
excluded and reported while the other tiers remain eligible. Protected and
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
  printed in every report.
- `--min-free` (required): if available space is at least this, do nothing
  and exit 0, without taking the lock. `ensure` still checks that
  `/run/diskgc/lock` is a safely reachable, readable regular file. If it
  is missing or unusable, stderr warns with the provisioning detail and
  the commands that provision it; exit 0 and the JSON `met` report are
  unchanged. A later run needing to reclaim exits 2 (`cause:
  provisioning`) if the lock is still missing or unusable. This readiness
  check does not take a `flock`.
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
  [architecture.md](architecture.md#the-reclaim-walk)), and, per tier,
  the tally that orders what that tier's own discovery finds under the
  host lock (every Cargo-target tier, union and `!profile:` tier, and
  what an indexed tier's queue did not take): candidates it leaves
  untallied are taken after the tallied ones, in the order discovery
  found them, each judged in full under its own locks. Until the
  snapshot exists (TASK-30), `--no-index` changes nothing: each run's
  index starts empty. A candidate a tally of this run refuses (e.g.
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

Planned; this build returns exit 2. The contract below is settled
(TASK-117) for implementation (TASK-32); nothing of it runs yet.

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
  a percentage is resolved against the filesystem's size at start.

**One process per filesystem and state directory.** The snapshot's
single-writer lock `index-<fs>.lock` ([architecture.md](architecture.md#the-tally),
Snapshot) lives in the state directory (`$XDG_STATE_HOME/diskgc`) and is
held for the process's lifetime, whichever the mode, so a second `watch`
(or `watch --tally-only`) with the same state directory on the same
filesystem exits 2 at start. The rule protects warmth and tally load,
not safety: a second watcher with another state directory runs, and the
host lock still serialises its reclaims. The start line names the state
directory. Two `PATH`s on one filesystem, or two btrfs
subvolumes of one filesystem, are one filesystem: they share available
space, so one watcher serves both. Watchers of different users or
different filesystems run side by side; their reclaims are serialised by
the host lock ([safety.md](safety.md#host-lock)) and their background
tallies' duty cycles add up.

**Configuration is read once.** Edits take effect on restart; there is no
reload (Watch signals: SIGHUP stops `watch`). Validate first with
`diskgc check`, then restart the service.

#### Watch start

In this order; the first failure exits with its code and deletes nothing:

1. Command line, configuration and the `--select` expression: exit 2 as
   for `ensure` (Configuration file, Selection grammar).
2. Running as root without `--allow-root`: exit 2 (`safety`). In a
   development build, the pre-release sandbox gate: exit 2 outside a test
   container ([safety.md](safety.md#pre-release-sandbox-gate)); not with
   `--tally-only`, which deletes nothing.
3. `PATH` (absolute, reached safely, [safety.md](safety.md#path-traversal)),
   kernel and `/proc` requirements ([architecture.md](architecture.md#deployment)):
   exit 2 when unsupported or invalid, 3 when an operation fails before
   `PATH` is open. The first space sample is not a start check: if it
   fails, it is the first cycle (The reclaim cycle). Raise
   `RLIMIT_NOFILE` as `ensure` does (Open-file limit under `ensure`).
4. The host lock file: missing or not a regular file exits 2 (`cause:
   provisioning`, with the commands that provision it), since a watcher
   that can never reclaim must fail where its operator looks, not log
   forever. Its `flock` is not taken. With `--tally-only` a missing file
   is one stderr warning: that mode only polls the lock (a missing file
   reads as not held).
5. Supplied roots that do not exist: exit 2, as for `ensure`.
6. The single-writer lock: waited for at most 10 s (an `ensure` holds it
   only while writing); still held exits 2 (`cause: usage`, naming the
   other process's filesystem key and the state directory). Failing to
   create the state directory or the lock file (`ENOSPC` on a full
   `/home` is the likely cause) is not fatal: the snapshot is a cache, so
   `watch` logs one line, runs without loading or writing a snapshot,
   and tries to create and take the lock again at every 5-minute
   snapshot tick (a lock then found held is one log line, never an
   exit).
7. Load a valid snapshot as the seed of the live index
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
  its first deletion (roots, executables, backend binding;
  [safety.md](safety.md#validation)), since the machine changes under a
  long-running process; it also re-resolves `PATH` and requires the same
  device as at start (another one is a cycle error, `host`, and nothing
  is deleted). The background tally pauses while the host lock is held
  (by this cycle or anyone else's reclaim) and resumes after it. Samples
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

The cycle's outcome is the status `ensure` would report for the same run
(Exit codes (`ensure`), JSON `status`): `met`, `exhausted`, `error`,
`lock_timeout`, `deadline`, `interrupted`, or `usage_error` (only `cause:
provisioning`: the host lock file vanished after start; start-time
checks catch the rest). It never ends the process.

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
   not applicable when the final sample failed;
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
  level (`null` when the final sample failed), and `attempt` counts
  consecutive `retry` cycles (`0` for `hold`).

What is not a record, and why:

- **No record when nothing happens.** A sample at or above `--min-free`
  is not an event; at the default interval it would be 8,640 lines a day
  that say nothing. The stream holds what diskgc did and why, and its
  silence means space was there.
- **No heartbeat.** Whether `watch` is alive is the supervisor's
  knowledge (process state, `systemctl is-active`); a heartbeat in the
  data stream would mix liveness into the record of actions.
- **No start or stop record.** A start failure prints the error object
  every command prints (it has a non-null `exit_code`); a successful start
  and a stop are stderr log lines. A cycle a signal stopped is a record
  with status `interrupted`.

Every line is therefore a cycle report, except a single start-failure
object as the only line. Consumers ignore unknown fields; a future kind of
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
  kept, and the kept counted by reason (one line in all, not one per
  candidate: a watcher that is `exhausted` every 15 minutes must not
  flood the journal);
- back-off: one line when it starts, one when space recovering ends it;
- the background tally: one line when the pressure brake engages and one
  when it releases, each with the number of batches since the previous
  such line; a skipped or failed snapshot write: one line
  ([architecture.md](architecture.md#the-tally), Snapshot);
- stop: one line naming the signal, one when the process exits.

A failed stderr write is ignored (the log is best effort; the process
keeps protecting the disk).

#### Watch signals

- **SIGTERM, SIGINT and SIGHUP** stop `watch` the same way. In a cycle:
  as `ensure` stops (Signals): no further candidate starts, a running
  child gets SIGTERM and SIGKILL after 10 s, the host lock is released,
  and the cycle's record (status `interrupted`) is written. While waiting
  for the host lock or in back-off or between samples: at once. The
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
  once. SIGKILL at any point is safe: deletions are crash-safe (the kill
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
| 2 | Refused at start: usage, configuration, privilege (`safety`), the sandbox gate, a missing host lock file (`provisioning`), a supplied root missing, or another `watch` holding the filesystem's single-writer lock. Nothing was deleted. |
| 3 | A host operation failed at start before `PATH` was open (`/proc`, the kernel checks), or writing stdout failed at any time. |

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
  pauses, so it never competes with the reclaim it serves.
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

- **Reclaims** take turns through the host lock. A cycle that waits past
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
  profile worth writing is visible. A name family is the name with a
  trailing random-looking suffix after `.`, `-` or `_` (five or more
  letters and digits, at least one a letter) replaced by `*`, then runs of
  digits replaced by `#`, e.g. `nix-shell.8raHjU` → `nix-shell.*`,
  `nix-shell-1144586-3257819220` → `nix-shell-#-#`. The grouping is a hint
  for humans, not part of the JSON contract.

Without `--select`, all profiles (built-in and user) are applied; each
`PATH` also serves as the root for `roots: required` profiles, so
`diskgc survey ~/src` finds Cargo targets there. `survey` starts from a
valid snapshot when one exists and tallies what it lacks
([architecture.md](architecture.md#the-tally)); it never writes the
snapshot.

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
    "where": {"idle_for": null, "has": null, "has_dir": null, "has_not": null, "app_not_running": null, "lacks_entry": null, "keep_newest": null, "newest_group": null, "protect_links": [], "protect_link_dirs": [], "protect_parent_link": null, "protect_cursor_extensions": null, "source_anchor": null, "liveness_contract": null},
    "action": {"type": "delete | command | cargo_lock_protocol | docker_builder_prune | docker_image_prune | docker_image_prune_all", "make_writable": false, "command": null, "cwd": null},
    "checkouts": "refuse | allow",
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
  filter; `examples` is also `null` for `cargo-target`, which takes none;
  `verified_with` is set on built-in sources only.
- `liveness_class` is a built-in source's contract class, spelled as in
  YAML (`sound`, `producer-equivalent`, `verified-heuristic`, `tolerant`,
  `delegated`, `none`; [Liveness contracts](#liveness-contracts)), `null`
  for a filter and for every user profile. `releases` lists the release
  catalogs that include a built-in (`["0.1"]` for the proposed 0.1
  catalog), and is empty otherwise.
- `find.roots` is always a list: the fixed roots, or empty when
  `find.roots_required` is `true` (`roots: required`: a tier or `--roots`
  supplies them) or when `find.backend` names where the kind's storage
  comes from (`"daemon"`, YAML's `roots: daemon`, for the Docker kinds;
  `null` for every other kind). `find.paths`,
  `find.exclude` and `find.type` are `null` for `cargo-target`.
- `where` always has its fields, `null` when unused (link lists are empty);
  `idle_for` is a duration (`3d`).
- `min_idle_for` is a source's mandatory minimum age, or `null`; it is
  `null` for filters. A tier's positive idle filters may require longer.
- `action` has every field: `make_writable` (a boolean) for `delete`;
  `command` (the argv) and `cwd` (`candidate`, `parent`, or `null` when
  it is decided per candidate: `type: any` without `cwd`) for `command`;
  neither for `cargo_lock_protocol`. Later kinds add their own `type`.
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
| 2 | Usage, config or provisioning error. Nothing was deleted. |
| 130 / 143 | Interrupted by SIGINT / SIGTERM. |
| 75 | Deadline reached while waiting for another reclaim, and a fresh sample is still below `--min-free` (EX_TEMPFAIL). Nothing was deleted; retry later. If the other holder freed enough meanwhile, the result is 0. |
| 0 | Final sample of available space ≥ `--min-free`. |
| 124 | Deadline reached during reclaim, below `--min-free`; or during a dry run (its plan has every candidate judged before it). |
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
run as root, a missing host lock file when reclaim is needed, with
`cause: provisioning`, and, in
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
  was too busy to check whether it is in use"; one kept because a process
  of the user cannot be inspected names that process. One line counts
  them and says to run again when the host is less busy. A candidate kept
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
  out).
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
  tier and profile name. Each tier logs where it looked (`N target(s)
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
    "skipped":  [{"tier": "", "profile": "", "candidate": "", "count": 1, "examples": [], "reason": "locked | in_use | in_use_unknown | not_idle | too_idle | changed | empty | keep_marker | other_fs | remote_backend | unsupported_fs | unsupported_layout | unsupported_backend | backend_unavailable | missing | unsafe_path | contains_ipc | contains_mount | contains_special | contains_checkout | protected | excluded | not_owner | filtered | future_time | stale | too_many_entries | unreadable | newest_version | linked_current | installed_extension | producer_unknown", "detail": null, "process_blockers": [{"pid": 42, "command": null, "evidence": "fd | cwd | root | exe | map | inode_map | environment | unreadable | target_cwd | target_exe | project_cargo | detached_cargo | cargo_manifest_dir | owner_pid | command_line | temp_root | unresolved_path"}], "estimated_bytes": null}],
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
shape above; `ensure` prints its whole report, every other command
(`check`, `profiles`, and the commands this version does not have) only
those three fields, e.g. `{"status": "usage_error", "exit_code": 2,
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
kernel. `provisioning` means the host lacks what diskgc's installation
must provide: the host lock `/run/diskgc/lock` is missing or is not the
regular file its tmpfiles line makes, when a reclaim needs it (exit 2,
status `usage_error`). The fix is the operator's, not the caller's: the
message prints the commands that provision it (the package's tmpfiles.d
drop-in, written out, installed in `/etc/tmpfiles.d` and applied with
`systemd-tmpfiles --create`; or `mkdir`/`touch`/`chmod` until the next
boot), so a harness alerts ops rather than retrying or rewriting its call
(TASK-151). An unsafe configured root is a `safety` refusal even when found
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
  until the snapshot (TASK-30) defines its `source`, although `ensure`
  builds and uses an index (TASK-31; its synchronous tally logs what it
  did).
- `tally_cut_short` is true only after a reclaim's tally (the
  synchronous one before the host lock, or a tier's under it) actually
  stops at `--tally-budget` with at least one candidate left untallied
  (or, for the synchronous one, a source not yet listed). It is false
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
  argv), not an enum: it is for reading, not for matching.
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
- `skipped[].process_blockers` is `null` when the refusal cannot be
  attributed to an observed process, including flock contention, an
  unsettled process scan and grouped rows. Otherwise it is a nonempty array
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
  only when a complete tally succeeded before a later refusal. Early skips,
  untallied paths and grouped rows use `null`. It is a point-in-time size,
  not a promise of space that could be reclaimed.
- A Cargo target whose profile directories hold only lock files is omitted
  from a live `ensure` result's `skipped` list and human skipped count; a
  stderr line names it (`TIER: PATH: its profile directories hold nothing
  but lock files; nothing to reclaim, not listed in the result`), so the
  tier's `N target(s) below` count adds up. It remains discoverable and may
  appear as an ineligible `empty` plan row in `--dry-run`.
- A root of a tier on another filesystem than `PATH`'s is not searched. It
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
  On btrfs, mounted subvolumes with the same mountinfo source are included.
  `command` is `null` when the process's command could not be read.
  It comes from the run's last process scan (one is made at the
  end if the run made none). It is `null` when that scan failed (diskgc
  out of file descriptors, the process list unreadable), stopped (the
  deadline, a signal) or did not settle (a process list that kept
  growing), and when the run made no scan: a run that reclaimed nothing
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

Docker and Nix tiers act through a backend that diskgc binds first
([safety.md](safety.md#docker-kinds), [safety.md](safety.md#nix-store)).
One rule holds for every backend kind, at preflight and at the recheck
right before acting:

- **Skip** (the tier is excluded, reported in `skipped` with candidate
  `null`, the other tiers run, and the exit code follows available space:
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
(`cargo-target`, `nix-shell-rc`, `pytest-tmp` and the idle filters; see
[0.1 release notes](#01-release-notes)).
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
`version: 1`; diskgc 0.1 refuses it, naming in one run each part it does
not ship: user `profiles:` (including overrides), `profile_policies` for
built-ins outside 0.1 (`npm-cache`), `protect` of a user profile, and tiers
that select a user profile or a built-in outside 0.1 (`node-compile-cache`,
`docker-build-cache`, `docker-dangling`). Every key keeps its meaning in a
later release.

```yaml
# Development build only: diskgc 0.1 refuses this configuration.
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
  verified_with: CPython 3.13.15 and observed CPython 3.12 .pyc layout in a project tree, 2026-10-20
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
`/nix/store`, or one skip with candidate `null`, and `estimated_bytes:
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
| `liveness_class` | built-in sources, required | The source's liveness contract class under the admission rule ([safety.md](safety.md#producer-liveness)): `sound`, `producer-equivalent`, `verified-heuristic`, `tolerant`, `delegated` or `none`. Its evidence, residuals and verdict are its row in [Liveness contracts](#liveness-contracts). Recorded data: diskgc does not act on it at run time. |
| `releases` | built-ins, optional | The release catalogs that include the built-in, listed explicitly: `["0.1"]` (quoted strings) puts it in the proposed 0.1 catalog. Membership, not "since": a later catalog lists its own members. No duplicates; refused with `liveness_class: none` unless empty. Absent, null or empty: the development library only. |
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
| `liveness_contract` | a code-backed producer liveness contract (`nix-shell`, `pytest`; designed: `node-project`, `python-project`): the candidate is eligible only when the contract permits it, judged from the same process scan as the in-use check, in the plan and again immediately before deletion ([Liveness contract predicate](#liveness-contract-predicate)) | `fs` source only |
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

Status (TASK-112 step 5): `nix-shell` and `pytest` are implemented and
judged; the `nix-shell-rc` and `pytest-tmp` built-ins use them.
`node-project` and `python-project` are designed below and refused at
load ("not supported by this version of diskgc", naming the supported
ones) until they land (TASK-79), so nothing half-built can be selected.
They are reserved, not part of the accepted set: since no valid profile
can name them today, TASK-79 may still rename or merge them (one
`project` contract, say) without breaking a configuration. What each contract shows,
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
| `node-project` (reserved: refused at load, TASK-79) | a `node_modules` directory whose parent holds `package.json` and that lies in no other dependency directory | project dependency directories |
| `python-project` (reserved: refused at load, TASK-79) | a virtual environment (holding `pyvenv.cfg`) whose parent holds a Python project manifest and that lies in no other dependency directory | project dependency directories |

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
- a contract this version does not judge yet (`node-project`,
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
  reached a bind-mounted subdirectory. Its `process_blockers` is `null`
  when no particular process caused it (a precondition, a marker, a
  listing); otherwise it holds one ordinary entry (a pid, as every entry
  has) per process that caused it, sorted by pid, each with evidence
  `unreadable` (existing) or `unresolved_path` (new: a path the contract needs for that process
  did not resolve: a relative value of a variable or option pytest
  resolves, an absolute one whose lookup in the process's root failed, or
  a failed lookup of one of Python's fallback roots).
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
it is not approved for deployment. The candidate keeps `cargo-target`, the
two `/tmp` built-ins with a liveness contract (`nix-shell-rc`, `pytest-tmp`;
TASK-65, Mark 2026-10-24) and the seven idle filters, and rejects every
operator-authored profile, including
overrides. It also refuses configured tiers and `--select` expressions that
refer to excluded built-ins, even during a dry run. `profiles list`,
`profiles show`, `check` and help use the same narrowed library.
`profile_policies` for the admitted sources works only to narrow: `checkouts:
refuse` on any of them, `allow` only where the built-in already allows
(`cargo-target`, `pytest-tmp`); `allow` on `nix-shell-rc` is refused, as are
policies for excluded sources, including `npm-cache` and `uv-cache`, at load
time. The other `/tmp` proposals stay out: `nix-shell-tmpdir`, `mktemp-dir`
and `tmp-entry` have no owner evidence ([Liveness
contracts](#liveness-contracts)). Operator-authored `fs` and delete policies
remain a release decision under TASK-67.

| Category | Profile | Finds | Action |
|---|---|---|---|
| test | `pytest-tmp` | pytest session directories `pytest-of-*/pytest-*`, one candidate per session (Mark, 2026-10-23), under `/tmp`'s temp-prefix v1 preset; `liveness_contract: pytest`, `min_idle_for: 1h` ([Liveness contracts](#liveness-contracts)); `checkouts: allow`: tests create Git repositories as fixtures in their temp directories, which are the run's own output | `delete` |
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
| withdrawn | `claude-code-tmp`, `chromium-tmp`, `playwright-tmp`, `zephyr-tmp`, `renode-tmp`, `xdg-cache-entry-v2` | withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24): class *none* ([Liveness contracts](#liveness-contracts)); removed from the library, each matching set kept as a user probe profile in the vectors (`<name>-probe`); re-admission only under a new name (TASK-135, TASK-136) | none |
| agents | `cursor-agent-versions`, `codex-releases`, `claude-code-versions` | versions under a supplied home root; keep the two newest per sibling set and the installed symlink targets. `claude-code-versions` matches native installer version files only, not `~/bin/claude-*` | `delete` |
| editors | `cursor-extensions` | version directories under `.cursor/extensions` of a supplied home root; keep the two newest of each extension id and every installed directory in `extensions.json` | `delete` |
| browsers | `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Only `Cache` and `Code Cache` directories in `Default` and `Profile *` under the corresponding browser's `.cache` directory below a supplied home root; each source requires its own `.config/.../SingletonLock` to be absent | `delete` |
| caches | `pip-cache`, `go-build-cache`, `npm-cache`, `ccache` | the tool's cache dir (roots required) | `command` with the tool's own cleaner |
| caches | `go-mod-cache` | `go/pkg/mod` below a supplied home root | `delete` with `make_writable` |
| python / js | `uv-cache`, `pnpm-store`, `yarn-cache` | the tool's cache dir (roots required) | `command` (`uv cache clean`, `pnpm store prune`, `yarn cache clean`) |
| python / js | `bun-cache` | `.bun/install/cache` or `.cache/.bun/install/cache` below a supplied home root | `delete` |
| python / js | `python-pycache` | `__pycache__` directories at relative depths 1 through 5 below each supplied project root | `delete` |
| python / js | `mypy-cache` | `.mypy_cache` directories at relative depths 1 through 5 below each supplied project root, with a regular `CACHEDIR.TAG` file | `delete` |
| python / js | `node-modules`, `python-venv` | planned: `node_modules` directories, and `.venv` directories holding `pyvenv.cfg`, at relative depths 1 through 5 below a supplied project or workspace root and inside no other dependency directory, judged by their project's activity; `checkouts: refuse` | no safe action yet; contract designed, see [Liveness contracts](#liveness-contracts) |
| python / js | `jest-cache` | Jest's default `jest_*` cache under `/tmp` or `/var/tmp`, including the temp-prefix v1 preset below | `jest --config '{}' --cacheDirectory '{path}' --clearCache` from the checked parent; Jest removes the measured directory itself |
| python / js | `node-compile-cache` | `node-compile-cache` under `/tmp`'s temp-prefix v1 preset | `delete` |
| c / c++ | `cmake-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `CMakeCache.txt`, a direct `CMakeFiles/` directory, no `CMakeLists.txt` entry, and a surviving recorded CMake source ancestor; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| c / c++ | `meson-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with a regular `build.ninja`, a direct `meson-private/` directory containing regular `coredata.dat`, generated `meson-info/meson-info.json` recording this exact build directory and a surviving Meson source ancestor, and no `meson.build` entry; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| c / c++ | `autotools-build-anchored` | directories of any name at relative depths 1 through 5 below a supplied project or workspace root, with regular `config.status`, `config.log`, and `Makefile`, status script recording this exact build directory and a surviving `configure.ac` source ancestor, and no `configure.ac` entry in the output; minimum 3-day idle age and `checkouts: refuse` | `delete` |
| jvm | `gradle-caches`, `maven-repo` | planned: `~/.gradle/caches`, `~/.m2/repository` (roots required) | no verified whole-directory cleaner: Gradle maintains cache entries by retention after a build, while Maven's dependency purge applies only to a project's dependencies |
| embedded | `west-build-anchored` | Zephyr `build/` dirs through depth five below supplied project roots; regular `CMakeCache.txt`, direct `zephyr/` directory, no `CMakeLists.txt` entry, surviving recorded CMake source ancestor under the root, and mandatory 3-day idle age | `delete` |
| embedded | `platformio-cache` | `.platformio/.cache` below a supplied home root; PlatformIO's default `core_dir/.cache` | `pio system prune --cache --force` with `PLATFORMIO_CACHE_DIR=.cache`, starting in the checked `.platformio` parent; PIO re-resolves the path as described in [safety.md](safety.md#command-actions) |
| generic | `tmp-entry` | planned: top-level regular files and directories in `/tmp`, `/var/tmp`; a dormant process can use an entry without an observable descriptor or initial environment path | no safe action yet |
| generic | `mktemp-dir` | planned: `tmp.*` directories under `/tmp`'s temp-prefix v1 preset; a live shell can use one as `TMPDIR` after exec, invisible to the process scan | no safe action yet |
| filters | `idle-1h`, `idle-1d`, `idle-2d`, `idle-3d`, `idle-7d`, `idle-10d`, `idle-30d` | (no `find`) | none |

`has-cachedir-tag` in the selection error examples is a parser-test fixture,
not a shipped built-in filter.

The command cache profiles use the tools' own cleaners on the exact measured
directory. They retain the default `checkouts: refuse` safety rule. Some real
uv and npm caches contain `.git` metadata; those candidates report
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
general process check for arbitrary Electron applications. The surveyed
Cursor and teams-for-linux installations have cache directories but no
`SingletonLock` in their application roots, so their profiles are deferred
until a reliable app-liveness signal is available. Firefox `cache2` also
needs a different guard and is outside this Chromium-family set.

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
profile was renamed.

### Liveness contracts

Every built-in source's liveness contract class and its verdict under the
admission rule of [safety.md](safety.md#producer-liveness), plus the
decided profiles that are not built-ins. A built-in source without a
sound, producer-equivalent, verified-heuristic, tolerant or delegated
contract is not admissible. The rule applies to every built-in, shipped
ones included (Mark, 2026-10-23); TASK-122 classified those that predate
it. This table is the single home for these decisions (TASK-112). Each
built-in's class is also data, its `liveness_class` in builtin.yaml, and
`releases: ["0.1"]` there puts a built-in in the proposed 0.1 catalog; a
test keeps the table, the library and the catalog in sync (every built-in
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
its task notes.

| Profile | Producer (verified) | Contract (evidence) | Class | Residuals | Verdict |
|---|---|---|---|---|---|
| `nix-shell-rc` | legacy `nix-shell` and `nix develop` (Nix 2.34.8 on the host, 2.34.9 pinned by `nix_shell_rc_lifecycle_is_pinned`): the directory's pid is the shell's own for both, interactive and `-c`; `nix develop`'s directory is referenced by nothing after start; legacy `nix-shell`'s is the `TMPDIR` of the shell and its children and its rc trap removes it except after `SIGKILL` (safety.md) | `liveness_contract: nix-shell`, `min_idle_for: 1h` | sound, under Owner PID's preconditions | A consumer that outlives the shell and keeps only a path string (Owner PID's scope). `min_idle_for: 1h` narrows it and is not part of the contract's soundness: a detached child that wrote in the directory within the hour keeps it `not_idle`. | Shipped (TASK-112 step 4) after the lifecycle verification (both commands, interactive and `-c`, normal exit, `SIGHUP` and `SIGKILL`, a detached child outliving the shell, a shell in a private PID namespace sharing `/tmp`), with its vectors and container tests. Matching set as in the library row. Small in bytes, large in count. In the proposed 0.1 catalog (TASK-65). |
| `pytest-tmp` | pytest 9.1.1 (sources read; lifecycle pinned by `pytest_lifecycle_is_pinned`; the contract by `pytest_live_policy_none_session_in_shell_root_is_kept` and `pytest_live_basetemp_session_is_kept`), pytest-xdist 3.8.0 with execnet 2.1.2 (`pytest_orphan_xdist_worker_keeps_session`: a worker outlives its killed controller by about 15 s, then execnet ends it) | `liveness_contract: pytest`, `min_idle_for: 1h`, `checkouts: allow`; one candidate per session | verified heuristic: lock rows sound; the rank row producer-equivalent or stricter (it never permits a session pytest's own next cleanup, with the default retention, would keep; while a pytest of the same user runs it needs a session older than pytest's dead-lock age, Mark 2026-10-23); the unused-root row heuristic, and it does remove sessions above the rank that pytest keeps | Those of safety.md (pytest sessions), each with its vector. `min_idle_for: 1h` covers pytest's startup window between the session's `mkdir` and its `.lock` (microseconds), with a wide margin for a stalled start; it is not part of the contract's soundness. | Shipped (TASK-112 step 5), admitted by Mark, 2026-10-23. Matching set as in the library row. Every verdict of the decision table names its row in the detail; the checks before the table (safety.md) name none. Container tests: `pytest_lifecycle_is_pinned`, `pytest_live_policy_none_session_in_shell_root_is_kept`, `pytest_live_basetemp_session_is_kept`, `pytest_orphan_xdist_worker_keeps_session`. In the proposed 0.1 catalog (TASK-65, Mark 2026-10-24). |
| `brave-browser-cache`, `chrome-browser-cache`, `chromium-browser-cache`, `edge-browser-cache` | Chromium-family browsers, default user-data directory | `app_not_running` | sound (presence marker) | A browser that starts between the last lock check and the unlink finds its `Cache` or `Code Cache` missing, which Chromium treats as an empty cache. | Shipped (TASK-74). |
| `cargo-target` | Cargo (1.90.0, 1.99.1, nightly-2026-10-16) | the kind's lock protocol, `checkouts: allow` | delegated (producer lock) | A program built in the target and running from it is protected by process heuristics only: Cargo holds no lock while it runs (PRD, Limits). | Shipped; in the proposed 0.1 catalog. |
| `docker-build-cache`, `docker-dangling`, `docker-unreferenced` | Docker Engine 29.8.2 and Buildx 0.37.2 in digest-pinned dind | the bound engine's own prune (`docker builder prune --all`, `docker image prune`, `docker image prune --all`); the daemon keeps what a build or container references | delegated | `docker-unreferenced` also removes tagged images that no container references; a later `docker run` pulls them again, or they are rebuilt when they were built locally and never pushed. | Shipped; not in 0.1 (Docker actions are outside its scope). |
| `pip-cache`, `uv-cache`, `go-build-cache`, `npm-cache`, `pnpm-store`, `yarn-cache`, `ccache`, `platformio-cache`, `jest-cache` | pip 26.1.2, uv 0.13.0, Go 1.26.8, npm 11.19.0, pnpm 12.9.0, Yarn 1.22.22, ccache 4.14.1, PlatformIO 6.1.19, Jest 30.2.0, each cleaner run in a Nix container on the exact measured directory | the tool's own cleaner (Built-in profile library); in use, `checkouts: refuse` | delegated | No worse than the user running that cleaner: what it does to a run in flight is the tool's (Jest's `--clearCache` and `ccache --clear` remove the directory's contents outright; `pnpm store prune` removes only packages no project references). A uv environment installed with the non-default `link-mode = "symlink"` points into the cache and breaks until reinstalled. Rebuilding needs the network. | Shipped; not in 0.1 (command actions are outside its scope). |
| `pytest-cache` | pytest 9.0.3 (cacheprovider source read, TASK-122) | consumer tolerance: `Cache.get` returns the caller's default for a missing or unreadable key, `Cache.set` creates the directory and only warns when it cannot write | tolerant | The next `--lf`, `--ff` or `--sw` run has no record of earlier failures and runs every test. | Shipped. |
| `python-pycache` | CPython 3.13.15 (importlib) | consumer tolerance: a missing or stale `.pyc` is recompiled from its source, a failed bytecode write is ignored, and a `.pyc` in `__pycache__` is never imported without its source (PEP 3147), so the directory never holds the only copy of code | tolerant | Recompilation on the next import; a read-only tree stays uncached (time only). | Shipped. |
| `mypy-cache` | mypy 1.11.2 (cache layout); write paths read in mypy 1.9.0's source (TASK-122), not re-read for 1.11.2 | in use; consumer tolerance between runs: a module whose cache entry is missing is checked again; `CACHEDIR.TAG` required | verified heuristic | The next run is a full check (time). A run whose cache directory disappears while it writes stops with mypy's blocking "Error writing plugins snapshot" or fine-grained dependency error: one failed, retryable run (the in-use scan sees the run only while it holds a file there). | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: tolerant between runs, not for a run in flight. Vector: `mypy-cache-run-in-flight-residual`. Container test: pending (TASK-137). |
| `node-compile-cache` | Node.js 24.21.0 (probe, TASK-122: a cold cache, a warm cache with hits, the directory renamed away mid-run and a path below a regular file all exit 0; the mid-run write fails with "no such file or directory" and is ignored) | consumer tolerance: entries are keyed by Node version and content hash, and every read or write failure is a miss | tolerant | Recompilation on the next start (time only). | Shipped. |
| `nix-user-cache` | Nix 2.34.8 (probe, TASK-122, private `XDG_CACHE_HOME`: `tarball-cache-v2` moved away while `fetcher-cache-v4.sqlite` stayed; `fetchTarball` over HTTP downloaded again and returned the same store path, exit 0). `gitv3` and `eval-cache-v*` not probed | in use (a reading process maps the Git cache's pack files); consumer tolerance between runs: Nix refetches or re-evaluates what its cache lacks | verified heuristic | A process between two reads of a Git cache can fail once (retryable). Refetching needs the network, so an offline build fails until the cache is repopulated. Not rebuildable: a source whose upstream is gone and whose store path was collected is lost with its cache copy. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the lost-upstream residual is a dated exception (safety.md, Producer liveness). Vector: `nix-user-cache-mapped-pack-in-use-and-run-between-reads-residual`. Container test: pending (TASK-137). |
| `go-mod-cache` | Go 1.26.7 (`go env GOMODCACHE`; Go's own `go clean -modcache` removes the same whole directory) | in use; consumer tolerance between runs: a missing module is downloaded again and checked against `go.sum` | verified heuristic | A build reading module sources while they are deleted can fail once (retryable). Needs the network: `GOPROXY=off` or offline builds fail until re-download. Not rebuildable: a module version neither the proxy nor its origin still serves. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the unserved-module residual is a dated exception (safety.md, Producer liveness). Vector: `go-mod-cache-build-in-flight-residual`. Container test: pending (TASK-137). |
| `bun-cache` | Bun 1.3.3 (`bun pm cache`; Bun's own `bun pm cache rm` removes the same whole directory); install backends from Bun's documentation, not probed | in use; consumer tolerance between runs: an install hardlinks or copies from the cache (Linux default), so installed `node_modules` keep their data; a missing package is downloaded again | verified heuristic | An install in flight can fail once (retryable). Needs the network. Projects installed with the non-default `--backend=symlink` point into the cache and break until reinstalled. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122), with the residuals named here: the backend claim is unprobed. Vector: `bun-cache-symlink-backend-and-install-in-flight-residual`. Container test: pending (TASK-137). |
| `cmake-build-anchored`, `meson-build-anchored`, `autotools-build-anchored`, `west-build-anchored` | CMake 3.30.5, Meson 1.7.0, GNU Autoconf 2.73, Zephyr West 1.5.0 build directories; the producer (`cmake --build`, `ninja`, `make`, `west build`) runs with the build directory as its working directory for the whole build | in use (a running build's cwd, a running built program's executable or mappings), `min_idle_for: 3d`, the recorded source survives (source anchor), `checkouts: refuse` | verified heuristic | (1) Configure choices kept only in the build directory (`CMakeCache.txt` `-D` values, Meson options, `./configure` arguments in `config.status`) must be entered again. (2) Files a user put in a build directory by hand are deleted with it: a user's own work, outside the rule's loss bound (vector `cmake-build-anchored-hand-placed-file-residual`). (3) A dormant IDE or debugger configured on the directory finds it gone and rebuilds. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122). Residuals (1) and (2) fall under the dated exception in safety.md (Source-anchored CMake output); remedy `.diskgc-keep` or `protect`. Vectors: `cmake-build-anchored-hand-placed-file-residual`, `cmake-build-anchored-configure-choices-and-dormant-ide-residual`. Container test: pending (TASK-137). |
| `cursor-agent-versions`, `codex-releases`, `claude-code-versions`, `cursor-extensions` | Claude Code native installer (host 2026-10-24: three versions kept beside the link target, so it prunes to three itself), Cursor agent CLI (31 versions on the host: no pruning), Codex CLI (11 releases), Cursor 3.1.15 extensions (superseded versions stay until Cursor's own startup cleanup, not verified) | `keep_newest: 2`, the installed link targets, `current` and `extensions.json`; in use: a running version's executable or mapped files lie inside its candidate (vectors: exe, map, cwd, fd) | verified heuristic | A launcher or script that runs an older version by its full path fails to start until reinstalled; a process of an older version that reads its code files and closes them (the extension host of an open Cursor window still running a superseded extension beyond the two newest) fails on its next lazy load until reloaded. Versions are downloaded again. | Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122). Vectors: `claude-code-versions-running-old-version-exe-in-use` (the evidence), `claude-code-versions-launcher-by-full-path-residual`, `cursor-extensions-superseded-extension-lazy-load-residual`. Container test: pending (TASK-137). |
| `claude-code-tmp` | Claude Code 2.1.280 (host 2026-10-24: each of 18 live interactive sessions held its own `<session>/tasks` directory open; on 2026-10-18, TASK-68, idle sessions held nothing in their directory: undocumented, version-dependent behaviour) | in use only; no floor (the tier's idle filter) | none | Not a built-in. Was: A session that ended and is resumed later (`--resume`) finds its scratchpad and task output gone; under a version that holds nothing, a live session's directory too. The loss is the agent's scratch files and background-task output, which can hold a user's work (scripts, patches, reports). Counterexample vector (user probe `claude-code-tmp-probe`) `claude-code-tmp-live-session-without-held-file-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). Re-admission only under a new name, with the held directory pinned per version, a floor, and Mark's decision on resumed sessions. |
| `chromium-tmp` | every Chromium-based browser and Electron application: the `org.chromium.Chromium` temporary-name prefix belongs to Chromium's base library, shared by all builds not branded Chrome (host 2026-10-24: 312 directories, of them 245 component-updater URL fetcher and 5 unpacker scratch of Brave and Chromium, 62 unnamed; 8 files; live Brave and Chromium) | in use only; no floor; no presence marker covers every producer | none | Not a built-in. Was: Not all can be named: a live application's temporary files and directories under the shared prefix, whose uses were not enumerated. For the updater scratch alone, one failed component update, retried by the updater. Counterexample vector (user probe `chromium-tmp-probe`) `chromium-tmp-live-updater-scratch-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). A narrower name for the updater scratch could be a verified heuristic. |
| `playwright-tmp` | Playwright 1.56.1 (`verified_with`); 1.59.1 running on the host 2026-10-24: a 5-day-old driver with headless Chromium held its `playwright_chromiumdev_profile-*` (descriptors and mappings), while its `playwright-artifacts-qHgpwa`, created in the same second, was held by no process | in use only; no floor | none | Not a built-in. Was: A live session's later download, trace or video fails until its browser is relaunched; the glob `playwright*` also matches whatever else a tool names so. The loss is not a cache miss, and a live producer leaves no evidence. Counterexample vector (user probe `playwright-tmp-probe`) `playwright-tmp-live-artifacts-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `zephyr-tmp` | no tool: on the host, a project script (`mktemp -d /tmp/zephyr-h5e5-run.XXXXXXXX`; 2,578 such and 213 `zephyr-h5-run.*`) removes its run directory on success and keeps it on failure, printing its Renode and UART log paths for the user; hand-made directories (`zephyr-h5-clock-patch`, `zephyr-usart-patch.*`, without `.git`) and Git checkouts match too | in use only; no floor | none | Not a built-in. Was: Failure diagnostics the user was pointed to; hand-made work without a checkout; a live run's directory between two steps. Counterexample vector (user probe `zephyr-tmp-probe`) `zephyr-tmp-kept-failure-diagnostics-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `renode-tmp` | Renode 1.17.0 TemporaryFilesManager (source read, renode-infrastructure `f3d5d7b7d`): one `renode-<pid>` per process; `GetTemporaryFile` creates each file and closes it; at start and exit Renode removes `renode-<pid>` directories of pids that are not alive, never `*-crash` (its crash reports) and nothing under `--keep-temporary-files` | in use only; no floor | none | Not a built-in. Was: A live Renode's directory holds no descriptor between uses, so a running emulation loses the files it created; crash reports (`renode-<pid>-crash`, 29 on the host) and directories kept on request are deleted. Counterexample vector (user probe `renode-tmp-probe`) `renode-tmp-live-emulation-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). Re-admission: a new name over `renode-<pid>` only, without `-crash`, under an Owner PID contract (Renode's own rule). |
| `xdg-cache-entry-v2` | any application under `$XDG_CACHE_HOME` (XDG Base Directory Specification 0.8 calls it non-essential data; the profile's own exclusion list records applications that keep sessions, credentials, models and backups there) | in use, `min_idle_for: 30d`, `checkouts: refuse`, exact-name exclusions | none | Not a built-in. Was: Not all can be named: any unknown application's state, or a cache that is the only copy; a dormant application that reads its cache only when it starts. Counterexample vector (user probe `xdg-cache-entry-v2-probe`) `xdg-cache-entry-v2-unknown-app-state-eligible`. | Withdrawn (TASK-122, mark-emulator for Mark, 2026-10-24). |
| `node-modules`, `python-venv` | npm: to be verified by the planned container test (an install from an offline tarball, a read-only run); pnpm, Yarn, Bun, venv, virtualenv and uv: planned, not verified; consumers are `node`, `python` and editors | planned: `liveness_contract: node-project` or `python-project`, `min_idle_for: 30d`, `checkouts: refuse` | verified heuristic: project activity | Those of safety.md (Project dependency directories); the loss in a residual is a reinstall, except hand-edited dependency files, a dated exception to the admission rule (Mark, 2026-10-23; remedy `.diskgc-keep` or `protect`). | Admitted by Mark, 2026-10-23. Nested dependency directories are never candidates, so a removal cannot silently switch Node to a hoisted copy. The matching set is fixed before the names ship. Not shipped: the loader refuses both contracts until they land with their vectors and container tests (TASK-79, which took over TASK-112 step 6). |
| `nix-shell-tmpdir` | `nix develop`'s rc runs `mktemp -d -t nix-shell.XXXXXX` and exports it after `exec`; no exit cleanup (left after a normal exit, Nix 2.34.8) | none: the name records no owner; a dormant shell shows no use | none | Not a built-in. | Excluded. Counterexample: the dormant-temp-users container test. The class is fixed at the producer, upstream (TASK-123). |
| `mktemp-dir` | any program | none | none | Not a built-in. | Excluded. Counterexample: a shell that exports `TMPDIR=$(mktemp -d)` after `exec`. |
| `tmp-entry` | any program | none | none | Not a built-in. | Excluded for good: there is no one producer, and every counterexample above applies. |
| `containers-image-tmp` | containers/image and Buildah spools in `/var/tmp`; the real producers' lifecycle is unverified | `min_idle_for: 3d` only | none | Not a built-in. | Withdrawn (Mark, 2026-10-23). Counterexample: `vm_containers_image_dormant_spool_exposes_liveness_gap`. Re-admitted only under a new name after TASK-125 verifies the real lifecycle in a container test. |
| `gradle-caches` | Gradle; its daemons lock cache entries only on demand | none for diskgc: Gradle's own retention-based cleanup owns the directory, but there is no cleaner diskgc could invoke on a measured directory, so it is not *delegated* | none | Not a built-in. | Excluded; tune Gradle's cache retention instead. |
| `maven-repo` | Maven, `mvnd`, IDEs | none: a JVM holds a jar open only while it is loaded; the repository has no lock by default | none | Not a built-in. | Excluded. |
| `nix-garbage` | the Nix store's own garbage collector | Nix's GC roots and lock (kind `nix-store`), not a `where` predicate | delegated | Not a built-in yet. | Governed by the `nix-store` protocol (TASK-36, TASK-118). |

### 0.1 release notes

**Status: draft.** 0.1 is not released until TASK-67 closes; until then
this describes the `release-01-candidate` package built from the current
source, and its gate lift is tested only in disposable containers. The
default development package stays sandbox gated.

**What 0.1 does.** One command frees space, two read-only ones explain
it:

- `diskgc ensure --min-free SIZE PATH` brings the filesystem of `PATH` up
  to a free-space goal, once, and exits with a code a CI pre-job hook or
  an agent harness branches on ([Exit codes](#exit-codes-ensure)); with
  `--dry-run` it only plans, with `--json` it prints one report object.
  It runs a configuration's tier groups, or one `--select` expression
  with `--roots`, under the host lock, with a cooperative `--timeout`.
- `diskgc check` validates a configuration without touching the machine;
  `diskgc profiles list|show|test` shows the built-in catalog.
- The catalog is three sources and the seven idle filters (`idle-1h` to
  `idle-30d`): `cargo-target` (Cargo `target/` directories under supplied
  roots, reclaimed by Cargo's lock protocol), `nix-shell-rc` (rc
  directories of ended `nix-shell` and `nix develop` shells directly in
  `/tmp`; Owner PID) and `pytest-tmp` (pytest sessions the pytest
  contract permits). Both `/tmp` sources keep anything changed in the
  last hour (`min_idle_for: 1h`); their limits are their rows in
  [Liveness contracts](#liveness-contracts). A configuration may use
  `profile_policies` only to narrow a built-in's checkout guard; user
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
   Without the lock, a run that needs to reclaim exits 2 with `cause:
   provisioning` and prints these commands; a run whose goal is already
   met only warns.
3. `share/doc/diskgc/examples/example.yaml`: copy it to
   `~/.config/diskgc/config.yaml` or `/etc/diskgc/config.yaml`, replace
   its Cargo root with directories you own, and run `diskgc check`. It is
   not installed as an active default.
4. `share/doc/diskgc/`: a README (install and first run), these release
   notes (`RELEASE-NOTES.md`, cut from `reference.md` at build time),
   `reference.md` and [safety.md](safety.md), so that the notes' links
   resolve (links into the design documents do not).

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
  that is nearly always. So 0.1 frees mostly sessions under the temp
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
  ([safety.md](safety.md#in-use), Visibility). `hidepid` hides more.
- **In-use blind spots under review (TASK-169).** Kernel-held references
  without a `/proc` descriptor or mapping (io_uring registered files, a
  loop device backed by a candidate, overlay upper and work directories),
  a mount point that exists only in another mount namespace, Cargo locks
  taken through an overlay, a project directory shared over NFS or
  virtiofs with another kernel, and a nested btrfs subvolume are not seen
  or not fully handled. None is reproduced yet; the 0.1 `/tmp` built-ins
  are less exposed (their contracts keep a session or rc directory on
  their own evidence).
- **diskgc in its own PID namespace** (most containers) keeps every
  `nix-shell-rc` candidate as `producer_unknown`, and every `pytest-tmp`
  session except those without a `.lock`, below pytest's retention rank
  and older than its dead-lock age: Owner PID needs the complete process
  list of the initial PID namespace.
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
  three attempts all see a stale scan is kept `in_use_unknown`, as is one
  whose scan's process list keeps growing (a fork storm) or that a process
  of the user diskgc cannot inspect could be using; after
  three such candidates in a row the rest of that tier is skipped for the
  run. Nothing is deleted on a stale scan; the exit code follows the
  space, and the summary says to run again when the host is less busy.
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
`index` is always `null` in 0.1 (there is no index). The command-action,
Docker and Nix parts of the profile schema stay provisional until the v1
release review (task-49). `just docs-vs-binary BIN` runs what this
reference documents against a build and reports any drift.

**Not in 0.1.** `watch` and `survey` (both exit 2), the index (so
`--no-index` changes nothing and `--tally-budget` bounds only the
synchronous tally), the Docker kinds, the Nix store kind, command actions,
user profiles and overrides, `ipc: live-only` in `ensure`, and every other
built-in in the [library](#built-in-profile-library), including the other
`/tmp` entries (`nix-shell.*` TMPDIRs themselves, `tmp.*`, anything
else). The `docs/ci-pre-job.sh` example stays in the source tree; the
package does not install it.

Evidence for the candidate package, and what it does not cover:

| Candidate deployment question | Evidence and current limit |
|---|---|
| Package contents | The actual `release-01-candidate` derivation passes the static ELF and installed-file check (the binary is byte-identical to its compile, the README, reference and release notes are installed); its tmpfiles rule and example config are exercised in disposable Nix and Debian containers. |
| Cargo operator journey | Those containers run the packaged, gate-lifted binary against real Cargo targets: dry run, busy-lock preservation, eligible deletion judged by the filesystem oracle, and a successful rebuild. Default and explicit `checkouts: allow`, and explicit `checkouts: refuse`, are checked on vendored checkout content. |
| Catalog and excluded actions | The packaged binary exposes only `cargo-target`, `nix-shell-rc`, `pytest-tmp` and the seven idle filters. Excluded built-ins and operator-authored command/delete policies are rejected before execution; backend spy files and the deletion oracle remain untouched. |
| Host and filesystem deployment | The candidate has no approval for an operator host or its filesystem, mount, permissions and running-work mix. Container results establish behavior only in those disposable fixtures. |
| Race and temporary paths | Cargo race protocol tests use a separate development test driver, so they do not establish packaged-binary race behavior. The `/tmp` contracts' container tests run the development binary; the packaged binary's `/tmp` evidence is the release vectors (ModelHost) and a recorded walk in a disposable container (TASK-65), not a container suite, because `min_idle_for: 1h` meets the kernel's ctime and the gate-lifted binary has no test clock offset. |
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
