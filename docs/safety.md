# diskgc safety: what is checked before anything is deleted

Normative for every deletion protocol. The guarantees in the PRD's "What bad
looks like" are implemented here; where a guarantee has limits, they are
stated here, once.

## Principles

- **Safety is code, always on.** No profile, tier or flag switches these
  checks off. A wrong profile can at worst select the wrong *unused,
  own* files, never files in use as far as diskgc can see.
- **Positive only.** Nothing is a candidate unless a source profile found it
  ([reference.md](reference.md#profiles)).
- **Checked at deletion time.** The index only orders work
  ([architecture.md](architecture.md)); every check below runs on the
  current filesystem state immediately before acting.
- **Not a defence against a hostile local user.** diskgc protects against
  deleting things that are in use, belong to others, or lie outside
  candidates. It assumes no process deliberately races it by renaming or
  replacing entries inside a candidate while it works; where such races
  are possible, the remaining window is stated.

## Filesystem candidates

Every candidate of kind `fs` or `cargo-target`, whichever profile found it.

### Before the walk

- **Mounts first.** `mountinfo` is read before anything is `stat`ed. A
  candidate that is a mount point or contains one (a user's FUSE or sshfs
  mount in `/tmp` is common) is skipped (`contains_mount`) without being
  touched, so a dead mount cannot hang the scan. Network filesystems (NFS,
  CIFS, sshfs) are never candidates' filesystems (`unsupported_fs`).
- Entries of systemd's `PrivateTmp` (`systemd-private-*`) are never
  candidates.

### The eligibility walk (mandatory, per candidate)

Immediately before acting, diskgc walks the candidate's whole tree
(fd-relative, see Path traversal) and requires all of:

- **Ownership of every entry**, not just the candidate: every file and
  directory is owned by the user diskgc runs as (or, under `--allow-root`,
  by a user in `owners:`). One entry owned by someone else makes the whole
  candidate `not_owner`: unlink permission is a property of the containing
  directory, so a user-owned directory can hold another user's files.
- **Idle:** for an `fs` candidate, its newest mtime/ctime is older than the
  tier's `idle_for` (if any). Cargo targets use their own measure (below).
- **No keep marker** (`.diskgc-keep`) in the candidate, anywhere inside
  it, or in any directory between the profile root and the candidate
  (`kept`).
- **No special content** (`contains_special`): nothing inside matches
  diskgc's fixed **structural recogniser** for Cargo targets: a directory
  with `CACHEDIR.TAG` whose profile directories (`<t>/<p>/` or
  `<t>/<triple>/<p>/`) hold any file matching `.cargo-*lock*`, known or
  unknown name alike.

  The recogniser is code, independent of the profile library, so adding or
  editing a profile never changes what a plain `delete` of an enclosing
  directory may remove. Content that a `command` profile would clean (e.g. a
  Go cache inside a deleted `/tmp` directory) is *not* protected by
  nesting; protect it with `protect:` or `!profile:` if it matters.
- **No sockets or FIFOs** inside (`contains_ipc`): those belong to running
  programs (agents, X11, tmux, ssh).
- **Not protected or excluded** by overlap with a `protect` entry or a
  negated profile ([reference.md](reference.md#protect)).

- **Fail closed on anything unreadable:** a directory the walk cannot
  open or list (mode `000`, `EACCES`) makes the candidate ineligible
  (`not_owner`); it is never treated as empty.

The walk records every entry's (device, inode) for the in-use check. Its
cost is proportional to the candidate's size: the index saves discovery and
size tallying, not this walk.

### In use

A candidate is skipped (`in_use`) if a visible process:

- has an open file descriptor, working directory, root, or memory mapping
  on any of the candidate's inodes. Matching is by **device and inode**:
  `/proc/<pid>/fd/*`, `cwd` and `root` are `stat`ed, `maps` is read by its
  dev:inode columns. Path text would be wrong for processes in other mount
  namespaces (PrivateTmp, flatpak, containers);
- has, in its **initial** environment (`/proc/<pid>/environ`), a variable
  whose value is a path inside the candidate (e.g. `TMPDIR` of a dormant
  `nix develop` shell). This is a heuristic: `environ` shows the environment
  at `exec`, so a variable changed later by `export` is invisible.

Or it is a directory locked with `flock` by someone else (`locked`), the
convention systemd-tmpfiles also honours.

**Unreadable or vanishing processes:** a process of the same user whose
`fd` directory cannot be read (`EACCES`: non-dumpable processes such as
`gpg-agent`, `ssh-agent` or setuid children) makes every candidate it could
be using `in_use`, i.e. it is treated as using everything it might; a
process that exits during the scan (`ESRCH`, `ENOENT`) is simply gone.
**Overlay and btrfs:** `maps` can report a different (device, inode) than
`stat` on overlayfs and btrfs subvolumes; diskgc matches both the reported
pair and the underlying inode where the kernel exposes it, and the
differential tests cover overlay.

**Freshness:** the `/proc` scan used for a candidate is never older than one
second (monotonic clock including suspend time) when that candidate's
deletion starts. Buckets
([architecture.md](architecture.md#the-index)) only group candidates for
scheduling; each member gets its own check under this bound.

**Visibility:** without root, other users' `/proc/<pid>/fd` cannot be read,
so their use of *your* files is not seen. Fine for one-user machines and
runners; weaker on shared hosts.

### Deletion

For the `delete` action:

1. Take an exclusive `flock` on the candidate (directories), as
   systemd-tmpfiles does, so cooperating tools see it is being removed.
2. Under that lock, run the eligibility walk and the in-use check (above).
   Any failure: release, skip.
3. Delete depth-first, reopening each directory relative to its parent
   (`openat2`, see Path traversal) and checking its (device, inode) against
   the walk's record, so at most one directory per level is open at a time
   (no file-descriptor exhaustion on large trees). Before each `unlinkat`,
   `fstatat(AT_SYMLINK_NOFOLLOW)` the name and compare its (device, inode,
   owner) with what the walk recorded; a mismatch skips that
   entry and marks the candidate `changed`. `make_writable` (if the profile
   allows it) adds owner write permission to a directory here, after the
   checks.
4. Release.

**Scope** means *the objects the walk checked*. Every directory diskgc
descends into is matched by (device, inode) against the walk's record, and
the candidate itself is held open while it is deleted, so if a checked
directory is renamed elsewhere mid-delete, diskgc keeps deleting that same
object (the cruft it checked), never whatever now sits at the old path.
Two windows remain and are accepted: an entry replaced between the `fstatat` and the
`unlinkat` of its name (Linux has no unlink-by-inode), and a file opened or
created inside the candidate after step 2. A file created mid-delete makes
the delete partial (`ENOTEMPTY`); that is reported as an error for the
candidate, never retried silently.

### Other properties

- **Hard links:** removing a name that is hard-linked elsewhere frees
  nothing; the signed `avail_delta` shows that.
- **Deleted but open:** a file deleted while still open keeps its space
  until the process closes it. Every report lists such files on the
  requested filesystem (`held_by_deleted_files`). diskgc never kills
  processes.
- **tmpfs:** if `/tmp` is a tmpfs, profiles under it only help a request for
  that tmpfs (filesystem matching), which is correct: it frees memory, not
  disk.

## `cargo-target`

The filesystem checks above apply (mounts, ownership of every entry, in
use, protection), with Cargo's own rules replacing the generic deletion
steps.

### Lock files

Cargo's lock set changes between versions. Measured on 2026-10-15 by
sampling `/proc/locks` during real Cargo commands:

- Cargo 1.90 (stable) holds an exclusive `flock` on `<profile>/.cargo-lock`
  during a build.
- Cargo 1.96 (nightly) holds `.cargo-build-lock` *and* `.cargo-lock` during
  a build; a second build waits on `.cargo-build-lock` first; `cargo check`
  waits behind a running build.
- **Neither holds any lock while the program runs** under `cargo run` or
  `cargo test`, for the whole runtime of the binary.
- `.cargo-artifact-lock` exists in some targets on the development machine,
  but neither version created it with plain `build` / `run` / `test`.

diskgc has an explicit **allowlist of known lock file names** (`.cargo-lock`,
`.cargo-build-lock`, `.cargo-artifact-lock`). A profile directory containing
any other `.cargo-*lock*` file is an unknown layout: the target is skipped
(`unsupported_layout`). Lock files are never deleted or moved.

### Discovery, idleness, eligibility

- **Discovery:** walk `roots` (depth limit 6) for directories with a valid
  `CACHEDIR.TAG` and at least one profile directory holding a known lock
  file, at `<target>/<profile>/` or `<target>/<triple>/<profile>/`. Do not
  descend into a found target. Never follow symlinks or cross mounts.
  Targets on network filesystems are refused (`unsupported_fs`): Cargo does
  not lock there.
- **Last build** (the kind's `idle_for` measure): newest mtime of the
  profile directories and their `.fingerprint/`, `deps/`, `build/`,
  `incremental/`. It changes on real rebuilds but not on no-op builds, so an
  up-to-date, actively used target can look idle; the lock and process
  checks still protect visible running work.
- **Empty:** a target whose profile directories hold nothing but lock files
  is `empty` and not re-listed on every run.

### In use

- any known lock file cannot be `flock`ed without waiting (`locked`);
- a visible process has its executable or working directory inside the
  target (`in_use`; e.g. a test binary still running);
- a visible process whose executable name, after normalising Nix wrapper
  names (`.cargo-wrapped` → `cargo`), starts with `cargo` has its working
  directory inside the target's project directory (the target's parent),
  or `CARGO_TARGET_DIR` pointing at the target in its initial environment
  (`in_use`).

Because Cargo holds no lock while a binary runs, these process checks are
the only protection during `cargo run` / `cargo test`. They are heuristics:
a Cargo started from outside the project directory without
`CARGO_TARGET_DIR` (e.g. `--manifest-path`, or a `target-dir` in
`.cargo/config.toml`) is not recognised between compilation and launch,
and a deleted binary then makes `cargo run` fail with "No such file". A
strict guarantee would need Cargo itself to hold a lock until the run ends.

### Reclaim protocol

1. Take every known lock file of the target, exclusively, without waiting.
   Any failure: release all, skip (`locked`). A Cargo that holds
   `.cargo-build-lock` while waiting for `.cargo-lock` makes this fail,
   which is intended.
2. List the lock files again, re-read the last build time, run the
   ownership walk and the process checks. A new lock file (a profile
   created since discovery), a newer build or a hit: release, skip
   (`changed` / `in_use`).
3. In each locked profile directory: delete `.fingerprint/` first (so a
   half-deleted profile always forces a rebuild), then every other entry
   except the lock files, with the per-entry identity check from Deletion.
4. Release the locks.

Nothing outside the locked profile directories is touched: not
`CACHEDIR.TAG`, not the triple directories, not top-level entries such as
`target/doc`, `target/tmp` (written by running tests without a lock) or
`target/criterion` (benchmark baselines, which are data). Because lock
files never move, a Cargo blocked on a lock and a Cargo started afterwards
lock the same inodes and stay mutually exclusive; a Cargo started during
step 3 prints `Blocking waiting for file lock` and continues after step 4.

## Docker kinds

Each Docker operation is bound to **one backend**, determined in the
preflight and used for tally, preflight and execution alike:

- **Endpoint:** the active Docker context must be the system daemon's local
  Unix socket; a remote context is skipped (`remote_backend`), rootless
  Docker (a socket under `$XDG_RUNTIME_DIR`) is not supported in v1
  (`unsupported_backend`).
- **Builder** (`docker-build-cache`): the default builder with the `docker`
  driver (the daemon's own BuildKit). Other builders (`docker-container`,
  `kubernetes`, `remote`) are not supported in v1 (`unsupported_backend`).
- **Storage root:** `DockerRootDir` for build cache; for images, the image
  store's root (the containerd root when the containerd image store is in
  use, which is the default on Docker 29). If the storage that an operation
  frees is not on the requested filesystem, the tier is skipped
  (`other_fs`); if it cannot be determined, or spans several filesystems,
  it is skipped as `unsupported_backend` and reported.

Operations:

- `docker-build-cache`: `docker builder prune --all --force
  --filter until=<idle_for>`. Without `--all`, Docker removes only dangling
  cache. The `until` filter counts from **last use** (verified on Docker
  29.9 in a disposable `docker:dind`: entries used 5 s earlier survived
  `until=30s`; after 45 s idle they were removed).
- `docker-images`: `docker-dangling` runs `docker image prune --force`;
  `docker-unreferenced` runs `docker image prune --all --force`, which also
  removes tagged images no container references. No age option: the image
  `until` filter is never "last used" (with the containerd store it is the
  local pull time, verified; with the classic store the build date, per
  Docker's documentation).
- One action prunes everything matching; there is no per-candidate stop.
  Volumes and containers are never touched.

## `nix-store`

`nix-store --gc --max-freed <remaining need>`. Nix decides liveness (GC
roots, temporary roots, runtime roots of running processes); generations and
profiles are never removed. `--max-freed` means "stop after at least this
much"; the effect is measured, not assumed.

Store selection: `NIX_REMOTE` or the `store` setting can point `nix-store`
at another store. Before GC, diskgc asks Nix, with the exact environment it
will pass to `nix-store`, which store is in effect
(`nix --extra-experimental-features nix-command store info --json`, whose
`url` names the store). Supported: `url` is `daemon`, or `local` without
`root=`, `store=` or `real=` parameters, and `builtins.storeDir` is
`/nix/store`; the filesystem of `/nix/store` is the one matched. Anything
else: the tier is excluded and reported (`unsupported_store`).

Verified on Nix 2.34.8 (2026-10-15): an untrusted, non-root user can delete
dead store paths through the daemon (`nix-store --delete` on a freshly
added path; `--gc --max-freed` uses the same daemon operation but was not
run, to avoid deleting the machine's real garbage). `builtins.storeDir`
alone is not enough: it still says `/nix/store` when `NIX_REMOTE` points
elsewhere. Lix and other Nix variants are not verified.

## Command actions

- **Trusted, arbitrary command execution.** diskgc runs the filesystem
  checks on the candidate *before* the command (ownership of every entry,
  mounts, in use, locks), but cannot limit what the command deletes; the
  profile author owns that.
- **Never as root.** Under `--allow-root`, tiers with `command` actions are
  refused at load in v1: a trusted cleaner run inside a user-controlled
  directory can load user-controlled configuration or code (Yarn's
  `yarnPath`, for example, replaces the executable for commands in that
  directory), which no check on diskgc's own config prevents.
- **The tool re-resolves the path it is given.** It receives a path string
  and resolves it itself, following symlinks; diskgc's traversal
  guarantees end at the `exec`. Where the tool allows it, the command should
  use `.` with the candidate as working directory (entered via `fchdir`, so
  it is the checked directory).
- **Executables** are resolved at run time, in the preflight, from the
  `PATH` diskgc runs with; every executable the command will run is shown
  in the report (for `[env, GOCACHE={path}, go, ...]` that includes `go`,
  not only `env`). A tool that is not installed makes the profile `missing`
  (skipped), not an error.
- Runs in its own process group; killed after `timeout`; inherits the host
  lock (below).

## Path traversal

All directory work is fd-relative. To reach a configured root or a
candidate:

1. In `/proc/self/mountinfo`, pick the deepest mount point that is a path
   prefix of the configured path (after unescaping `\040` and friends;
   where mounts are stacked, the topmost one).
2. Open it with `O_PATH | O_DIRECTORY | O_NOFOLLOW` and confirm with `statx`
   that it is that mount: `STATX_ATTR_MOUNT_ROOT` set and `stx_mnt_id`
   equal to the mountinfo id.
3. Resolve the rest relative to it with
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`; if the
   path is the mount point itself, the rest is `.`.

A `/home -> /var/home` symlink on the root filesystem fails in step 3
(`ELOOP`) and becomes `unsafe_path`. This refuses symlinks and any mount
crossing below the root, including a bind mount of the same filesystem,
which a device comparison would miss. (`statx` on the full path is not used
to find the mount: it follows symlinks in the middle of the path.)

The walk and the deletion descend with
`openat2(... RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS)`; `statx` results are
trusted only when the attribute's bit is set in `stx_attributes_mask`, and a
check the kernel cannot answer skips the candidate (`unsupported_fs`).
Minimum kernel: Linux 5.8; older kernels exit 2 at startup.

## Validation

- **Static** (`check`, and every startup): YAML syntax, schema, unknown
  keys, `version`, unique tier names, profile names, absolute paths, no
  `~`, size and duration syntax, the selection grammar (qualifiers, one
  source per term, no unparenthesised mixing, `!` only narrows), one kind
  and one action per expression, predicates supported by the kind, roots
  fixed vs required, `protect` entries, `override:` on shadowed built-ins,
  command actions refused under `--allow-root`, profile `examples`. Needs
  nothing from the machine, so it runs in a build sandbox and when
  cross-building. Failure: exit 2, nothing deleted.
- **Environmental** (at the start of every reclaim, for *all* tiers, before
  the first deletion): roots and paths exist, contain no symlinks, resolve
  to a device and mount; `owners:` names resolve to non-system users;
  executables of command actions resolve; Docker and Nix backends are bound
  and reachable.
  - A missing path or tool, another filesystem, or a remote Docker context
    is normal: that tier is skipped with its reason (`missing`, `other_fs`,
    `remote_backend`).
  - A misconfiguration (`unsafe_path`, `unsupported_store`,
    `unsupported_backend`) excludes that tier and is reported as an error
    (exit 3 if the goal is then not met). The other tiers still run:
    refusing all cleanup because one tier is broken would let the disk
    fill.
  - A daemon that is installed but down is a backend error: reported, and
    `watch` retries soon.

## Host lock

- One reclaim per host at a time, via `flock` on the fixed path
  `/run/diskgc/lock`, opened read-only. Not configurable: two configs naming
  two lock files would silently break mutual exclusion.
- The lock file is provisioned by whatever installs diskgc: on systemd
  machines a tmpfiles.d drop-in (`/run` is emptied on reboot), elsewhere the
  equivalent boot-time step:

  ```
  d /run/diskgc      0755 root root -
  f /run/diskgc/lock 0644 root root -
  ```

  Missing → exit 2, printing those two lines.
- Any local user who can read the file can hold the lock and stall
  reclaims; `ensure` then exits 75 at its deadline.
- Every child inherits the host lock's file descriptor (close-on-exec
  cleared on that one fd only), so the lock stays held until the last child
  and its children exit; a SIGKILLed diskgc never leaves a child deleting
  without it, at the cost that an orphaned child stalls other reclaims until
  it ends. diskgc releases the lock only by `close()`, never
  `flock(LOCK_UN)`, which would release it for every inherited copy. Limits:
  best-effort for command actions (tools such as `sudo` or `ssh` close
  inherited fds), and work inside daemons (a Nix GC, a Docker prune) keeps
  running after its client dies; overlapping prunes waste work but do no
  harm. (`PR_SET_PDEATHSIG` was rejected: it fires on parent *thread* death
  and does not reach grandchildren.)

## Pre-release sandbox gate

Until v1 is released, diskgc's deleting primitives (unlink, rmdir, the
delete walk, and starting prune, GC or command children) refuse to act
unless they run inside a test container, so every entry point, including
test drivers, is covered: both the image marker `/.diskgc-test-sandbox` and
Podman's `/run/.containerenv` must exist (the same markers as the test
interlock, see [testing.md](testing.md)). `check`, `survey`, `profiles`,
`--dry-run` and `--tally-only` work anywhere. The gate is compiled in by
default and removed only by the release package built for v1 (a dedicated
build flag set in one place, by the release task), so it cannot be dropped
by accident during development, and developers and AI agents cannot
delete real data on their own machines with a work-in-progress build.

## Privileges

- diskgc refuses to run as root (uid 0) unless `--allow-root` is given;
  the laptop service runs as the owner, CI as the runner user.
- Docker requires the `docker` group (root-equivalent; Docker's model, not
  ours). Nix GC goes through the Nix daemon as a normal user.
- With `--allow-root`: an `owners:` list is required, command actions are
  refused, and the config file and every directory above it must be owned
  by root and not writable by others (like sshd's StrictModes).

## Deadlines

`--timeout` and per-tier `timeout`s are **cooperative**: diskgc checks the
deadline between operations (each walk step, each unlink, each child wait)
and, at the deadline, starts no new work and terminates a running child
(SIGTERM, SIGKILL after 10 s). A single filesystem operation that blocks in
the kernel (a hung network mount, a disk in error recovery) cannot be
interrupted by a signal and can overrun the deadline; diskgc lists the
overrun in the report's `overruns` when it returns. Network filesystems are never candidates, which
removes the commonest cause. A supervised worker that the caller could
abandon while it stays blocked is not part of v1: it would have to keep the
host lock held while abandoned, which moves the problem rather than solving
it.
