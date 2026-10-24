# diskgc architecture: index, tally, reclaim walk, implementation

Normative for how diskgc gathers information and orders its work, and how
it is built and deployed. What is deleted and how safely is in
[safety.md](safety.md); the user-facing contract in
[reference.md](reference.md).

## Gathering and reclaiming

diskgc separates **knowing what could be freed** from **freeing it**. When a
disk is nearly full is the worst moment to discover what is big: walking a
45 GiB `/tmp` with 143,000 top-level entries took ncdu over 7 minutes on the
development machine.

1. A **tally** builds and maintains an **index** of what the profiles find,
   with size estimates, ages and last-checked times, slowly and in the
   background.
2. A **reclaim walk** takes candidates from a **priority queue** over that
   index and acts on them, after the mandatory per-candidate safety checks.

**The index is a cache, never an authority.** It decides the order of work
and saves global discovery and size tallying. It never decides whether a
deletion is safe: every check in [safety.md](safety.md), including the
eligibility walk of each candidate's tree, runs on the current filesystem
state at deletion time. A stale, partial, corrupt or missing index can make
diskgc slower or pick a worse candidate first; it can never make it delete
something the checks would refuse.

## The index

A set of trees, one per filesystem, of **units**: things the source
profiles found, with what the tally learned about them.

```
Unit {
  identity: single (mount, inode, birth time) | bucket (mount, parent, name family)
  path
  profiles                      # every source profile that found it (a set)
  members                       # bucket only: count (names are re-read, not stored)
  est_bytes, files              # allocated blocks, each inode counted once
  newest_change                 # newest mtime/ctime seen (for idle-* filters)
  checked_at, tallied_at
  cursor                        # resumable walk position while a big tree is being tallied
}
```

- **Granularity:** one unit per candidate (one pytest session, one Cargo
  target, one top-level `/tmp` entry). Small candidates (below 64 MiB) are
  merged per (profile set, parent directory, name family) into one
  **bucket**, so 98,000 tiny `nix-shell-*-*` rc dirs become a handful of
  units. Buckets exist for scheduling and storage only; every member is
  checked on its own at deletion time.
- **Identity** is (mount, inode, birth time from `statx`), falling back to
  (mount, inode, parent identity) where the filesystem reports no birth
  time. ctime and size are not identity; a change only means the estimate
  is stale. A path that several profiles find is one unit with a set of
  profiles, so it is counted once.
- **Memory budget:** the index (units and the path trie together) has a
  fixed budget of 64 MB, a constant in code (not configurable in v1).
  Raising the bucket threshold cannot merge candidates with different parents or name families, so the budget is the
  real bound: when it is reached, the tally stops adding units, the index
  reports `coverage: partial`, and the reclaim walk's per-tier discovery
  (below) still finds what the index lacks.
- **Trie and queue:** units live in a per-mount path trie, which gives the
  reclaim walk a cheap first answer to overlap questions (`protect`,
  `!profile`); the deletion-time checks answer them again from the disk.
  The **priority queue** is a derived view: for each tier, the units its
  expression selects on the requested filesystem, ordered by group, tier
  and the tier's `order`. Changing tiers rebuilds the queue, not the index.
- **Freshness** has two levels. A **re-check** is cheap (`statx` the unit
  and its parent: still there, same identity). A **re-tally** walks the tree
  to refresh size and `newest_change`; directory mtimes do not reveal growth
  deep inside a tree, so re-tallies are scheduled by age.

### Order within a tier

- `oldest` (default): least recently changed first (by `newest_change`).
  Keeps the LRU gradient even in tiers without an `idle-*` filter.
- `best-fit`: the smallest unit whose estimate covers the remaining need,
  otherwise the largest. Frees something sizable in one step when the disk
  is nearly full. Buckets count as many deletions and rank after single
  units of the same size. Because the next pick depends on space actually
  freed, `best-fit` is adaptive and has no exact dry-run equivalent.

## The tally

- **Background (in `watch`):** the tally runs forever in mini-batches of at
  most 50 ms or 2,000 filesystem operations, with a **worker duty cycle**:
  after a batch that took *t*, it sleeps 19 × *t*, so the tally worker is
  active about 5 % of wall-clock time whatever the device. This bounds the
  worker, not the device: it does not measure device busy time, and several
  `watch` processes on one disk add up. It also runs at idle I/O priority
  and `nice 19` (idle I/O priority is honoured only by the BFQ scheduler; on
  NVMe with scheduler `none` the duty cycle is the only limit). Actual
  contention is measured separately in tests. Big trees are tallied across
  many batches via their `cursor`, which survives entries vanishing under it
  (`ENOENT` ends that branch).
