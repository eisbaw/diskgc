# diskgc: PRD

Status: draft, round 12. The human decides when this is done.

This file is the product contract: purpose, users, behaviour, safety
boundaries, acceptance criteria, scope and decisions. Each detailed rule
lives in exactly one place:

| Document | Holds |
|---|---|
| [docs/reference.md](docs/reference.md) | CLI, exit codes, JSON output, configuration, profile schema, selection grammar, built-in profile library |
| [docs/safety.md](docs/safety.md) | every deletion protocol: filesystem checks, Cargo, Docker, Nix, command actions, path traversal, validation, host lock, privileges, deadlines |
| [docs/architecture.md](docs/architecture.md) | index, tally, reclaim walk, measuring space, implementation, deployment |
| [docs/testing.md](docs/testing.md) | test layers, required scenarios, race tests, interlock, `just` recipes |
| [docs/diskgc-design.svg](docs/diskgc-design.svg) | one-page overview diagram |

## Tentative-vs-Committed

`firm`

The shape, interface and protocols are decided. The Cargo kind ships only
after its race tests pass ([testing.md](docs/testing.md)); if they expose a
flaw, the Cargo protocol is revised, not the rest. The backlog schedules the
container harness and the Cargo race tests first.

## Essence / problem

Machines that build software unattended run out of disk, and when they do,
all work stops. On a development laptop, overnight agents fill the disk with
Cargo `target/` dirs, Nix store paths, Docker images and `/tmp` leftovers,
and in the morning nothing is done. On persistent CI runners the same
caches accumulate between jobs.

Almost all of that space is *rebuildable cache*. The problem is not "find
big files"; it is: **keep enough space available that no job runs out of
bytes, by deleting the cache that is cheapest to lose and not in use as far
as diskgc can tell, and fail loudly and early when that is not possible.**
v1 is about filesystem bytes; inode exhaustion and quotas are out of scope.

## Users / actors

- **Laptop owner** running several projects and overnight coding agents on
  a Linux machine (any distribution; NixOS is not assumed). Runs `watch` as
  a service under their own user; agents call `ensure` before each task.
- **CI runner fleet operator.** Persistent runners, one job at a time. Calls
  `ensure` from the pre-job hook; optionally `watch --tally-only` between
  jobs.
- **Agent harness** (a script). Calls `ensure --json` and branches on the
  exit code and the JSON result: retry later, skip the task, alert.

## How it works

- **Two entry points, one core.** `diskgc ensure --min-free 50G PATH` brings
  one filesystem up to a free-space goal, once, for CI hooks and agents.
  `diskgc watch --min-free 30G --target-free 60G PATH` is a service that
  does the same whenever space drops below the low threshold (hysteresis).
  Read-only companions: `survey` (what profiles match, with candidate
  lists), `profiles list|show|test`, `check`.
- **Profiles say what and how.** A profile recognises one kind of cruft
  (pytest temp dirs under `/tmp`, Cargo targets, Docker build cache, ...)
  by positive patterns, and says how to remove it (`delete`, the tool's own
  cleaner, or a code-backed protocol). A library of built-in profiles ships
  with diskgc; users add and override their own, typically drafted by an AI
  from `survey` output and checked by the tool.
- **Tiers say which and when.** The config's tiers select profiles with
  qualified expressions such as
  `(profile:pytest-tmp || profile:nix-shell-rc) && filter:idle-3d`, grouped
  so that stale cruft goes before anything that costs rebuild time.
- **Gather, then act.** A throttled background tally keeps an index of what
  the profiles find, so freeing space never waits for a survey. The reclaim
  walk takes candidates in policy order, discovers each tier afresh before
  moving on, and checks every candidate on the live filesystem before
  acting.

## Safety boundaries

What diskgc guarantees for its own actions (`delete` and the code-backed
kinds), and where those guarantees end. The protocols are in
[safety.md](docs/safety.md).

- Only files a source profile found, that a tier selected, and that no
  `protect` entry or `!profile` overlaps, are ever deleted.
- Only files whose every entry belongs to the user diskgc runs as (or to
  an explicit `owners:` list under `--allow-root`).
