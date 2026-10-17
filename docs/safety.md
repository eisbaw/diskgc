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
be using `in_use`, i.e. it is treated as using everything it might. A
process is gone only when the kernel says so through a pidfd for it: the
pidfd cannot be opened because no task is left, or polling it reports a
hang-up (the pid has no task left; Linux 6.9 and later), or polling it
reports readable and a read of `/proc/<pid>/status` made after that poll
shows the process as a zombie (`State: Z`) that is the only task of its
group (`Threads: 1`). Readable alone is not enough: a poll racing a
non-leader exec can find the old leader, a zombie the exec has just made a
non-leader, and report readable while the process lives on. The status read
is sound because it shows one task: its state, then, under the lock that
guards every change of the thread count, the count of the group's tasks not
yet released (0 once that task itself was released), so `Z` with 1 means no
task of the group is live, and a group whose only task is a zombie never
runs again. That holds on weakly ordered CPUs (arm64) too: the state is
read before that lock is taken, and the lock's release orders it, so the
zombie state seen held at the latest while the count was 1 (a task that has
exited never runs again, also while a tracer reaps it). The old leader of an
exec shows `Z` only while the exec'ing thread is still counted (2 or more).
The poll only decides whether that read is made: the read alone shows that
no task of the process is live, given a `/proc` of diskgc's own pid
namespace and that the number still names the process. On Linux 5.8 to 6.8 polling never reports a
hang-up; a process released during a scan then cannot be confirmed and is
unreadable, never gone. No `/proc` listing and no `ENOENT` decides it: a
lookup can race an exec, `hidepid` can hide a live process (a hidden zombie
is unreadable, not gone), and an exec can briefly show the old leader as a
zombie. A process that cannot be read for any of these reasons is
unreadable, never gone. diskgc refuses to start when `/proc` is not that of
its own pid namespace, or when `pidfd_open` is unavailable (blocked by
seccomp or a sandbox), since without it no process could ever be confirmed
gone. The namespace check reads the `NSpid` line of `/proc/self/status`,
which lists diskgc's pid in every namespace from the one of the mounted
`/proc` down to its own: it must hold exactly one pid, diskgc's own. More
than one means the `/proc` of an ancestor namespace, even when diskgc has
the same number in both (then `/proc/self` still names its own pid);
every pid listed there would name another process, or none, when the
pidfd is opened by that number in diskgc's namespace, so a live process
could be taken for gone. A missing `/proc/self/status` (diskgc not visible
in that `/proc`) or a missing `NSpid` line is refused too, so diskgc does
not run on kernels built without `CONFIG_PID_NS`, which print no `NSpid`
line. The check is made once, at startup, on a `/proc` directory that
diskgc opens then and keeps open; every later `/proc` read goes through
that open directory, never through the path `/proc`, so all of them reach
the mount that was checked for as long as diskgc runs (a mount made on
`/proc` afterwards, such as an ancestor namespace's procfs exposed by
unmounting it, is never read).
**Threads:** every task of a process is read (`/proc/<pid>/task/<tid>`),
since a thread can hold files after the main thread exited and can have
its own fd table or working directory. Reading goes in rounds, each one
listing, the reads of its tasks, and a decision. The task list is not a
snapshot: a listing stops early when a task it holds is released, even one
it never showed, and a listing resumed part-way can skip entries. So each
round lists the tasks, reads the kernel's thread count (`Threads:` in
`/proc/<pid>/status`, which changes together with the thread list), and
lists them again; the listing is complete when every task of the first
listing is in the second one and their number equals the count (not zero),
and then it holds every thread alive at the count (assuming no tid is
reused within the round). A round is the answer when its listing is
complete, every task in it gave a trustworthy read, and one task is live;
otherwise another round follows, for a bounded number of rounds, after
which the process is treated as unreadable. Only the answering round's
reads are used; within it, results of tasks that exit meanwhile are kept. A
task that is exiting (`PF_EXITING`) shows empty or missing entries while it
releases them: it is never the source of the memory map or environment.
A tid can also change hands: when a non-leader thread execs, it takes over
the leader's tid once the old leader is a zombie, with the old leader's
start time, so `stat` cannot tell the two apart; the exec'ing thread's old
tid then shows the old leader, as a zombie (`Z`) for a moment (normally far
below a microsecond) and dead (`X`) until it is released, and a lookup
under `/proc/<pid>` made meanwhile can fail (`ENOENT`) or stay bound to the
old leader until its release. Hence the leader's tid is read in every
round, never carried over, and read once more at the end of a round in
which another task was seen ended (`Z`, `X` or vanished), since the
leader's tid may then name the survivor, whose private fd table or working
directory would otherwise be missed. Each entry of a task is read in full
from one task and one memory map: the environment in one read call, the
memory map in two passes, each read to the end and confirmed still present,
keeping the lines of both (a mapping moved below the read position during
one pass is found by the other), the fd table in one directory read; a
read that cannot be completed that way counts as finding nothing. A read
that finds nothing from a live task is never taken as the answer: an empty
memory map, environment or fd table of a live process counts as no
trustworthy read, so a process whose environment or fd table really is
empty (`env -i`, all fds closed) is treated as unreadable while it runs,
which costs candidates. A live task that cannot be read is treated like an
unreadable process, and so is a process that keeps creating and ending
threads faster than a round completes (each round needs its listing to
hold still from the first listing to the second); while that lasts it
costs every candidate it could be using. Accepted limits (each needs an
event outside diskgc's control at an unlucky moment, often several at
once; none of them makes diskgc take a live process for gone):

