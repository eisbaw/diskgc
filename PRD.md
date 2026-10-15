# diskgc: PRD

Status: draft, round 3. The human decides when this is done.

Diagram: `docs/diskgc-design.svg` (interface plus an *example* tier list; the
tier list itself is config).

## Tentative-vs-Committed

`firm`

The shape, interface and protocols below are decided. The Cargo tier still
ships only after its race tests pass (see What good looks like); if they
expose a flaw, the Cargo protocol is revised, not the rest. The backlog must
therefore schedule the Cargo race tests (and the Nix-daemon GC and Docker
`until` checks) first, before features that build on them.

## Essence / problem

Machines that build software unattended run out of disk, and when they do,
all work stops. On a development laptop, overnight agents fill the disk with
Cargo `target/` dirs, Nix store paths and Docker images, and in the morning
nothing is done. On persistent CI runners the same caches accumulate between
jobs.

Almost all of that space is *rebuildable cache*. The problem is not "find big
files"; it is: **keep enough space available that no job hits ENOSPC, by
deleting the cache that is cheapest to lose and provably not in use, and fail
loudly and early when that is not possible.**

diskgc is one binary. `ensure` brings one filesystem up to a free-space goal,
once. `watch` is a loop around the same core with two thresholds. What may be
deleted, and in which order, is a YAML file of typed tiers arranged in
groups. The code knows how to execute each tier type safely; the config
decides which tiers exist and their order. A `generic` tier type is the
escape hatch for anything else.

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
              [--dry-run] [--json] [--config FILE] [--allow-root] PATH
diskgc watch  --min-free SIZE --target-free SIZE [--interval DUR]
              [--config FILE] [--allow-root] PATH
diskgc check  [--config FILE]
```

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
  including waiting for another reclaim to finish. A CI hook never hangs.
- `--dry-run`: print, in order, every candidate diskgc would consider, whether
  it is eligible or the skip reason, and its tallied size; delete nothing.
  Does not take the lock. Always prints, even if the goal is already met.
  With a `random` group, the printed order is for the printed seed only.
  Sizes are estimates; Docker and Nix tiers can only say what they would run.
  Exits 0 (2 on usage errors, 130/143 when interrupted); JSON `status` is
  `dry_run`.
- `--json`: print exactly one JSON object on stdout at exit, for every exit
  code including usage errors.

### `watch`

- One process per filesystem. On NixOS, one instance of a systemd template
  unit per watched path.
- `--target-free` is required here (unlike `ensure`): a watcher without two
  thresholds would reclaim a sliver at every poll. The error message says so.
- Every `--interval` (default `10s`): one `statvfs`. At or above
  `--min-free`: nothing else happens, no scanning, no tallying.
- Below `--min-free`: run the same core as `ensure`, goal `--target-free`.
  No overall deadline; each action is bounded by its tier's `timeout`. The
  wait for the host lock must stay interruptible by SIGTERM.
- Back-off: if a reclaim ends still below `--min-free` (everything left is
  in use), do not start another until available space has dropped by a
  further `max(1 GiB, 1% of the filesystem)` or 15 minutes have passed. One
  log line per back-off, not per poll. These values are fixed in code.
- On SIGTERM/SIGINT: stop as described under Signals, exit 0.

### `check`

Load and validate the config, print the resolved config path and the groups
and tiers as diskgc understood them, exit 0 or 2. The NixOS module runs it at
build time so a bad config fails `nixos-rebuild`, not the service at 3am.

### Exit codes (`ensure`)

The first matching row wins:

| Code | Meaning |
|---|---|
| 2 | Usage, config or provisioning error. Nothing was deleted. |
| 130 / 143 | Interrupted by SIGINT / SIGTERM. |
| 75 | Deadline reached while waiting for another reclaim, and a fresh sample is still below `--min-free` (EX_TEMPFAIL). Nothing was deleted; retry later. If the other holder freed enough meanwhile, the result is 0. |
| 0 | Final sample of available space ≥ `--min-free`. |
| 124 | Deadline reached during reclaim, below `--min-free`. |
| 3 | A tier failed (command error, permission error, daemon gone), below `--min-free`. Something may have been deleted; the report says what. |
| 1 | Every eligible candidate was processed without errors, still below `--min-free`. The report lists what was skipped and why. |

The mapping is explicit in code and covered by tests. A Rust panic (101) is a
bug.

### Output

- stdout carries only the result. Human mode: silent when nothing needed doing
  (like `cp`); otherwise a short summary. `--json`: always one object.
- stderr carries progress logs: one line per action with tier, candidate,
  bytes freed (statvfs delta), duration. Output of child commands (Docker,
  Nix, generic) goes to stderr, prefixed with the tier name.
- `watch` logs to stderr only; journald collects it.

JSON result (field names are part of the contract):

```json
{
  "status": "met | exhausted | error | usage_error | lock_timeout | deadline | interrupted | dry_run",
  "mount": "/", "path": "/home/mark/src/app", "seed": null,
  "min_free_bytes": 0, "target_free_bytes": 0, "target_met": false,
  "avail_before_bytes": 0, "avail_after_bytes": 0,
  "actions":  [{"tier": "", "candidate": "", "freed_bytes": 0, "duration_ms": 0}],
  "skipped":  [{"tier": "", "candidate": "", "reason": "locked | in_use | not_idle | changed | empty | kept | other_fs | remote_backend | unsupported_fs | missing | unsafe_path"}],
  "errors":   [{"tier": "", "candidate": "", "message": ""}]
}
```

`seed` is `null` when no group uses `random`.

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

Limits, stated plainly: the guarantee is best-effort for generic commands
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

## Configuration (YAML)

Lookup: `--config`, then `$DISKGC_CONFIG`, then
`$XDG_CONFIG_HOME/diskgc/config.yaml` (default `~/.config/...`), then
`/etc/diskgc/config.yaml`. None found: exit 2, listing the paths tried. The
binary has **no built-in tiers**; an example config ships as
`docs/example.yaml`, and the NixOS module provides a default.

The config contains `version` and the tier groups. Thresholds, intervals and
timeouts are command-line arguments, so the same file serves `watch` on a
laptop and `ensure` on a runner.

```yaml
version: 1