- Not if a visible process has any of the candidate's files open, mapped,
  as its working directory, or (heuristically) named in its initial
  environment; not if the candidate directory is `flock`ed by someone
  else (a locked *file* inside is only caught as an open file of a visible
  process), nor if it contains a mount, a `.diskgc-keep` file, a Cargo
  target or (except for the Cargo kind) a version-control checkout, nor
  (by default) a socket or FIFO; a configurable policy may
  block only live ones instead (a FIFO some visible process holds open, a
  socket bound in diskgc's network namespace), so stale test fixtures do
  not block forever.
- Cargo targets only inside profile directories whose locks diskgc holds;
  no Cargo lock file is ever deleted or moved.
- No symlink followed, no mount crossed.
- The index never decides safety; every check runs at deletion time.
- Development builds refuse to delete anything outside the test
  containers ([safety.md](docs/safety.md#pre-release-sandbox-gate)), so
  work-in-progress builds cannot delete real data. Only release packages
  (0.1 onwards, see Scope) are built without that gate.

Limits, stated plainly: diskgc only sees the caller's own processes without
root, and not even those of them the kernel will not show it (setuid, more
capable or non-dumpable ones such as `sudo`, `systemd --user`,
`ssh-agent`), whose use of a candidate is therefore not seen; Cargo holds
no lock while a program runs, so running binaries are protected by process
heuristics only; Linux has no unlink-by-inode, so an
entry replaced between diskgc's identity check and its unlink can be
deleted (diskgc is not a defence against a hostile local user racing it);
a `command` action can delete whatever its author wrote, so command actions
are trusted code and never run as root; deadlines are cooperative, and a
filesystem operation blocked in the kernel can overrun them.

## What good looks like (observable)

- With reclaimable candidates available, no job on a watched host fails for
  lack of filesystem bytes unless writes outpace the reserve.
- `ensure` exits 0 only if its final sample of available space is at least
  `--min-free`, and exits 0 without taking the lock when space is already
  there.
- The Cargo race tests pass against every supported Cargo version
  ([testing.md](docs/testing.md)).
- Every model scenario and property in [testing.md](docs/testing.md) passes,
  including: the index is never an authority; selection semantics match the
  reference implementation; renamed or replaced entries are never deleted
  in their place; mixed-owner trees are skipped; built-in profiles' golden
  match sets are unchanged unless renamed.
- With available space steady between the thresholds, `watch` makes zero
  deletions and its tally keeps its duty cycle.
- With a warm index on a 100,000-entry `/tmp`, the first deletion starts
  within 10 seconds of crossing `--min-free` for candidates of up to 10,000
  files (the time includes the candidate's eligibility walk; it is measured
  and reported for larger ones).
- New stale cruft created after the last tally is deleted before any
  pressure tier runs.
- Every action produces one log line and one JSON `actions` entry; every
  skip has a reason.
- An invalid config exits 2 and deletes nothing; `check` reports the same
  error without touching the machine.
- `ensure` returns within `--timeout` + 10 s unless a single filesystem
  operation blocks in the kernel, in which case the overrun is reported.
- No test touches the developer's live system; `just e2e` stays under its
  time budget.

## What bad looks like (must never happen)

These apply to diskgc's own actions. A `command` action can do anything its
author writes; diskgc only guarantees which candidates it runs for, where,
when, and never as root.

- Anything deleted that no source profile found and no tier selected, or
  that overlaps a `protect` entry or a negated profile.
- Any entry deleted that the eligibility walk did not check, or that
  belongs to an unauthorised user.
- Any Cargo lock file deleted or moved; a target deleted while a Cargo
  process holds any of its locks; two Cargo builds running in one target
  dir because of diskgc; anything outside locked profile directories
  deleted by the Cargo kind.
- A filesystem candidate deleted while a visible process has one of its
  files open, mapped or as working directory; a socket or FIFO deleted
  under the default policy, or, under any policy, one a visible process
  holds open or has bound in diskgc's network namespace.
- A Docker volume or container, a Nix GC root, profile or generation
  removed.
- Deletion following a symlink or crossing into another mount.
- A pressure tier running while an earlier tier still had eligible
  candidates that diskgc could have discovered.
- `ensure` exiting 0 when the space is not there.
- A static config error discovered only after something was deleted, or a
  tier's run-time checks run only after other tiers deleted.
- A `command` action run for a candidate that failed the safety checks, in
  a working directory other than the checked candidate (or its parent), or
  as root.
- A built-in profile whose matching set changed without a new name.

## Scope

Releases. **0.1, the `ensure` MVP**, is the first build that may delete on
a real machine: `ensure` (with `--select`, `--roots`, `--dry-run`, `--json`,
the exit codes, host lock, deadlines and signals), `check`, the config file
(schema `version: 1`, frozen by the contract review before 0.1), kinds `fs`
and `cargo-target`, and the verified `/tmp` and Cargo built-ins, as a
static binary with a tmpfiles drop-in and an example config. It ships only
with the Cargo race tests, the journeys for its users and the contract
review green; `watch`, the index, `survey`, Docker, Nix and command actions
come later. **v1** is the full scope below. ("v1" names the product
release; the config schema has its own `version: 1`.)

In scope (v1): `ensure`, `watch` (including `--tally-only`), `check`,
`survey`, `profiles`; profiles with `find` / `where` / `action`, the
qualified selection grammar, `protect`, user profiles and overrides; kinds
`fs`, `cargo-target`, `docker-build-cache`, `docker-images`, `nix-store`;
the verified part of the initial built-in library; the index, background
and synchronous tally, snapshot; `--json`; generic-Linux deployment
(static binary, systemd unit, tmpfiles drop-in), a NixOS module, and plain
binary + YAML for CI.

## Non-goals

- Disk usage analysis (use `ncdu`, `nix-du`); `survey` only reports what
  profiles match and the unmatched name families.
- Authoritative persisted state: no usage database, no cooldowns. The index
  snapshot is a cache.
- Copy-on-write filesystems with snapshots (Btrfs, ZFS), where deleting may
  free nothing (warned about); network filesystems as candidates.
- Event-driven watching (inotify/fanotify); coordination across hosts.
- Removing Nix generations/profiles, Docker volumes/containers.
- Cargo top-level entries outside profile directories (`target/doc`,
  `target/tmp`, `target/criterion`).
- Inode exhaustion and quotas.
- Running as root by default; command actions as root; Podman as the
  Docker backend; rootless Docker; non-default Docker builders; non-Linux;
  Linux older than 5.8.
- FUSE or `LD_PRELOAD` simulation for tests; a VM test suite, for now.
- Defence against a hostile local user racing diskgc.

## Prior art and why it is not enough

| Tool | What it does | Gap |
|---|---|---|
| Nix `min-free` / `max-free` | GC with hysteresis during Nix builds | Only triggers on Nix builds; ignores Cargo, Docker and `/tmp` writers. Complementary. |
| `nix.gc.automatic` | Timed Nix GC | Time-based, not pressure-based. |
| Docker `builder.gc` policy | Bounds BuildKit cache size | Docker only; size-based, not free-space-based. |
| `cargo-sweep`, `cargo-clean-all` | Age-based Cargo target cleanup | No lock awareness, no free-space goal, nothing else. |
| `systemd-tmpfiles` age cleanup | Age-based deletion (on the development machine: `/tmp` after 10 days); skips `flock`ed paths | No free-space trigger, no open-file or socket checks; defeated by atime refreshes and by most cruft being days, not weeks, old (observed: 80 % of a 45 GiB `/tmp` was under 3 days old). |

diskgc's contribution is the coordinator: one free-space goal, an ordered
policy across ecosystems, composable profiles, and in-use checks that apply
whatever the profile.

## Key decisions

| Decision | Chosen | Why |
|---|---|---|
| What vs when | Profiles (what and how) separate from tiers (which and when) | The same cruft knowledge serves every machine; only policy differs. |
| Composition | Qualified names; terms of one `profile:` (or a group) `&&` `filter:`s; `\|\|` only with parentheses; `!` only narrows | Composable by name, no precedence traps, nothing deleted that no profile found. |
| Roots | Fixed, or `roots: required` supplied by a tier, `--roots` or a `protect` entry | A selection or protection can never be re-pointed or left unbound. |
| Built-in profiles | YAML data with examples, golden sets and `verified_with`; a changed matching set means a new name | Narrowing is unsafe under negation and protection, so meaning is immutable per name. |
| Safety | Code, always on; eligibility walk, ownership of every entry, in-use and identity checks at deletion time | A wrong profile or a stale index costs at most unused own files or speed. |
| Nesting | Fixed structural recognisers (Cargo targets, `.diskgc-keep`, version-control checkouts), independent of the profile library | Editing a profile never changes what a parent's plain delete may remove. |
| Gather vs act | Throttled background tally builds an index; reclaim walk orders by it, discovers per tier, checks live | No survey when the disk is full; policy order holds even with a stale index. |
| Order in a tier | `oldest` default; opt-in `best-fit` | LRU by default; one sizable step when asked. |
| Deadlines | Cooperative, with reported overruns | Blocking kernel operations cannot be interrupted; honesty over a false promise. |
| Command actions | Trusted, preflight-resolved, never as root | Cleaners can load user-controlled config. |
| Docker | One bound backend per kind (local endpoint, default builder, storage root) | Tally, preflight and prune must act on the same thing. |
| Privileges | Non-root by default | Removes the "root runs commands from YAML" hazard. |
| Thresholds | Command-line flags; `ensure` target defaults to `--min-free` | Same config for laptop and CI; no surprise deletions. |
| Build and test | Rust, Nix flake, static musl binary; pure planner + executor over a `Host` port; YAML host model; rootless-container e2e | Rules tested as data, races deterministic, nothing tested on the live system. |

## Open questions

None blocking. Measured or verified facts and remaining checks are recorded
where they apply ([safety.md](docs/safety.md) for Cargo, Docker and Nix).

## Weak assumptions & risks

- **Cargo lock coverage.** Lock sets measured for the versions in
  [safety.md](docs/safety.md) (Lock files); the race tests run on the
  toolchains pinned in [testing.md](docs/testing.md), which also states
  what is acted on but untested (Cargo older than the oldest pinned).
- **Process heuristics.** Running binaries, dormant shells and Cargo's
  compile-to-run gap are protected by `/proc` heuristics with known blind
  spots (other users' processes, `--manifest-path`, variables exported after
  `exec`).
- **Deletion races.** Linux has no unlink-by-inode; a replace between the
  identity check and the unlink remains possible.
- **Built-in profiles come from one machine's survey.** Paths differ between
  tool versions and distributions; mitigated by `verified_with`, golden
  sets, `survey --list` review, and immutable names.
- **`claude-code-tmp`** deletes finished Claude Code sessions' outputs; the
  example config protects it.
- **Index coverage and cost.** The index can be partial (memory budget) or
  stale; per-tier discovery and live checks keep that a speed problem. The
  tally's duty cycle bounds the worker, not the disk.
- **Eligibility walks on giant candidates** delay the first deletion; time
  to first deletion is measured, not assumed.
- **Burst writes.** Thresholds cannot protect against a burst; the gap below
  `--min-free` must exceed write rate × (interval + slowest tier).
- **Nix GC** can take minutes and hold the host lock; with
  `auto-optimise-store`, freed space can lag; Lix and other variants are
  unverified.
- **Half-deleted Cargo `incremental/` state** might make rustc fail despite
  the fingerprints (`.fingerprint/`, or `build/<pkg>/<hash>/fingerprint` on the
  per-unit layout) being deleted first; resolved by race test 6.
- **`hidepid`** hides even more processes from the in-use checks on hardened
  systems.
- **Docker containerd root discovery** for filesystem matching must be
  settled during implementation; unknown layouts are skipped as
  `unsupported_backend`.
- **No real block filesystems, network filesystems or kernel versions in
  e2e**; those cases rest on the model.
- **Lock stalling.** Any local user can hold `/run/diskgc/lock`.

## Irreversibility map

Hard to walk back (deep review, tests before features):
- **The deletion path:** everything in [safety.md](docs/safety.md),
  especially the Cargo protocol and lock allowlist, the eligibility walk,
  the per-entry identity check, and the structural recognisers.
- **The CLI contract used by CI and harnesses:** `ensure` flags, exit codes
  and their precedence, JSON field names and enums, `.diskgc-keep`, the lock
  path.
- **YAML schema `version: 1`:** groups, tiers, the selection grammar and its
  semantics, the profile schema, `protect` entries, predicate names, and the
  names of built-in profiles and kinds.
- **The meaning of each built-in profile name**: its matching set never
  changes under that name.

Tentative (change freely):
- Example groups and tiers; default timeouts and back-off constants;
  human-readable output; log format.
- The `Host` trait, the crate split, the index structure, bucket rules,
  tally pacing and the snapshot format (a cache, discarded on version
  mismatch).
- The test host-model schema (pinned by regression files, migrated with
  them).