1. a thread started after a round's count is not read (also when it then
   execs, and whatever it opens), the same race as a file opened after the
   scan;
2. a mapping moved below the read position in step with both passes (two
   `mremap` calls, one during each pass) is missed;
3. a privileged reader can, in a coincidence of several events (an exec
   right after the first directory read of the exec'ing thread's fd table,
   a table larger than one read, and that entry's own lookups bound to the
   leader's tid), take a prefix of that table for the whole;
4. the pidfd pins a process, not its number, and every read under
   `/proc/<pid>` goes by number: when a listed process is released and
   another one gets its number before a later read of the scan, the
   newcomer may be missed (reported as not using files it uses) until the
   next scan lists it;
5. the check and the deletion are not atomic: a process that starts using
   a candidate after its in-use check (including a thread that acquires
   files after a round's count) is not seen. The check is kept fresh (see
   Freshness below), not atomic with the deletion.

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
   where mounts are stacked, the topmost one: the one no other mount at that
   point is stacked on; if that is not exactly one mount, `unsafe_path`).
2. Open it with `O_PATH | O_DIRECTORY | O_NOFOLLOW` and
   `RESOLVE_NO_SYMLINKS`, and confirm with `statx` that it is that mount:
   `STATX_ATTR_MOUNT_ROOT` set and `stx_mnt_id` equal to the mountinfo id.
   Anything else is `unsafe_path`: a mount point shadowed by a later mount
   over one of its parents, or a mount table that changed between reading
   and opening (refused, not retried).