groups:
  - name: stale                # nobody will miss these
    ranking: random            # ordered (default) | random
    tiers:
      - name: cargo-stale
        type: cargo
        roots: [/home/mark/src, /home/mark/topics]
        idle_for: 7d

      - name: docker-build-cache-stale
        type: docker-build-cache
        unused_for: 7d

      - name: nix-dead
        type: nix-gc

  - name: pressure             # costs rebuild or download time
    tiers:
      - name: docker-dangling
        type: docker-images
        select: dangling

      - name: cargo-recent
        type: cargo
        roots: [/home/mark/src, /home/mark/topics]
        idle_for: 1h

      - name: pip-cache
        type: generic
        path: /home/mark/.cache/pip
        command: [pip, cache, purge]

      # - name: docker-unreferenced   # removes tagged images too; opt in
      #   type: docker-images
      #   select: unreferenced-by-containers
```

Rules:

- **Groups run in list order. Always.** Escalation (stale before pressure) is
  group order; nothing reorders groups.
- **`ranking` decides tier order inside a group:**
  - `ordered` (default): list order.
  - `random`: shuffled on each reclaim. On a CI fleet, runners then lose
    different caches first, so the fleet as a whole stays warmer. The seed is
    in the report and can be fixed with `DISKGC_SEED` for reproducible tests.
- Candidates *within* a tier are always oldest first.
- The same tier type may appear several times with different parameters.
- All paths must be absolute. `~` is rejected: it would mean `/root` for one
  caller and `/home/mark` for another.
- Validation is complete at load; any error → exit 2, nothing deleted.
  Unknown keys are errors. `version` is mandatory. Tier names are unique
  across all groups.
- Disabling a tier = commenting it out. There is no `enabled` field.

### Protection marker

A file named `.diskgc-keep` in a candidate directory, or in any directory
between a tier's root and the candidate, excludes it. Reported as `kept`.

### Common tier fields

| Field | Meaning |
|---|---|
| `name` | Unique; used in logs and reports. |
| `type` | One of the types below. |
| `timeout` | Per-tier limit for one action (default `10m`); the child is killed after it. |

Age fields are **type-specific on purpose**, because each measures something
different and none of them proves "not in use":
`idle_for` (Cargo: last build), `unused_for` (BuildKit: last use as recorded
by Docker), `unchanged_for` (generic: newest mtime under the folder).

## Tier types

### `cargo`: Cargo `target/` dirs

- **Discovery:** walk `roots` (depth limit 6, fixed in code) for directories that
  contain a valid `CACHEDIR.TAG` and at least one *profile directory*: a
  directory holding a `.cargo-lock`, at `<target>/<profile>/` or
  `<target>/<triple>/<profile>/`. Do not descend into a found target. Never
  follow symlinks, never cross mount points (like `find -xdev`). Refuse
  targets on NFS (`unsupported_fs`): Cargo does not lock there.
- **Last build:** newest mtime of the profile directories and their
  `.fingerprint/`, `deps/`, `build/`, `incremental/` subdirectories. A few
  `stat` calls, no tree walk. Candidates are ordered by it, oldest first.
- **Eligible** only if the last build is older than `idle_for` and some
  profile directory holds more than its lock file (otherwise `empty`: an
  emptied target is not re-listed on every run).
- **In use (skip):**
  - any `.cargo-lock` cannot be `flock`ed without waiting (`locked`), or
  - a process the caller can see has its executable or working directory
    inside the target, from `/proc/*/exe` and `/proc/*/cwd` (`in_use`; e.g.
    a test binary still running after Cargo released the lock).
- **Reclaim protocol: delete inside locked profile directories only, never
  move or delete a lock file.**
  1. Take every `.cargo-lock` of the target without waiting. Any failure:
     release all, skip (`locked`).
  2. List the lock files again and re-read the last build time. A new lock
     file (a profile created since discovery) or a newer build: release,
     skip (`changed`). Repeat the `/proc` scan now that the locks are held;
     a hit: release, skip (`in_use`).
  3. In each locked profile directory: delete `.fingerprint/` first (so a
     half-deleted profile always forces a rebuild), then every other entry
     except `.cargo-lock`. Deletion walks with directory file descriptors
     and `unlinkat`, never follows symlinks, never crosses mounts.
  4. Release the locks.

  Nothing outside the locked profile directories is touched: not
  `CACHEDIR.TAG`, not the triple directories themselves, and not top-level
  entries such as `target/doc`, `target/tmp` (written by running tests
  without a lock) or `target/criterion` (benchmark baselines, which are data,
  not cache). Because lock files never move, a Cargo that was blocked on a
  lock and a Cargo that starts afterwards both lock the *same* inode and stay
  mutually exclusive. A Cargo started during step 3 prints its usual
  `Blocking waiting for file lock` and continues after step 4.

### `docker-build-cache`

`docker builder prune --force --filter until=<unused_for>`. Docker's build
cache `until` is based on last use (verify on the deployed Docker version).
One action prunes everything matching; there is no per-candidate stop.

### `docker-images`

`select: dangling` runs `docker image prune --force`.
`select: unreferenced-by-containers` runs `docker image prune --all --force`,
which also removes tagged images that no container references.
No age option: Docker's image `until` filter uses the image's *creation*
time, so an image built long ago but used hourly would look "old".

### `nix-gc`

`nix-store --gc --max-freed <remaining need>`. Nix decides liveness (GC
roots, temporary roots, runtime roots of running processes). `--max-freed`
means "stop after at least this much"; the real effect on available space is
measured, not assumed. Never removes generations or profiles.

### `generic`: the escape hatch

```yaml
- name: go-build-cache
  type: generic
  path: /home/mark/.cache/go-build
  unchanged_for: 2d          # optional; walks the tree
  command: [go, clean, -cache]
  timeout: 5m