- **Order of work:** discovery (list profile roots for new and vanished
  candidates), re-checks, re-tallies, with aging so that none starves the
  others on a busy `/tmp`.
- **Pausing:** before each batch the tally tests, without blocking, whether
  anyone holds the host lock; if so, a reclaim is running (in this or
  another process) and the tally waits. The tally never speeds up under
  pressure.
- **Synchronous mode:** without a usable index (`ensure` on a runner with no
  `watch`, `--no-index`, or `--select` / `--roots` naming roots no index
  covers), the same engine runs first at full speed for at most
  `--tally-budget`, **before** taking the host lock (it is read-only); the
  reclaim walk then uses the partial index plus per-tier discovery.
- **Snapshot:** `watch` writes its index atomically (temporary file +
  rename) every 5 minutes and on exit, to
  `$XDG_STATE_HOME/diskgc/index-<fs>` (mode 0600: it lists paths of the
  user's files, no contents), with a format version and a hash of what
  shapes the index (profiles and roots, not tiers). `<fs>` is the
  filesystem's UUID where it has one, otherwise its device number; not
  `f_fsid`, which is 0 on tmpfs on older kernels, and not the mount path. A
  single writer is enforced with a separate lock file, `index-<fs>.lock`,
  that is never renamed; a second `watch` for the same filesystem and user
  fails at start (exit 2). `ensure` and `survey` use the snapshot if version
  and hash match and it is younger than an hour, otherwise they tally
  synchronously; a snapshot of another user is unreadable, and the report
  says so. A mismatched or corrupt snapshot is ignored with a log line.
- **Docker units** carry the reclaimable size the bound backend reports
  (`docker buildx du`, `docker system df`), or no size if unknown; TASK-120
  settles which query, since Buildx writes client configuration.
- **Nix units carry no size: Nix is sized only by its action.** The only
  way to learn what a GC would free is Nix's own liveness computation
  (`--print-dead`), which takes as long as the root discovery of a real GC,
  reports no sizes, and unlinks stale `gcroots/auto` registrations, so it
  is neither read-only nor cheap enough for a tally or a dry run. The
  consequences: a `nix-store` tier is one unit with `estimated_bytes:
  null`; its place in the policy is decided by its group and tier alone,
  never by size (`best-fit` has nothing to compare, and a tier holds one
  Nix unit); a dry run cannot say whether the GC would free anything; and
  a reclaim runs the GC whenever earlier tiers left the goal unmet, even
  when little is dead, costing the GC's fixed time (root discovery and the
  `.links` scan, [safety.md](safety.md#nix-store)). Put the Nix tier after
  cheaper tiers that free comparable space. TASK-98's out-link estimate
  is a separate, hypothetical-root analysis, not this tally.

## The reclaim walk

```
reclaim(min, goal, filesystem, deadline):
  if avail(filesystem) >= min: return met            # no lock taken
  index := live index (watch) | fresh snapshot | synchronous tally (budget)
  lock /run/diskgc/lock, waiting until deadline
    on timeout: return met if avail >= min else lock_timeout
  if avail(filesystem) >= min: return met            # someone else freed it
  preflight all tiers (environmental validation, backend binding)
  for group in groups, tier in group (ranking):
    queue := units of this tier on this filesystem, in the tier's order
    discovered := false
    loop:
      if avail >= goal: return met
      candidate := next from queue
                   or, once the queue is empty and not discovered yet:
                   discover this tier's sources (bounded by their patterns)
                   for candidates not in the queue; discovered := true
                   or: break                            # tier done
      cheap re-check of identity; gone or replaced: skip (stale)
      eligibility walk + in-use + selection re-evaluated now   # safety.md
        failed: record skip with reason; continue
      act(candidate, need = goal - avail); record avail_delta
  return met if avail >= min else (error if any errors else exhausted)
```

- **Discovery is per tier.** A tier is not left until its own sources have
  been discovered afresh, so a stale index can never make a pressure tier
  run while candidates of an earlier stale tier exist but are unknown.
  Discovery costs what the profiles' patterns cost (fixed depths, no `**`);
  for `roots: required` profiles such as `cargo-target` it is a bounded walk
  of the roots.
- **The first deletion waits only for its own checks.** With a warm index,
  the time to the first deletion is the eligibility walk of the first
  candidate, not a survey. A giant candidate costs a giant walk; that is
  the price of checking its contents.
- Every step checks the cooperative deadline
  ([safety.md](safety.md#deadlines)). `watch` calls the walk with
  `min = --min-free`, `goal = --target-free`; `ensure` with its two flags.
- `exhausted` is reported only after every tier's discovery, so an index
  that missed something can delay success but never fake exhaustion.
  `stale`: the unit was gone or replaced at re-check. `changed`: something
  moved under a check (a Cargo lock or build appeared after the locks were
  taken, or an entry's identity changed during deletion).
- Until the index exists (task-28 to task-31), `ensure` runs one
  `--select` tier this way without one: after taking the lock it discovers
  the tier's candidates and tallies each (the eligibility walk without the
  idle and in-use checks, read-only) for the tier's order, within
  `--tally-budget`; candidates the budget left untallied follow the
  tallied ones in the order discovery found them. It then acts on each in
  that order, every check made again under the candidate's own locks (a
  Cargo target's lock files, an `fs` candidate's `flock`;
  [safety.md](safety.md#deletion)), so a candidate that is not idle is
  locked and walked before it is skipped. A candidate the tally refuses
  is skipped for the run and never locked: re-judging it under its locks
  would take Cargo's locks on kept and other users' targets (or `flock`
  a kept directory) and walk oversized ones again. A refusal that
  stops being true after the tally (a `.diskgc-keep` removed) is a missed
  reclaim until the next run, never an unsafe delete. The tally runs
  under the host lock rather than before it (The tally: Synchronous
  mode), so other reclaims wait that much longer.

### Measuring space

- **Available space** = `f_bavail × f_frsize` from `statvfs`: what an
  unprivileged writer can use, never the root reserve. No `syncfs` (it
  waits for unrelated I/O and cannot be cancelled). Some filesystems free
  blocks asynchronously, so if a sample after an action shows less than
  expected, diskgc re-samples a few times over at most 2 s.
- **`avail_delta`** is the observed net change around an action, signed:
  concurrent writers can make it negative, unrelated deletions can inflate
  it. Success is judged only by the final sample.
- **Stopping:** diskgc stops as soon as an observed sample reaches the goal.
  It does not bound eventual reclamation: if a filesystem frees blocks later
  than the re-sampling window, further candidates may already have been
  deleted by the time the space appears. Docker and Nix actions are whole
  operations and can overshoot by design; a Nix GC's `--max-freed` is a
  lower bound on Nix's own byte count, not on available space, and in an
  optimised store its space appears only at the end of the GC
  ([safety.md](safety.md#nix-store)).
- **Tallies** count allocated blocks and each inode once (Cargo hard-links
  binaries from `deps/`), and are estimates; they order work and are never
  used to decide success.

### Filesystem matching

Before any tier runs: filesystem candidates (including Cargo targets) by
device and mount ([safety.md](safety.md#path-traversal)); Docker by its
bound backend's storage root; Nix by the device of `/nix/store`, the only
store it collects ([safety.md](safety.md#nix-store)).
A tier whose backend is elsewhere is skipped and reported (`other_fs`,
`remote_backend`): deleting there would hurt a cache and free nothing here.

## Hysteresis

| Where | Mechanism |
|---|---|
| `watch` | `--min-free` triggers, `--target-free` (required) is the goal. |
| `ensure` | Same pair; `--target-free` defaults to `--min-free` (no surprise deletions); CI hooks set it higher. |
| Candidate eligibility | `idle-*` filters: recently used caches are not eligible. |
| Nothing left to free | `watch` back-off until space drops further or 15 minutes pass. |
| Within a reclaim | Stop as soon as the goal is observed. |

Per-tier cooldowns are not included: the snapshot could carry them, but it
is a cache that may be missing, and a cooldown that silently disappears is
worse than none. The cost (a no-op Nix GC per CI job when nothing is dead)
is accepted until measured otherwise.

## Implementation

- **Rust everywhere**: the product, the test harness, the host-model
  generator, test helpers. No shell scripts in product or tests; `just`
  recipes only call Nix, Cargo and Podman.
- **Nix flake** is the single development entry point:
  - build with **crane** (one shared dependency build for package, clippy
    and tests), with source filtering so that editing docs rebuilds nothing;
  - product toolchain from **rust-overlay**, pinned by `rust-toolchain.toml`;
    the binary is statically linked (musl) so it runs on any distribution;
  - *Cargo under test* is separate: pinned toolchains from the same locked
    rust-overlay, in the test image ([testing.md](testing.md#cargo-toolchains-under-test));
  - outputs: `packages.default`, `nixosModules.default`,
    `devShells.default`, `checks` (fast checks and evaluation of the NixOS
    module), and the container images for the e2e tests.
- **Cargo workspace:**

  The product workspace and the separate `diskgc-e2e` workspace both set
  `overflow-checks = true` in their release profiles. The flake builds the
  shipped binary and runs its Rust tests with release optimizations; checked
  arithmetic there must panic on overflow rather than silently wrap while
  deciding whether to delete. This can cost some throughput, which the
  container scale tests measure on the same release builds.

| Crate | Role |
|---|---|
| `diskgc-core` | Config schema and grammar; the index types and queue derivation; the **planner** (pure: index in, plan, skip reasons, exit-code precedence and report out); the **tally** and the **executor** (re-check → walk → checks → act → re-sample), both over the `Host` port; the `Host` trait. No syscalls of its own. |
| `diskgc-linux` | `LinuxHost`, the real `Host`. A library, because the contract tests use it. |
| `diskgc-model` | `ModelHost`, a `Host` backed by a YAML host model, and the model-test harness. Test-only. |
| `diskgc-modelgen` | Generates host-model values (fixtures and `proptest` strategies). Test-only. |
| `diskgc-e2e` | Destructive tests that need a real kernel, their harness and the host-side runner. Excluded from the workspace; built by the flake only. See [testing.md](testing.md). |
| `diskgc` | The binary: CLI and output; installs signal delivery into `LinuxHost`; wires `diskgc-core` to `LinuxHost`. |

### The `Host` port

- **Two shapes of operation.** Files and locks are syscall-shaped
  (`openat2`, `statx`, `getdents` (names only), `flock`, `unlinkat`,
  `statvfs`, `mountinfo`, `/proc` reads), so the safety logic (flags,
  ordering, rechecks) lives in the core where the model can test it. Docker and Nix are domain-shaped, in two
  operations. A **query** run (`docker context`/`builder`/`info`, `nix
  store info --json`) is on `Host`: not gated, so `survey`, `--dry-run` and
  `--tally-only` work anywhere; it takes a closed enum of queries that
  `LinuxHost` maps to argv, never argv from the caller or from config. A
  **deleting** run (a prune, a GC, every command action) is
  `run(argv, env, deadline)` on `Delete`, behind the pre-release sandbox
  gate. Each has one wrapper in `LinuxHost`, allowlisted by the source scan
  ([safety.md](safety.md#pre-release-sandbox-gate)).
- **What the in-use check reads** ([safety.md](safety.md#in-use)): the
  `/proc` readers; per process its `status`, `uid_map` and the owner of a
  live task's `fd` directory (`proc_status`, readable for every process);
  diskgc's own pid; and two socket-diagnostics dumps of the Unix sockets
  bound in diskgc's network namespace (`bound_unix_sockets`, a read-only
  `NETLINK_SOCK_DIAG` query: ungated, like the query runs). Reading a list
  of processes is a provided method (`proc_views`) that calls the readers
  one process after another, so `ModelHost` sees every reader call;
  `LinuxHost` reads several processes at once, each on its own, and the
  test driver's wrapper passes the call through to it unless a pause or
  kill point names a reader.
- **diskgc's own state** (the index snapshot) is written through one port
  operation that takes only a typed key (no fd or name from the caller):
  `LinuxHost` opens the state directory itself, creates the temporary file
  with `O_CREAT|O_EXCL` (or `O_TMPFILE` and `linkat`), never `O_TRUNC`, and
  renames only onto `index-<fs>`; `ModelHost` checks the same shape. It is
  not gated (it is diskgc's
  cache, not user data, and `watch --tally-only` must work anywhere). It
  and the creation of its lock file (`index-<fs>.lock`, decided in the
  snapshot task) will be the only writes or renames the scan allows
  outside `Delete`; until the snapshot exists, the scan allows none.
- **Time is part of the port.** The clock, every blocking operation (host
  lock wait, child wait, the SIGTERM → 10 s → SIGKILL sequence, re-sampling)
  and the tally's batch timing and sleeps are port operations; `ModelHost`
  keeps a virtual clock and delivers signals as events, so deadlines, exit
  codes 75 / 124 / 130 / 143 and the duty cycle are model-testable. Two
  clocks: `now` (monotonic, including suspend) for deadlines and freshness,
  `wall_clock` (realtime, the scale of file times) only for comparing with
  file times in the idle filters.
- **File contents are not read**, with one exception: `read_head` reads the
  first bytes of a small marker file (a `CACHEDIR.TAG`'s signature) through
  the descriptor of the object checked, never by name again; anything but
  a regular file is refused on that descriptor's type before it is
  opened for reading (a FIFO or a device is never opened). diskgc's own
  configuration file is read by the binary, outside the port
  (`LinuxHost::read_config_file`, the same type check first, at most
  1 MiB): it may be a symlink, and only its content is validated; parsing
  it is pure.
- **Locking a file checked by identity** (Cargo's lock files):
  `reopen_regular` opens again, read-only and non-blocking, the regular
  file an `O_PATH` fd refers to, through that fd (never by name), refusing
  anything else by the fd's own type before opening it, since `flock`
  refuses an `O_PATH` fd ([safety.md](safety.md#reclaim-protocol)).
- **Deleting operations are a trait of their own** (`Delete: Host`:
  `unlinkat`, `fchmod` for `make_writable`, later the deleting `run`, and
  `may_delete`, the gate's decision asked before a reclaim starts), so
  code given only a `Host` (the dry run, survey, tally) cannot delete or
  change permissions, and the pre-release sandbox gate guards exactly
  those methods.
- **Who diskgc is, and what its children keep:** `effective_uid` (the
  ordinary-mode owner every entry must have, and what `{uid}` stands for;
  root mode uses the resolved `owners:` IDs for entry ownership) and
  `inherit_lock`, which makes a held lock inheritable by children by
  clearing close-on-exec on that one fd (the host lock,
  [safety.md](safety.md#host-lock)).
- **Listings are bounded**: `read_dir` takes a limit and stops past it
  (the deletion engine passes what is left of a candidate's entry budget),
  so a huge directory, or one filled faster than it is read, cannot exhaust
  memory or keep a listing going.
- **`ModelHost` checks how it is called**: an `openat2` without the required
  `RESOLVE_*` flags, a `statx` of more than one name or from the root, or
  an `unlinkat` without an identity check right before it fails the test
  ([testing.md](testing.md#2-model-tests-a-yaml-host-model-instead-of-a-real-disk)).
  Releasing or re-locking a held lock cannot be expressed:
  the lock calls take the fd by value and return a `Locked` handle that
  lends the fd out by reference only and closes it when dropped, and there
  is no unlock operation; the model keeps each lock exactly as long as an
  fd refers to its open file description.
- **statx fields are optional in the port**: a field the kernel did not
  mark as answered (`stx_mask`, `stx_attributes_mask`) is absent, so the
  core cannot use it by accident and `ModelHost` can model a filesystem
  that does not answer.

## Deployment

- One statically linked Rust binary, Linux 5.8 or newer, any distribution.
  Nix, Docker and Cargo are optional. It needs the `/proc` of its own pid
  namespace and `pidfd_open` (not blocked by seccomp), and refuses to start
  otherwise ([safety.md](safety.md#in-use), unreadable or vanishing
  processes).
- **Generic Linux:** the binary, a systemd template unit for `watch` (user
  or system), the tmpfiles drop-in for the host lock file, and an example
  config. Distribution packages can follow.
- **NixOS**, as one option: a module that renders the YAML, ships the
  tmpfiles rule, runs `check` at build time, and starts the `watch`
  instances.
- **CI runners:** binary, YAML, the tmpfiles drop-in (or equivalent), and
  `diskgc ensure --min-free 50G --target-free 80G "$CI_BUILDS_DIR"` in the
  pre-job hook; optionally `watch --tally-only` between jobs for a warm
  index.