3. Resolve the rest relative to it with
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`; if the
   path is the mount point itself, the rest is `.`. Without `O_NOFOLLOW`,
   so a symlink as the last component is refused (`ELOOP`) rather than
   opened. The opened object's `stx_mnt_id` must equal the mount's: a
   second line behind `RESOLVE_NO_XDEV`.

A configured path with `.` or `..` segments is `unsafe_path` before
anything is opened. A `/home -> /var/home` symlink on the root filesystem
fails in step 3 (`ELOOP`) and becomes `unsafe_path`. This refuses symlinks
and any mount crossing below the root, including a bind mount of the same
filesystem, which a device comparison would miss. A path that lies *on* a
bind mount resolves on that mount (step 1 picks it), like any other mount.
(`statx` on the full path is not used to find the mount: it follows
symlinks in the middle of the path.)

The walk and the deletion descend with
`openat2(... RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS)`; `statx` results are
trusted only when the kernel says it answered them: each field only when
its bit is set in `stx_mask`, each attribute only when its bit is set in
`stx_attributes_mask`. A check the kernel cannot answer skips the candidate
(`unsupported_fs`).

Minimum kernel: Linux 5.8 (`STATX_MNT_ID` and `STATX_ATTR_MOUNT_ROOT`;
`openat2` is 5.6); older kernels exit 2 at startup. Every command checks it
except `--version`, `--help` and `check`, which touch nothing on the
machine. The check reads `major.minor` of the `uname` release, so a vendor
kernel numbered below 5.8 is refused even if it has these features
backported, and an unparseable release is refused too.

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
  `flock(LOCK_UN)`, which would release it for every inherited copy, and
  never calls `flock` again on a lock it holds (converting shared to
  exclusive is not atomic: a failed conversion drops the lock). Limits:
  best-effort for command actions (tools such as `sudo` or `ssh` close
  inherited fds), and work inside daemons (a Nix GC, a Docker prune) keeps
  running after its client dies; overlapping prunes waste work but do no
  harm. (`PR_SET_PDEATHSIG` was rejected: it fires on parent *thread* death
  and does not reach grandchildren.)
- Waiting for the lock (until the deadline) retries a non-blocking
  `flock` every 10 to 100 ms rather than blocking in the kernel, so the
  deadline holds without signals. Limits: after the holder releases, the
  waiter can take up to 100 ms to notice, and waiters are not served in
  order: one that retries at the wrong moments can lose to later arrivals
  until its deadline.

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

How it holds:

- **Where:** `LinuxHost`'s `Delete` methods (`unlinkat` so far) ask the
  gate before anything else; a refusal is a host error of its own kind
  (`Cause::SandboxGate`: no errno, the detail names the marker that is
  missing or wrong) and nothing is attempted. `ModelHost` has no gate: it
  deletes nothing real and is test-only.
- **Structure:** the one deleting syscall wrapper in `diskgc-linux` takes a
  permit that only the gate can make. A source-scanning test
  (`diskgc-linux/src/source_scan.rs`) reads every production crate (every
  crate in `crates/` except the test-only `diskgc-model` and
  `diskgc-modelgen`, which may never be production dependencies):
  - outside an allowlisted wrapper, none may delete or rename a name, open
    for writing or truncating (also `File::create`, `File::create_new`,
    `File::options`, `OpenOptions`, `fs::write`, `fs::copy`, `set_len`,
    flags built from numbers), punch holes, start a child process
    (`process::Command`, `fork`, `exec*`), change namespace or root, use
    `libc`, call `syscall`, or `include!` code. Each wrapper is marked
    gated (it must take the permit) or ungated with its reason; adding one
    is a review point;
  - the core and the CLI may not reach the OS at all (`std::fs`,
    `std::os::unix::fs`, `rustix`, `libc`; of `std::process` only
    `ExitCode`): everything goes through the `Host` port;
  - no import renames, glob imports or `#[path]` modules, so names cannot
    be hidden from the scan; no build script and no target outside `src/`;
    no symlinks under `src/`;
  - the dependencies and features of each production crate are a reviewed
    list in the test (editing it is a review point), and nothing overrides
    a dependency's source (`[patch]`, `[replace]`).

  It walks each `src/` tree, skips only test modules (any other
  `#[cfg(test)]` fails it), and refuses source it cannot read with
  confidence (block comments, raw strings). `diskgc-e2e` is not scanned: it
  is test infrastructure outside the product build that deletes only
  inside test containers, behind its own interlock
  ([testing.md](testing.md#safety-interlock-for-destructive-tests)).
  Child processes are of two kinds ([architecture.md](architecture.md#the-host-port)):
  read-only queries, ungated, and deleting runs (prune, GC, every command
  action), gated; each has one allowlisted wrapper.
- **Once per process:** the gate decides at the first deleting call and
  keeps the answer, a refusal included; the markers do not change while
  diskgc runs. This assumes diskgc never changes its mount namespace or
  root (`setns`, `unshare`, `chroot`, `pivot_root`); the scan forbids those
  calls, so adding one fails it.
- **The flag** is the cargo feature `release-without-sandbox-gate` of
  `diskgc-linux`. A test fails if it is a default feature, or if any Cargo
  manifest, `flake.nix` or the justfile names it or uses `--all-features`;
  the release task changes that test when it sets the flag.
- **Limits:** the gate is an accident guard, not a security boundary.
  - A user who can create both markers (root on the host) defeats it.
  - It authenticates the image, not isolation: the marked test image run
    with a writable bind mount of host data (by hand, e.g.
    `podman run -v ~/src:/src ...`, or by a harness change) passes the gate
    and can delete host data through that mount. The runner mounts only the
    binaries under test, read-only.
  - It cannot see a feature enabled from outside the repository (a
    `cargo --features` typed by hand).
  - The scan is a tripwire against ordinary mistakes, not a proof. It does
    not see: a deleting or overwriting function of an allowlisted
    dependency under a name it does not list (rustix has many); flags or
    syscalls reached in other numeric ways (`from_raw`, a transmute);
    deliberate obfuscation (a macro that builds a name, an `unsafe` cast);
    code shapes the text scan does not model.
  - The shipped binary contains no deleting code yet, so no test inspects
    the binary itself; the configuration is what is checked.

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