```

- **This is trusted, arbitrary command execution.** diskgc provides no in-use
  protection and cannot limit what the command deletes; the config author
  owns that. Prefer the tool's own cleaner (`pip cache purge`,
  `go clean -cache`), which knows its own locking, over `rm`.
- `path` (required): absolute, not `/`, not a mount root. It decides which
  filesystem the tier belongs to, is tallied for `--dry-run`, and is the
  command's working directory. Missing at run time → skip (`missing`), not a
  config error.
- Path safety: opened with `openat2(RESOLVE_NO_SYMLINKS)` (mounts along the
  path, such as a separate `/home`, are fine), then `statx`: the device must
  match the one validated at load and `STATX_ATTR_MOUNT_ROOT` must be unset;
  otherwise skip (`unsafe_path`). Entered with `fchdir`, so a symlink
  swapped in after validation is never followed.
- A symlink anywhere in `path` (e.g. `/home -> /var/home`) is a config error
  at load; the message names the component and the real path to use.
- `command`: argv list, no shell. Use `[sh, -c, "..."]` explicitly if wanted.
- Environment: `DISKGC_PATH`, `DISKGC_NEED_BYTES`, `DISKGC_MOUNT`.
- Runs in its own process group; killed after `timeout`. Like every child
  (see Signals), it inherits the host lock.
- Non-zero exit is an error in the report (and exit 3 if the goal is not
  met), never just a log line.

## Reclamation core

```
reclaim(min, goal, filesystem, deadline):
  if avail(filesystem) >= min: return met            # no lock taken
  lock /run/diskgc/lock, waiting until deadline
    on timeout: return met if avail >= min else lock_timeout
  if avail(filesystem) >= min: return met            # someone else freed it
  for group in groups:                                # list order
    for tier in group.tiers(ranking, seed):           # ordered or shuffled
      skip tier if its backend is not on this filesystem
      for candidate in tier.candidates():             # oldest first
        if avail(filesystem) >= goal: return met
        if kept / empty / not idle / in use / changed: record skip; continue
        tier.reclaim(candidate, need = goal - avail)
        syncfs; record freed = avail delta
  return met if avail >= min else (error if any errors else exhausted)
```

`watch` calls it with `min = --min-free`, `goal = --target-free`; `ensure`
with its two flags.

- **Available space** = `f_bavail × f_frsize` from `statvfs`: what an
  unprivileged writer can use, never the root reserve. `syncfs` before each
  re-read, because some filesystems free blocks asynchronously.
- **Filesystem matching** happens before any tier runs: Cargo and generic
  candidates by `st_dev`; Docker by its data root (`DockerRootDir`, or the
  containerd root when the containerd image store is in use) and a local,
  not remote, Docker context; Nix by the device of `/nix/store`. A tier
  whose backend is elsewhere is skipped and reported (`other_fs`,
  `remote_backend`): deleting there would hurt a cache and free nothing here.
- **Overshoot:** for Cargo and generic candidates, at most one candidate past
  the goal. Docker and Nix actions are whole operations with no such bound.
- **Tallies** (directory sizes) are computed only for `--dry-run`. They count
  allocated blocks and each inode once (Cargo hard-links binaries from
  `deps/`), and are still estimates. Decisions and reports use the statvfs
  delta.

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
  user (believed allowed; to verify).
- With `--allow-root`, the config file and every directory above it must be
  owned by root and not writable by others (like sshd's StrictModes), since
  generic tiers execute commands from it.

## Hysteresis, everywhere it applies

| Where | Mechanism |
|---|---|
| `watch` | `--min-free` triggers, `--target-free` (required) is the goal. |
| `ensure` | Same pair; `--target-free` defaults to `--min-free` (no surprise deletions), CI hooks set it higher. |
| Candidate eligibility | `idle_for` / `unused_for` / `unchanged_for`: recently used caches are not eligible. |
| Nothing left to free | `watch` back-off until space drops further or 15 minutes pass. |
| Within a reclaim | Stop as soon as the goal is met; re-check after every candidate. |

Not included: per-tier cooldowns. They need state that survives between
`ensure` processes; the cost (a no-op Nix GC per CI job when nothing is dead)
is accepted until measured otherwise.

## Prior art and why it is not enough

| Tool | What it does | Gap |
|---|---|---|
| Nix `min-free` / `max-free` | GC with hysteresis during Nix builds | Only triggers on Nix builds; ignores Cargo and Docker writers. Complementary. |
| `nix.gc.automatic` | Timed Nix GC | Time-based, not pressure-based. |
| Docker `builder.gc` policy | Bounds BuildKit cache size | Docker only; size-based, not free-space-based. |
| `cargo-sweep`, `cargo-clean-all` | Age-based Cargo target cleanup | No lock awareness, no free-space goal, no other tiers. |
| `systemd-tmpfiles` age cleanup | Age-based deletion of paths | No pressure trigger, no in-use checks. |

diskgc's contribution is the coordinator: one free-space goal, an ordered
policy across ecosystems, and in-use checks per type.

## Deployment

- One Rust binary, Linux only.
- NixOS module: renders the YAML, ships the tmpfiles rule for the lock, runs
  `check` at build time, and starts one `diskgc watch` instance per
  configured path as the configured user.
- CI runners: binary, YAML, the tmpfiles drop-in, and
  `diskgc ensure --min-free 50G --target-free 80G "$CI_BUILDS_DIR"` in the
  pre-job hook.

## What good looks like (observable)

- With reclaimable candidates available, no job on a watched host fails with
  ENOSPC unless writes outpace the reserve (see risks).
- `ensure` exits 0 only if its final sample of available space is at least
  `--min-free`, and exits 0 without taking the lock when space is already
  there.
- **Cargo race tests pass**, each with real `cargo build` processes:
  1. a build holding the lock: its target is skipped;
  2. a build that starts during deletion: it waits, then succeeds;
  3. one build blocked on the lock plus another started right after the
     reclaim: they never run concurrently and both succeed;
  4. a target whose test binary is still running: skipped;
  5. a new profile (`--release`, `--target`) started between discovery and
     locking: skipped as `changed`, its build succeeds;
  6. diskgc SIGKILLed at random points during deletion, repeatedly: the next
     `cargo build` always succeeds.
- With available space steady between the two thresholds, `watch` does zero
  scans and zero deletions.
- Every action produces one log line and one JSON `actions` entry.
- An invalid config exits 2 and deletes nothing; `check` reports the same
  error; `--json` reports it as `usage_error`.
- The real run's deletions are a prefix of the `--dry-run` list on an
  unchanged system (`ordered` groups, or `random` with the same seed; Cargo
  and generic tiers).
- `ensure` never runs past `--timeout`.

## What bad looks like (must never happen)

- Anything deleted that no configured tier selected (generic commands
  excepted: their scope is the config author's).
- A `.cargo-lock` file deleted or moved, a target deleted while a Cargo
  process holds its lock, or two Cargo builds running in one target dir
  because of diskgc.
- Anything outside locked profile directories deleted by the Cargo tier.
- A Docker volume or container, a Nix GC root, profile or generation removed.
- Deletion following a symlink or crossing into another mount.
- A generic command run in any directory other than its validated `path`.
- `ensure` exiting 0 when the space is not there, or running past its
  deadline.
- A config error discovered only after something was deleted.

## Scope

In scope (v1): `ensure`, `watch`, `check`; tier types `cargo`,
`docker-build-cache`, `docker-images`, `nix-gc`, `generic`; groups with
`ordered` / `random` ranking; `--json`; NixOS module; plain binary + YAML for
CI.

## Non-goals

- Disk usage analysis (use `ncdu`, `nix-du`).
- Persisted state: no usage database, no cooldown timestamps.
- Copy-on-write filesystems with snapshots (Btrfs, ZFS), where deleting may
  free nothing. Warn on them.
- Event-driven watching (inotify/fanotify).
- Coordination across hosts.
- Removing Nix generations/profiles, Docker volumes/containers. (A generic
  tier *could* run `nix-collect-garbage --delete-older-than`; the example
  config must not.)
- Cargo top-level entries outside profile directories (`target/doc`,
  `target/tmp`, `target/criterion`). A generic tier can cover them if
  someone wants that.
- Running as root by default; Podman; rootless Docker; non-Linux.

## Key decisions

| Decision | Chosen | Why |
|---|---|---|
| Tier policy location | YAML; tier types in code | Policy differs per machine; safety needs type knowledge. |
| Ordering | Groups in list order; `ordered` or `random` tiers inside a group; candidates oldest first | Random spreads cache loss across a fleet without ever running pressure tiers before stale ones. |
| Thresholds | Command-line arguments | Same config for laptop and CI; smaller frozen schema. |
| Cargo deletion | Lock in place, delete inside locked profile dirs only, keep lock files | Blocked and new Cargo processes stay on one lock inode; unlocked areas are never touched. |
| Cargo in-use | Locks + `/proc` exe/cwd scan + `idle_for` | Catches binaries still running after Cargo released the lock. |
| Generic tier | Folder required + argv command | Folder gives it a filesystem, a tally and a working directory. |
| State | None persisted | Wiped runners have nothing to drift or corrupt. |
| Success measure | statvfs delta | Tallies are wrong with hard links and Nix dedup. |
| Concurrency | Fixed, provisioned `/run/diskgc/lock` | One lock inode per host, guaranteed. |
| Privileges | Non-root by default | Removes the "root runs commands from YAML" hazard. |
| `--target-free` default | `ensure`: equal to `--min-free`; `watch`: required | Asking for 50G must not delete 70G unless asked; a watcher needs two thresholds. |

## Open questions

None blocking. Items to settle during implementation are listed as risks
below, each with the check that resolves it.

## Weak assumptions & risks

- **Cargo lock coverage (load-bearing, unverified).** Assumes Cargo holds an
  exclusive `flock` on `<profile>/.cargo-lock` for the whole build, and that
  all writers inside a profile directory go through Cargo. Verified so far:
  the files exist, including per-triple directories. Not verified: lock
  duration, Cargo's newer separate `build-dir` layout, whether `cargo test` /
  `cargo run` hold the lock while running binaries. Resolved by the race
  tests above.
- **Half-deleted incremental state.** Deleting `.fingerprint/` first should
  force a full rebuild, but rustc loading a partly deleted `incremental/`
  session might still fail. Resolved by race test 6.
- **Last-build heuristic.** Directory mtimes change on real rebuilds but not
  on no-op builds, so an up-to-date, actively used target can look idle. The
  lock and `/proc` checks still protect running work.
- **`/proc` scan visibility.** Only the caller's own processes are visible
  (and fewer with `hidepid`). Fine when diskgc runs as the owner of the
  targets; weaker under `--allow-root` on multi-user hosts.
- **Burst writes.** Thresholds cannot protect against a write burst. The gap
  below `--min-free` must exceed write rate × (interval + slowest tier).
  Documented, not enforced.
- **Nix GC.** Can take minutes on a large store, holding the host lock
  meanwhile (hence the deadline). With `auto-optimise-store`, freed space can
  lag. Non-root GC via the daemon: to verify.
- **Docker.** Build cache `until` semantics (last use) and where images live
  (containerd image store vs `DockerRootDir`) to verify on the deployed
  version. Remote contexts or builders are skipped, not guessed.
- **Generic tier.** Arbitrary commands. Mitigated by non-root default, argv
  execution, `openat2` working directory, the inherited host lock and the
  StrictModes check under `--allow-root`. Still the most dangerous surface.
- **Lock stalling.** Any local user can hold `/run/diskgc/lock`, and an orphaned child keeps holding it until it exits.
- **`syncfs`** can stall on a busy disk; it counts against the deadline.

## Irreversibility map

Hard to walk back (deep review, tests before features):
- **The deletion path:** the Cargo lock-in-place protocol, fd-relative
  deletion with symlink and mount rules, generic tier `path` handling and
  command execution, filesystem matching.
- **CLI contract used by CI and harnesses:** `ensure` flags, exit codes and
  their precedence, JSON field names and enums, `.diskgc-keep`, the lock path
  `/run/diskgc/lock`.
- **YAML schema `version: 1`:** `groups`, `ranking`, tier type names and
  field names.

Tentative (change freely):
- Example groups and tiers; default timeouts; back-off constants; discovery
  depth; human-readable output; log line format.
