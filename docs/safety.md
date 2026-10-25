# diskgc safety: what is checked before anything is deleted

Normative for every deletion protocol. The guarantees in the PRD's "What bad
looks like" are implemented here; where a guarantee has limits, they are
stated here, once.

## Principles

- **Safety is code, always on.** No profile, tier or flag switches these
  checks off; the one exception is that a profile may let its own
  candidates hold version-control checkouts
  ([No source checkout](#the-eligibility-walk-mandatory-per-candidate)).
  A wrong profile can at worst select the wrong *unused, own* files,
  never files in use as far as diskgc can see.
- **Positive only.** Nothing is a candidate unless a source profile found it
  ([reference.md](reference.md#profiles)).
- **Checked at deletion time.** The index only orders work
  ([architecture.md](architecture.md)); every check below runs on the
  current filesystem state immediately before acting, on the path as it is
  then (a directory on the way renamed since it was reached is `changed`).
  What changes after the last check and during the act is each protocol's
  stated window (Deletion: accepted limits, check to delete).
- **Report time formatting.** The core uses jiff only to format the host's
  supplied wall-clock timestamp as UTC in JSON. It does not ask jiff to
  read a timezone database or the filesystem; deletion still goes through
  the checked host port and sandbox gate.
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
  touched, and a name a pattern matches is never `statx`ed when it is a
  mount point, so a dead mount cannot hang discovery or the judgement of a
  candidate (a container test mounts a FUSE filesystem whose server never
  answers, task-18). That covers those two only: the in-use scan can still
  hang on a process that uses a dead mount (In use, limits), and a mount
  made after the table was read is a limit of the deletion (Deletion). For
  an `fs` candidate the mount table is read again when it is reached for
  its tally and its deletion, and once more under its lock (Deletion). The
  shared eligibility walk also reads it before statting the candidate or
  any descendant; this protects nested mounts in Cargo profile entries.
  `statx` uses `AT_NO_AUTOMOUNT` for a candidate entry, so merely checking
  an autofs trigger at that entry does not ask the kernel to mount it.
  Path traversal and a pre-existing mounted network path can still block
  in the kernel; the flag cannot interrupt a lookup already in progress.
- **No network filesystems.** A candidate's filesystem is never one that
  another host may be using (`unsupported_fs`): a root on one is refused
  by its `mountinfo` line before it is opened, and discovery never crosses
  into one below a root (it is a mount point there, `contains_mount`). The
  type in `mountinfo` decides (`may_be_networked` in the engine), and it
  fails closed for FUSE:
  - network, cluster and VM-shared types are named (`NETWORK_FS_TYPES`):
    NFS, CIFS/SMB, Ceph, AFS, 9p, Lustre, GPFS, OCFS2, GFS2, BeeGFS,
    virtiofs, VirtualBox and VMware shared folders, and the FUSE clients of
    network storage by their `fuse.<name>` (`sshfs`, `ceph-fuse`,
    `glusterfs`, `rclone`, `s3fs`, `gcsfuse`, `moosefs`/`mfs`, `lizardfs`,
    `juicefs`, ...), so the reason is clear;
  - **every FUSE filesystem** (`fuse`, `fuseblk`, and either prefix with a
    subtype) is refused: a FUSE server can store its files anywhere, and
    its type names its program, not where the data is. This includes
    `fuse.fuse-overlayfs`: its backing directories can be on network
    storage.
  - **an untriggered automount point** (`autofs`: systemd's
    `x-systemd.automount`, the automounter daemon) is refused too, fail
    closed (TASK-180): a path lookup through it mounts what it stands for,
    most often NFS, and with that server down the lookup hangs in the
    kernel before any deadline can act; mountinfo does not say what it
    would mount. A root at or below one is refused without being looked
    up, so it is never mounted by diskgc. Once something has triggered
    it, the mounted filesystem is stacked on it and is the one judged: an
    NFS share is refused as NFS, a local filesystem (an automounted
    `/boot`) is used. Below a root, the walk never triggers one: it
    descends with `RESOLVE_NO_XDEV`, which the kernel checks before it
    would mount, and a mount point below a root is `contains_mount`.
    Limit: not triggering is not never blocking; a lookup can still wait
    on a mount that another process triggered and whose server is dead
    (the In use and Deletion limits on dead mounts).
  - Limit: a non-FUSE type that is neither on the list nor local (a new
    kernel network filesystem) is taken as local; the list is extended
    when one is seen.
- Entries of systemd's `PrivateTmp` (`systemd-private-*`) are never
  candidates and never entered: a name a pattern matches with that prefix
  is passed over, and a root inside one is refused before it is opened, as
  a misconfiguration (`unsafe_path`, an error that excludes the tier): a
  name on its path has the prefix, or its mount's root in its filesystem
  (the `mountinfo` root field) does: a bind mount of such a directory
  elsewhere. Limit: a copy of one, or one renamed to another name, is not
  recognised.

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
- **No special content** (`contains_special`):
  - no `systemd-private-*` entry anywhere in a plain-delete candidate,
    even when every entry belongs to the caller; its name is refused before
    it is `statx`ed;
  - no entry anywhere in the candidate, the candidate itself included, has
    a Cargo lock file name: any name matching `.cargo-*lock*`, the known
    ones (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`, the
    allowlist of `cargo-target` below) and unknown ones alike, of any
    type, whatever directory holds it and whether or not a `CACHEDIR.TAG`
    is near: no Cargo lock file is ever deleted, also when the candidate is
    a profile directory, lies inside a target, or is an entry deep inside
    one (a hypothetical finer-grained lock below `build/<package>/<hash>/`:
    no measured Cargo makes one, and a future lock named outside this
    pattern would not be recognised). This name rule is diskgc's fixed
    **structural recogniser** for Cargo targets: every profile directory a
    Cargo has built in holds a lock file, so any target inside a candidate
    refuses it. A name that only resembles one (`.cargo-ok`, `cargo-lock`)
    is a plain file. Refusing a non-Cargo file that has such a name only
    keeps its candidate. (A socket or FIFO of such a name is refused
    earlier, as `contains_ipc`, when the profile blocks IPC);
  - no device node and no object of a type Linux does not define: never
    deleted.

  The recogniser is code, independent of the profile library, so adding or
  editing a profile never changes what a plain `delete` of an enclosing
  directory may remove. Content that a `command` profile would clean (e.g. a
  Go cache inside a deleted `/tmp` directory) is *not* protected by
  nesting; protect it with `protect:` or `!profile:` if it matters.
- **No source checkout** (`contains_checkout`), unless the candidate's
  source profile says `checkouts: allow` (below); of every kind and
  action, plain `delete`, command actions (Mark, 2026-10-18:
  over-protecting trusted code is the safe default) and the Cargo kind
  alike. No entry anywhere in
  the candidate, the candidate itself included, is named `.git`, `.hg` or
  `.svn`, whatever its type. That covers Git's directory and the `.git`
  *file* of a worktree, a submodule or a `--separate-git-dir` checkout,
  Mercurial's and Subversion's directories, and a symlink of one of those
  names (never followed: it counts as a checkout, which errs on the side
  of keeping). A checkout may hold uncommitted work, which no profile can
  tell from cache: task-68's dry run of a marker-file profile
  (`cmake-build`) found, as its largest candidate, an in-source build in a
  Git checkout with uncommitted changes. Decided by Mark, 2026-10-18
  (task-71, `.hg` and `.svn` the same day): it only adds safety. A
  candidate inside a checkout's working tree (a project's `build/`
  directory) is not affected: only what lies in the candidate counts,
  plus the path rule below. The cost is a name compare per entry the walk
  already reads; no extra system call. For the Cargo kind, "the
  candidate" is each entry of a target's profile directories (what it
  deletes, below): the path rule runs from the root down to each profile
  directory, its own name included, so the target's own name, a target
  triple's and the profile directory's count too (`.git/debug`,
  `target/.git`, `target/.git/debug`), whatever the profile directory
  holds (an empty one there refuses its target too); each entry's own
  name and what it holds are the walk's.
  - **Inside a checkout's metadata, by path** (Mark, 2026-10-18): a
    candidate whose path has a name `.git`, `.hg` or `.svn` between its
    profile root and itself is `contains_checkout` too: a broad pattern
    under a project root finding `proj/.git/objects` or `proj/.hg/store`,
    whose own trees hold no marker; deleting part of a repository's
    metadata can destroy it. A path-only check of the names strictly
    between the root and the candidate (`checkout_metadata_below` in the
    engine): no system call, fail closed (a candidate whose path is not
    below its root is refused as `unsafe_path`, whatever the setting).
    The root's own path is not checked: a
    root that a tier, `--roots` or a profile names inside a checkout's
    metadata is taken as meant. It follows the same setting as the walk's
    check: one switch turns both off.
  - **On by default; a profile may turn it off** (Mark, 2026-10-18,
    task-80; there is no special case for any profile in code). A source
    profile, built-in or user, says `checkouts: allow` to let its
    candidates hold checkouts; `checkouts: refuse` is the default, so a
    profile that does not say anything refuses (fail closed), and the
    loader rejects any other value, an empty one (`checkouts:`, `~`)
    included. A config can use a narrow `profile_policies:` entry to set
    `checkouts: allow | refuse` for a named built-in source without changing
    its matcher or action. An omitted entry preserves the built-in value;
    `cargo-target` explicitly allows checkouts. Nothing else turns the guard
    off: no tier, `protect` entry, `!profile:` or flag. The config loader
    warns once per operator-authored checkout allowance in `check` and
    `ensure`; the built-in Cargo default is documented here. A dry run's
    plan entries carry the setting (`checkouts` in the JSON,
    [reference.md](reference.md#output)). **The risk:** a broad user
    pattern with `checkouts: allow` (`paths: ["*"]` under a project root)
    can delete a repository with uncommitted or unpushed work, or part of
    a repository's metadata. Built-in profiles turn it off only where the
    tool's own output legitimately contains repositories; user profiles
    should do the same. Which built-ins allow checkouts, and why, is in
    each one's row of
    [reference.md](reference.md#built-in-profile-library). The setting
    applies to every kind: a user profile of kind `cargo-target`
    that does not say `allow` refuses a target holding a checkout.
    **Overlap:** where one candidate is found by several source profiles
    of one tier (a union, `a || b`), it may hold a checkout only if every
    source term that admits it after its filters allows checkouts: `refuse`
    wins among admitted terms. Filesystem delete and command unions
    inspect every admitted refusing source's candidate view, including
    different names for one hard-linked inode, during planning, tally, and
    reclaim. A supported same-source Cargo union reduces to one idle
    bound; other union forms remain unsupported. Each tier judges its own candidates by its source
    settings. When protected matches are enumerated, and which enumeration
    each check uses, is under Not protected, not excluded (below); it is not
    an atomic cross-tree exclusion.
  - **Kept too (the accepted cost):** whatever holds a clone is never
    deleted by a profile that refuses checkouts (the default), and that
    can be much of what a profile finds. Seen on the development machine:
    agents' scratch clones in `/tmp` (review and fix checkouts), which
    held more than all the eligible `/tmp` entries together in the part
    of `/tmp` a dry run judged; CMake build
    directories that fetch dependencies (FetchContent's
    `_deps/<name>-src/.git`, CPM's `.cpm-cache/<name>/<hash>/.git`).
    Inferred, not measured: Cargo's git checkouts
    (`~/.cargo/git/checkouts/`, clones with a `.git` directory); temporary
    clones of pip and npm building a VCS dependency.
  - **Not recognised (limits):** bare repositories (no entry of those
    names: a `foo.git/` holding `HEAD`, `objects/` and `refs/`), so Go's
    module cache (`pkg/mod/cache/vcs/<hash>/`, bare repositories, seen on
    the development machine) stays deletable, and so does a user's bare
    repository holding unpushed commits; other version-control systems,
    for example `.bzr`, `_darcs`, `CVS`, Sapling's `.sl`, `.pijul`,
    Fossil's `.fslckout` and `_FOSSIL_`, and a Jujutsu `.jj` without a
    colocated `.git`; a name of another case (`.GIT`) on a
    case-insensitive filesystem.
- **No sockets or FIFOs** inside (`contains_ipc`): those belong to running
  programs (agents, X11, tmux, ssh). Which ones count is a policy with two
  values: *any* socket or FIFO keeps its candidate (the default: the walk
  stops at the first), or only a *live* one does: a FIFO a visible process
  holds open (by device and inode, as the in-use check sees it), a socket
  bound in diskgc's network namespace. Under the second, the walk records
  every socket and FIFO and the in-use check, which runs after the walk,
  decides with the same scan; a stale one (a test fixture's FIFO nothing
  holds open) is deleted with its candidate, and one whose liveness cannot
  be told counts as live. A FIFO is live when a visible process has it open
  (`in_use`). A socket is live when the kernel's socket diagnostics
  (`NETLINK_SOCK_DIAG` with `UDIAG_SHOW_VFS`, read-only) report a socket
  bound to that file, matched by the low 32 bits of its inode on any device
  (`contains_ipc`): the kernel's inode field is 32 bits, and its device is
  the superblock's, which `statx` does not show on a btrfs subvolume or an
  overlay of several filesystems; a coincidence counts as bound. One dump
  is not a snapshot (the kernel resumes it by position in a hash bucket, so
  one socket closed meanwhile can make it skip a stable one, and it does not
  say so), so a scan takes two complete dumps and uses their union: missing
  a socket that stays bound takes such a close during each, two timed
  events (an accepted limit). A dump that fails, is cut short or holds a
  truncated datagram makes every socket live. Sockets bound in another
  network namespace are not seen, so a socket file used only from another
  network namespace looks stale and is deleted. Only the default is reachable until
  the configuration schema defines the setting.
- **Not protected, not excluded:** no overlap, by name or by identity (a
  same-device bind mount), with a `protect` match or a negated profile's
  ([reference.md](reference.md#protect)). `protect` matches are enumerated
  before the first deletion of the run and again before each later tier;
  a tier's negated profiles at its start. An incomplete enumeration stops
  the run (`protect`) or excludes the tier (negated). Each candidate is
  judged before its lock, under it before the walk, and after the walk
  immediately before the deletion (a Cargo target: before each part).
  Which enumeration a check uses depends on the action (decided 2026-10-25
  for TASK-200 on Mark's delegation):
  - **`delete` and the Cargo protocol** judge against the tier's
    enumeration. Their deletion unlinks only entries the walk listed, so a
    match made inside the candidate after the walk is never unlinked (its
    directory fails `ENOTEMPTY`). One made inside it between the
    enumeration and the walk is in the walk and changes a time the walk
    reads: creating or renaming an entry into a directory moves the
    directory's mtime and ctime (POSIX), and the walk reads every
    directory's times again once it has listed it (the stat that found it,
    made while its parent was listed, can be older than such a change; a
    moved entry's own ctime is not relied on: vfat and some FUSE servers
    keep it). When the walk's newest mtime or ctime (for a Cargo target,
    its last build: the profile directories, lock files and walked entries)
    is unknown, or no more than 3 s older than the start of the
    enumeration, or later, the check after the walk enumerates every match
    again, as before a command. The 3 s cover the kernel's coarse timestamp
    clock (a tick behind) on top of filesystems with whole- or two-second
    times. A wall clock that moved more than 3 s away from the monotonic one
    (including suspend) since the enumeration makes every time doubtful:
    the check enumerates again. Before TASK-200 every check enumerated every
    protected and negated tree again, three full enumerations per
    candidate: with a 61,000-directory protect, 2.2 to 3.3 s per candidate
    and clears expiring before their deletion (`in_use_unknown`), against
    5 to 33 ms without the protect (TASK-184).
  - **A command action** enumerates every match again under the lock and
    immediately before its program runs: the program may act on anything
    in the candidate, walked or not.

  Limits (delete actions): a directory on the candidate's path, outside
  it, that becomes a match after the enumeration (a new `CACHEDIR.TAG`
  making an ancestor a Cargo target) is not seen until the next tier or
  run; a walked directory inside the candidate that becomes a match only
  by a marker made after the walk (a `CACHEDIR.TAG` or `.cargo-lock`
  making it a Cargo target) loses its walked entries, where the per-check
  enumeration saw such a marker until the final check (the window grew
  from the final check to the unlink to the walk to the unlink, which
  includes the in-use scan); a bind mount made after the enumeration that
  shows part of the candidate under a protected match changes no time
  inside it and is not seen; a match made inside a candidate whose times
  a wall-clock step set back past the enumeration is not seen when the
  step was undone before the check. A match made in the candidate after
  its walk is never unlinked, but its directory then fails `ENOTEMPTY`:
  an error of that candidate in the report, where the per-check
  enumeration could skip it `protected`. For every action, a separately
  writable protected tree can change between the last enumeration and
  the deletion. None of this is an atomic cross-tree exclusion.

- **Fail closed on anything unreadable**, never treating it as empty or as
  nothing to check. A listing that fails part-way is that error, never a
  shorter listing. By the error of opening or listing a directory, or of
  `statx`ing an entry:
  - `EACCES`, `EPERM` (e.g. the user's own mode-`000` directory): the
    candidate is skipped as `unreadable`, the path and errno in the
    message;
  - `ENOENT` (the object vanished while the walk ran, a directory removed
    after it was opened included): a concurrent change, `changed`;
  - anything else (`EMFILE`, `ENFILE`, `ENOMEM`, `EIO`, ...): it says
    nothing about this candidate and would hit the next one too, so it is
    an error of the run (reported, exit 3 if the goal is not met), never a
    skip: a low fd limit or a failing disk fails loudly instead of quietly
    skipping every remaining candidate. The in-use scan follows the same
    rule for running out of descriptors ([In use](#in-use), limit 4).

  A field the kernel does not report (type, inode, owner, mount id) is
  `unsupported_fs`; an entry replaced while the walk runs is `changed`; a
  mount inside (an entry on another mount, or `EXDEV` descending) is
  `contains_mount`.
- **Bounded:** a candidate of more entries than a fixed budget
  (`DEFAULT_MAX_ENTRIES` in the engine) is skipped (`too_many_entries`) as
  soon as a listing would exceed what is left of that budget, so neither
  the record nor a listing of a directory that someone fills faster than it
  is read can exhaust memory.

The walk stops at the first refusal, in walk order (each directory's
names sorted), so a candidate reports one reason even when others also
hold; which one is reported never changes whether it is deleted.

The walk records every entry's identity, (device, inode), owner, type and
birth time (where the filesystem reports one), for the in-use check and
for the identity checks of the delete. Its cost is
proportional to the candidate's size: the index saves discovery and size
tallying, not this walk.

**Shape of the walk and the delete.** Both are loops over an explicit stack
of directories, never recursion, and use single names below an open
directory only, so a tree deeper than `PATH_MAX` is walked and deleted like
any other. Each directory's names are sorted, so the order of operations is
the same on every filesystem and run. Only a small fixed window of
directories below the candidate holds an fd at a time (`FD_WINDOW` in the
engine; one is closed before the next opens), plus the candidate itself,
its parent and the one fd a listing opens for itself; a directory whose fd
was closed is reopened when it is needed again, from its nearest open
ancestor one name at a time (`openat2`, see Path traversal), each one on
the way matched against its recorded identity and preceded by a deadline
check, never through `..`. Climbing back up a chain deeper than the window
reopens up to its depth minus the window for every window's worth of
levels: quadratic in the depth of a single chain, nothing for trees
shallower than the window.

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
  at `exec`, so a variable changed later by `export` is invisible. A value
  counts component by component when it is a `:` list (`PATH`), each
  absolute component compared with the candidate's path as written and with
  `.` and `..` resolved lexically; relative values, symlinked paths and
  paths of other mount namespaces are not recognised. A process whose
  environment cannot be read while its other entries can (`Failed`: an
  empty environment, as after `env -i`, which cannot be told apart from a
  read racing an exec) is read again like any unreadable process; if it
  stays so, the heuristic has no evidence from it and it is noted, but it
  does not block: its fds, mappings and directories still count, an empty
  environment names nothing, and a read caught by an exec clears on the
  re-reads.

The last case is a real safety boundary for temporary directories. A live,
idle shell can export `TMPDIR=/tmp/tmp.X` after exec while holding no path in
that directory open. The process scan then judges `/tmp/tmp.X` eligible. A
disposable-container test reproduces this with a live process and the real
binary's dry-run; it never performs the unsafe deletion. `mktemp-dir`,
`nix-shell-tmpdir` and the broad `tmp-entry` are therefore not built-ins.
The same test reproduces the false negative for a live
`/tmp/claude-{uid}/nix-shell.*` TMPDIR; the frozen prefix preset only
selects named entries inside that directory, never the outer shell itself.
The withdrawn `containers-image-tmp` source had a fixed three-day idle floor,
but that floor cannot prove a producer has finished with its spool. A
disposable VM test gives an old `/var/tmp/buildah*` directory genuine old
kernel ctime, then parks a live producer after it closes the spool. Its
matching set and floor (now a user profile) judge the directory eligible
while the producer still
needs it on resume. The test only dry-runs that live directory. The
producer is a fixture; whether real Buildah or Podman ever reuses a dormant
spool with this lifetime remains unverified. No liveness contract for such
reuse is available, so the profile is withdrawn from the built-in library
(Mark, 2026-10-23); re-admission needs the real lifecycle verified in a
container test (TASK-125).
An idle-time filter cannot establish liveness. The rc names
`nix-shell-<pid>-*` and `nix-develop-<pid>-*` also cannot use a host-only
`/proc/<pid>` check: a shell can share `/tmp` from a different PID namespace
and put its namespace-local PID in the name. `nix-shell-rc` therefore
ships only with Owner PID, which reads every `NSpid` level.
[Producer liveness](#producer-liveness) designs the evidence these profiles
need (Owner PID across PID namespaces) and records which of them have none.

Or it is a directory locked with `flock` by someone else (`locked`), the
convention systemd-tmpfiles also honours: diskgc opens the candidate
directory below its parent and takes an exclusive `flock` without waiting
(Deletion, step 3); a lock held by anyone else, shared or exclusive, makes
that fail. The walk then runs under the lock and must find the locked
directory (same device and inode): a directory replaced between the open and
the walk is `changed`. Only directories are locked; a candidate that is a
single file is not.

**Browser caches need an application lock check too.** A running Chromium
family browser may hold no file descriptor in its disk cache; its live
cache can therefore pass the general in-use scan. An `fs` source for such
a cache uses `where: {app_not_running: .config/chromium/SingletonLock}`
with each source root set to a home directory (an absolute lock path also
works).
The symlink's relative text is `hostname-pid`; diskgc neither follows nor
interprets it.
Any present lock retains the cache, including one naming a PID that appears
gone here. A browser in another PID namespace can share this home directory
while its lock names only the PID inside that namespace; looking up that PID
in our `/proc` can falsely report that the browser exited. Malformed and
unreadable locks, foreign hosts and config directories that cannot be reached
safely also retain the cache. Only an absent lock in a safely reached config
directory permits it. The check runs during planning and tallying, again
under the candidate lock, and immediately before deletion or a command
action after the process scan. A browser started after the last check can
still race deletion; Chromium's symlink is not a lock diskgc can acquire to
exclude that startup.

**Every process, and only once settled.** The scan works in rounds: it
lists the processes, then reads every one it has not read yet and every
one that is due to be read again (below), and lists them again, until a
listing calls for no read. A process that forks a child (which inherits
its fds, working directory and mappings) and then closes a file before
diskgc reads it shows nothing of that file, but the child was born before
that read, so it is in a later listing and is read there; re-reads, and the
status reads that can find a process gone, are made before a listing too,
never after the last one. Every read only adds: a use any read of a process
found counts, also when a later read of it fails or finds it gone. A pid
found gone and listed again was reused (or is a zombie not yet reaped): it
is read again as a new process, once per scan. After 16
listings that each brought new pids the scan is **unsettled** (a process
it did not read could have inherited anything); listings made only for
re-reads do not count (they are bounded by the re-reads each process
gets). The checker waits 20 ms and makes one complete new scan before
judging a candidate. If that scan is unsettled too, whether the candidate
is in use cannot be told: it is skipped `in_use_unknown`, not `in_use`
(TASK-190, decided 2026-10-25 on Mark's delegation: `in_use` would give a
harness evidence of a use that was never seen), and it counts toward the
tier's three-in-a-row limit (Freshness, below), so a sustained fork storm
costs at most three candidates' scans per tier, not two per candidate.
The next candidate starts a new scan immediately. Other blocking scans can
be reused for less than half a second from completion. A machine that
keeps starting processes faster than the scan reads them costs reclaim
that way. diskgc's own process is never counted: it holds the candidate
open and locked while it checks it.

**The run's deadline and signals** are checked by the scan before every
listing and after every pause (the re-reads', the 20 ms before the retry
of an unsettled scan, Owner PID's re-reads): up to 16 growing listings
and the reads they call for, twice, can take seconds on a busy host. A
scan that reaches the deadline stops the run (`deadline`); one that sees
SIGINT or SIGTERM stops it (`interrupted`). Neither judges the candidate
(TASK-189). What a scan cannot interrupt is one batch of reads (every
process new in one listing), a few hundred milliseconds on a host of
thousands of processes, and a read blocked in the kernel (A process using
a dead mount, below).

**Unreadable or vanishing processes:** a process that cannot be read is
read again, each process on its own schedule: 2 ms after its first read, 8
ms after its second, 30 ms after its third (an exiting process is gone by
then, one caught in an exec readable), and so is a readable one whose
environment could not be read. What stays unreadable is judged by what
every process shows of itself: `/proc/<pid>/status` (its `Uid:` real,
effective, saved and filesystem uid, `Gid:`, `CapPrm:`, `CapEff:`), its
`uid_map` (absent on a kernel without user namespaces), and the owner of
the `fd` directory of a live task of it (`/proc/<pid>/task/<tid>/fd`, for
a task whose `stat` shows it neither ended nor exiting before and after),
which is the effective uid while the process is dumpable and root while it
is not. The leader's own entries do
not tell: `/proc/<pid>` always shows the effective uid, and a leader that
called `pthread_exit` has no memory map and shows root on some kernels
while its live threads are dumpable. *The user* means the users whose
files candidates may hold: diskgc's own uid, or the `owners:` under
`--allow-root`; a process is the user's when any of its four uids is.
In root mode the scanner also includes uid 0 processes: root may hold an
owner's file even though root-owned candidate entries remain forbidden by
the eligibility walk.
Then (user decision 2026-10-17: read literally, the earlier rule, every
unreadable same-user process makes every candidate `in_use`, blocks all
reclaim on any desktop):

- another user's process (refused, `EACCES` or `EPERM`, and none of its
  uids is the user's) is invisible (Visibility, below) and never blocks;
  it is the only one not read again;
- a process the kernel refuses is noted and does not block when its
  status explains the refusal as a privilege boundary on **every** read of
  it in the scan (its status is read with each of its failed reads); one
  read without that explanation and it blocks, so that one event (an exec
  swapping a dead leader and its survivor under one tid can make one read
  show a false non-dumpable owner) is not enough. A process refused gets
  four status reads of its own (reads that failed only for its
  environment do not use them up). The privilege boundaries are:
  - **setuid**: its effective uid is not the user's (`sudo -i`,
    `fusermount3`);
  - **more capable**: it holds a permitted capability that diskgc's
    *effective* set lacks, and both diskgc's `uid_map` and its own read as
    the full identity map (`0 0 4294967295`), taken as both being in the
    initial user namespace: the kernel shows a process of the reader's
    user namespace only to a reader whose effective set includes its
    permitted set (`systemd --user` holds `CAP_WAKE_ALARM`). Equal map
    text alone proves nothing (two sibling namespaces can both show
    `1000 1000 1`), and the namespace itself cannot be read without the
    access that was refused. So when diskgc runs in another user namespace
    (a rootless container), or a map does not exist (no user namespaces in
    the kernel) or cannot be read (noted with its reason), this excuse
    never applies. Limit: a child namespace with the full identity map, which
    only real root can make, is taken as the initial one. A process in a
    user namespace diskgc's user created (`unshare -r`) is readable
    whatever it holds there, so a refusal of it is something else;
  - **non-dumpable**: every one of its four uids is the user's and the
    live task's `fd` directory is owned by another uid than its effective
    one (root, or the overflow uid 65534 where root is not mapped into
    diskgc's namespace) (`ssh-agent`, `gpg-agent`, `(sd-pam)`); when no
    live task can be found, it is not excused.

  One exception to "every read": when the last read finds it another
  user's (refused, none of its uids the user's: a `sudo` or `su` child
  after its `setresuid`), that stands, as for any other user's process.
  Limit: a privileged process that switches all its uids to another user
  while keeping the user's files open is then not seen;
- any other unreadable process of the user makes every candidate
  `in_use_unknown` (it could be using any of them, and whether it does
  cannot be told; the skip names it, evidence `unreadable`), counted
  toward the three-in-a-row limit like an unsettled scan (TASK-190): one
  whose reads fail for another reason (also another user's: a failed
  read says nothing about who may read it), one whose status cannot be
  read, and one refused although it is dumpable, unprivileged and the
  user's (a shell started with `newgrp` or `sg`: the kernel also compares
  groups).

The cost of the noted ones is stated plainly: their open files, mappings
and working directories are not seen, so a candidate one of them uses can
be deleted under it. A process in the middle of an exec is briefly
non-dumpable or refused, which is why every refused process of the user is
read again before it is excused. A process is gone only when the kernel
says so through a pidfd for it: the
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
namespace and that the number still names the process. On Linux 5.8 to 6.8
polling never reports a hang-up (measured on 5.8.16, 5.15, 6.1, 6.6 and
6.8.12 guests, with 6.12 reporting it: docs/testing.md). When it reports
readable, diskgc checks
the held pidfd with `pidfd_send_signal(..., 0)`: `ESRCH` confirms release;
success or `EPERM` does not. Signal 0 delivers no signal. A seccomp filter
that returns `EPERM` also leaves this check inconclusive; an unreadable
process then remains blocking. Other syscall errors fail the read. No `/proc`
listing and no `ENOENT` decides it: a
lookup can race an exec, `hidepid` can hide a live process (a hidden zombie
is unreadable, not gone), and an exec can briefly show the old leader as a
zombie. A process that cannot be read for any of these reasons is
unreadable, never gone. diskgc refuses to start when the held `/proc`
directory is not a procfs mount (`fstatfs`), when `/proc` is not that of
its own pid namespace, or when `pidfd_open` is unavailable (blocked by
seccomp or a sandbox), since without it no process could ever be confirmed
gone. Podman 2.1.1's built-in seccomp profile (NixOS 20.09) does this:
inside its containers `pidfd_open` fails with `EPERM` and diskgc refuses
to start; other runtimes and versions are not checked yet (TASK-126). The namespace check reads the `NSpid` line of `/proc/self/status`,
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
unmounting it, is never read). A disposable-container regression covers a
tmpfs mount over `/proc` after startup, followed by lazy detachment of that
overmount and of a private procfs mount held by the host: its readers keep
working even after the held mount is detached. It does not construct an
ancestor PID namespace procfs underneath the mount, so that exact namespace
transition is not exercised by the test.
**Threads:** every task of a process is read (`/proc/<pid>/task/<tid>`),
since a thread can hold files after the main thread exited and can have
its own fd table or working directory. Reading goes in rounds, each one
listing, the reads of its tasks, and a decision. The task list is not a
snapshot: a listing stops early when a task it holds is released, even one
it never showed, and a listing resumed part-way can skip entries. So each
round lists the tasks, reads the kernel's thread count (`Threads:` in
`/proc/<pid>/status`, which changes together with the thread list), and
lists them again. The listing is complete only when the first listing's
numeric tids all appear in the second listing, their number equals the
nonzero count, and each task directory pinned before the count still has a
task and has the same procfs inode as a fresh lookup of that numeric tid
after the second listing. It then holds every thread alive at the count.
A tid reused between the listings invalidates the round even if both
lists contain the same number. A round is the answer when its listing is
complete, every task in it gave a trustworthy read, and one task is live.
Otherwise another round follows, for a bounded number of rounds, after
which the process is treated as unreadable. Only the answering round's
reads are used; within it, results of tasks that exit meanwhile are kept. A
task that is exiting (`PF_EXITING`) shows empty or missing entries while it
releases them: it is never the source of the memory map or environment.
A task read is bracketed by two `stat` reads through the same held `stat`
file of the task. Its procfs inode holds the kernel `struct pid`, so a
recycled numeric tid cannot redirect the second read to another task: once
the task is released, the held file reads `ESRCH`. A different start time or
a vanished held task discards that round. The start time's clock-tick
resolution is no longer the sole identity check.
**Shared fd tables** (threads made with `CLONE_FILES`, the usual case) are
read once per round, not once per task: after one task's fd table was read
in the round (a trusted read, from a task live after it), another task of
the round counts as read by that read when, in this order, its own `stat`
shows it live, the kernel says the two hold the same table
(`kcmp(KCMP_FILES)`), the first task's `stat` shows it still live with its
start time, and the task's own `stat` again shows it live with its start
time. That read then stands for this task's table: a task only ever
leaves a table it shares (`unshare(CLONE_FILES)` and `execve` give it a
copy), never joins an existing one, and the first task was the same task
from its read to the comparison. So the table they share is either the one
that was read, which this task held then too or joined only as a task
created since (limit 1), or a copy the first task made after its read,
which only a task created since can share (limit 1); what was opened in it
after the read is limit 5. An exec kills every other task before it swaps
tids, so a task compared across it is not live at its second `stat`.
Anything else, `kcmp` refused (no `CONFIG_KCMP`, or a
seccomp filter such as a container runtime's default profile without
`CAP_SYS_PTRACE`) included, reads the task's table in full, and the
leader's extra read after a task was seen ended is always a full read. A
desktop's threads share some 10^2 tables; reading every task's copy made
the scan exceed Freshness below on a busy host (TASK-115).
The `cwd`, `root` and `exe` links of a task are read together, in one round
and one bracket. The memory map and the environment, which every task of a
process shares, are read from the first task of a plain listing whose read
is trusted and live after it: such a round needs no counted or confirmed
listing, so those readers do not pin every task of the process.
A task directory pinned in an unsuccessful round stays pinned for that
process read. If its tid is recycled during a later round, that read may
reach the 16-round bound and treat the process as unreadable; this can
delay reclaim under heavy thread churn.
The listing temporarily holds one directory fd per listed thread; the
task reads also retain one fd (its `stat` file) per encountered tid until
that process read ends. A highly threaded process can therefore reach
`RLIMIT_NOFILE`; that is an error of the run (limit 4), not an unreadable
process.
The fd-directory owner used to classify an unreadable process is accepted
only when the task is live and the same held `stat` file returns the
same start time before and after that owner read. A vanished held task is
passed over, even if its numeric tid was reused within one clock tick.
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
costs every candidate it could be using. A 30-second, eight-worker fork
storm in a disposable container produced 261 unsettled checker samples;
all 261 blocked the unused fixture and explicitly reached the 16-growth-listing
limit. A scan after the storm cleared the fixture.
This measures conservative behavior on one 7.2.9 kernel under synthetic
load. A sustained busy host can still make no progress until its process
list settles. Accepted limits (each needs an
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
4. a scan now holds one pidfd for each listed process incarnation and checks
   whether it was released after its readers and before accepting the final
   listing. A replacement with the same numeric PID is read again. The
   `/proc/<pid>` paths used by the readers still go by number, so a read
   crossing a replacement can include the newcomer's evidence; this is
   conservative for a found use. If a pin cannot be opened or checked, or
   incarnations keep changing, the scan is unsettled and blocks deletion.
   **File descriptors.** The pins make a scan hold about one descriptor
   per visible process for its whole length, plus the transient fds of
   the reads in flight (measured: 2,402 at peak on a host of ~2,250
   processes). The binary raises its soft `RLIMIT_NOFILE` to the hard
   limit at startup ([reference.md](reference.md#ensure)), since the
   stock soft limit of 1024 is too low on a busy host; the hard limit is
   never raised, and children get the soft limit diskgc was started with.
   Running out anyway (`EMFILE`, or `ENFILE` for the system's table) in
   any read of the scan (a pin, a `/proc` reader, a status read, a
   `uid_map`, diskgc's own status, a lookup in a process's root, Owner
   PID's namespace and `NSpid` reads and its pidfd probe, the pytest
   contract's listing of the session's directory, the socket query) says
   nothing about the process being read and would hit the next one too,
   so it is an error of the run (exit 3 if the goal is not met), never an
   unsettled scan, an unreadable process, an unknown privilege excuse, an
   unknown owner (`producer_unknown`) or an unknown socket: the same rule
   as the walk's ([Fail closed on anything unreadable](#the-eligibility-walk-mandatory-per-candidate)).
   The readers report it as its own kind of failure, which every caller
   must handle (TASK-185).
   A host with more processes than the hard limit therefore reclaims
   nothing through `fs` and Cargo tiers, loudly, until the limit is
   raised. A process that appears after the final identity check belongs
   to the check-to-delete race in item 5;
5. the check and the deletion are not atomic: a process that starts using
   a candidate after its in-use check (including a thread that acquires
   files after a round's count) is not seen. The check is kept fresh (see
   Freshness below), not atomic with the deletion;
6. a file handed to a process that was already read (an fd passed over a
   Unix socket, `SCM_RIGHTS`) is a new use of the same kind, and an fd in
   flight in a socket's queue belongs to no process at all: neither is
   seen;
7. `kcmp(KCMP_FILES)` compares the two tasks' fd-table pointers without a
   lock that orders the two loads, so in principle two tasks that both
   leave a shared table (`unshare(CLONE_FILES)`) at that instant, with the
   freed table's memory reused for the other's new one between the loads,
   compare equal while holding different tables; the second task's table
   is then not read (Threads, shared fd tables).

**A process using a dead mount:** the scan `statx`es every visible
process's fds, working directory and root through `/proc` (above). For
FUSE, these calls use `AT_STATX_DONT_SYNC | AT_NO_AUTOMOUNT` so a stalled
server does not hold the in-use scan while its cached attributes are
available. A disposable-container test holds an fd and cwd in a mount,
stops its server, and checks that ensure exits within its deadline
without deleting the held candidate. This does not help NFS: lookup
through a `/proc` magic link can revalidate the dentry
(`d_weak_revalidate`) and wait for the server. A process using a dead
NFS mount remains a possible hang, never a deletion; the deadline
cannot interrupt that kernel wait (Deadlines).

**Overlay and btrfs:** `maps` can report a different (device, inode) than
`stat` on overlayfs and btrfs subvolumes (the layer's or volume's device,
and on older kernels the layer's inode). For a candidate on one of them, a
mapping therefore also matches an object by inode number alone, on any
device: more matches, the conservative side. Fds, working directories and
executables are statted through their links and match exactly. Linux 7.2
reports the overlay's own pair in `maps` (the container test logs it); the
inode-only match is exercised by the model only. Limit: where overlayfs
composes its inode numbers from the layer's (`xino`) and the kernel shows
the layer's inode in `maps`, a mapping alone does not match.

**Freshness:** the `/proc` scan used for a candidate is never older than one
second (monotonic clock including suspend time) when that candidate's
deletion starts. A scan is as old as its first read. For a `delete` of
`fs` candidates, one scan serves every candidate whose deletion starts
while it is younger than that bound, and a candidate that would start on
a scan one second old or older gets a new one; within one Cargo target
under its locks, the same holds for its parts (`cargo-target`, Reclaim
protocol, step 3). Before a command action, whose program may act on
anything in the candidate, a scan serves only the candidates checked
strictly before 500 ms from its start. The `fs` rule was decided
2026-10-25 for TASK-203 on Mark's delegation ("one scan serves
consecutive fs candidates while each delete starts on a scan younger than
FRESHNESS; never start a delete on an older scan"): on the development
laptop (1609 processes, load 17-19) a scan took 336 ms (median, glibc) to
514 ms (static build) and 1050 ms (p95), and a pytest session's walk and
delete 100 to 300 ms, so the 500 ms window made every candidate scan
again: about 0.5 to 1 s per candidate, one to two hours for its 6400
sessions (TASK-184). Nothing the bound protects changes: every deletion
still starts on a scan younger than one second, against which its own
in-use check, liveness contracts and the other final checks run right
before it, and a stale scan still clears nothing. What changes is the
margin of limit 5: a process that starts using a candidate is certain to
be seen only if it started at least one second before that candidate's
deletion starts, where the shorter window made that typically half a
second. The check that
clears a candidate looks at the clock as its last step before the
deletion starts, and the deletion (or command) looks again as its own
last step. A scan kept from an earlier check that reaches the bound
during a check, or between its clear and the deletion, has reached the
end of its reuse: the check is made again on a new scan without spending
an attempt (below); only a scan made for the check spends one. A stale scan there (a suspend during the scan or right after
it, a scan slower than the bound on a busy host) never clears anything:
it spends one of the candidate's three attempts, and the next attempt
waits (250 ms, then 1 s; never to or past the deadline) and checks against
a new scan. When the attempts are spent the candidate is skipped
`in_use_unknown` and the run goes on with the next one (a Cargo target's
parts share the target's three attempts, so its locks are not held for
more); its exit code
follows available space like any other skip (decided 2026-10-25 for
TASK-144 on Mark's delegation: "never delete on a stale scan; retry with
backoff a few times, then skip that candidate and carry on"). A part-way
Cargo target stops at the part that hit it, as for `in_use`. A signal
during a retry pause ends it within 50 ms and stops the run. After three
candidates in a row of one tier (in the order taken) end `in_use_unknown`
(a stale scan, an unsettled scan, or a process that cannot be inspected),
the host is taken to be too busy for this run: the tier's remaining
candidates are skipped `in_use_unknown` without a scan, and any other
outcome resets the count. A later tier starts again from zero. Refusing
needs no freshness. Index blocks
([architecture.md](architecture.md#memory-budget-and-collapse)) only
group candidates for scheduling; each member is found again when its
block is expanded and gets its own check under this bound, which a scan
kept as above may answer: a use that starts after that scan is limit 5
above.

**Visibility:** without root, other users' `/proc/<pid>/fd` cannot be read,
so their use of *your* files is not seen; nor is the use of the user's own
processes noted above as unreadable (setuid, more capable, non-dumpable).
Fine for one-user machines and runners; weaker on shared hosts.

### Producer liveness

Status (TASK-112): Owner PID and the `nix-shell` and `pytest` contracts
are implemented; the `nix-shell-rc` and `pytest-tmp` built-ins use them,
and any user profile can require them. The project-dependency contracts
(`node-project`, `python-project`) are designed, not implemented, and so
are the parts of A process's paths only they use (the climbs of held
directories and the translation of held files): the loader refuses those
contracts until they land (TASK-79). The protocols below are normative
for the implementation.

The in-use scan answers "does a visible process use this candidate now?".
It cannot answer "has the program that made this candidate finished with
it?". A producer can be alive and hold nothing diskgc can see: a pytest
session between two tests, a shell that exported `TMPDIR` after `exec`, an
image copy between writing a blob and committing it. Age cannot answer it
either: `idle_for` measures changes, and a dormant producer changes
nothing; read-mostly trees such as `node_modules` change nothing even
while in use. This section defines the evidence a source can require for
that second question, how far each kind of evidence can be trusted, and
the rule that decides which built-ins may ship. The schema is in
[reference.md](reference.md#liveness-contract-predicate); which
built-in has which contract is in
[reference.md](reference.md#liveness-contracts).

Terms. The **producer** is the process whose life bounds the use of a
candidate: it made the candidate and may come back to it. A **consumer** is
any process that reads or writes it. A **contract** is the evidence a
source requires before its candidate may be judged unused, together with
the producer versions and configurations it was verified for. A contract
permits reclaim; it does not prove that no process will ever use the
candidate again, and its class says how much it does prove.

**Kinds of evidence.**

| Evidence | What it can show | Trustworthy when | Examples |
|---|---|---|---|
| Kernel use: fd, cwd, root, mapping (In use) | in use | always, for processes visible at scan time; silent on dormant ones | every candidate |
| A path in a process's initial environment (In use) | in use | heuristic: an `export` after `exec` is invisible | `TMPDIR` of a running child |
| A `flock` on the candidate directory (Deletion, step 3) | in use | always; its absence says nothing about a producer that never locks | the systemd-tmpfiles convention |
| Producer lock: a lock the producer holds for as long as it uses the candidate | in use, or gone | the producer takes it for its whole use, in every supported configuration | Cargo's `.cargo-lock` files (kind `cargo-target`) |
| Presence marker: a file the producer creates at start and removes when it ends cleanly | present: maybe in use; absent: no evidence of use | absent means gone only if every supported configuration creates it; a crash leaves it present | Chromium's `SingletonLock`; pytest's session `.lock` (never with `--basetemp`, not when its retention keeps 0) |
| Owner PID: a pid in the candidate's name or in a marker | no process carries that pid, or one may | diskgc can see every process of this kernel (Owner PID, below); a pid match is a number match, not proof of identity | `nix-develop-<pid>-<n>`; the text of pytest's `.lock` |
| Producer retention rule: what the producer's own cleanup removes | reclaimable, relative to the producer | only relative: no safer than the producer's own cleanup | pytest's cleanup of sessions beyond its newest three |
| Temp-root users: no process could be using the temporary root the candidate lies in | reclaimable | heuristic: a producer that changed its temporary root inside the process is not seen | pytest sessions in a finished shell's `TMPDIR` |
| Project activity: no change, read or visible use anywhere in the owning project for a long time | reclaimable | heuristic: a project used without any change, read-time update or visible process is not seen | `node_modules`, `.venv` |
| Consumer tolerance: every consumer treats a missing entry as a cache miss | no liveness needed | verified per tool | `__pycache__`, `node-compile-cache` |
| A tool's own cleaner, or a code-backed kind | delegated | per tool or kind, when diskgc can invoke it on the measured directory | `pip cache purge`; the Cargo lock protocol; Nix GC |
| Nothing: a dormant consumer that keeps only a path string | nothing | never | a shell's own `TMPDIR`; `mktemp -d` directories |

**Contract classes and the admission rule.** Every built-in source has one
contract class, recorded in the
[liveness table](reference.md#liveness-contracts):

- **sound**: when the contract permits reclaim, the producer can no longer
  use the candidate, given preconditions diskgc checks on every run and
  fails closed on. Owner PID, a producer lock, and a presence marker that
  every supported configuration creates are of this class.
- **producer-equivalent**: diskgc removes only what the producer's own
  cleanup, in its default configuration, removes from the same evidence; it
  may act sooner than the producer would. The claim is relative: no worse
  than running the producer.
- **verified heuristic**: the verdict can be wrong in named residual cases.
  Admissible only when each residual is named in the liveness table; the
  loss in each is rebuildable data or one failed, retryable producer run,
  never a user's own work (dated exceptions: Mark 2026-10-23,
  hand-edited files in a dependency directory, under the project
  contracts below; mark-emulator for Mark 2026-10-24, files placed by hand
  in a build directory, under the anchored build profiles, Source-anchored
  CMake output; mark-emulator for Mark 2026-10-24, for `nix-user-cache` and
  `go-mod-cache` only, a cached copy that its upstream no longer serves,
  admitted with that residual named in their rows); every known counterexample has a checked-in
  ModelHost vector and, where the kernel's view matters, a container test
  with the real pinned producer; and Mark's decision to admit it is recorded
  in the task and in the table. A decision made on his delegation is
  recorded as "admitted by mark-emulator for Mark, DATE", and Mark may veto
  it (TASK-122). Two points are open, not decided here: whether such an
  admission needs Mark's own ratification before its source enters a
  `releases:` catalog (none of the twelve admitted on 2026-10-24 is in
  one), and the container tests of those twelve, each row's "Container
  test: pending (TASK-137)".
- **tolerant** and **delegated**: as in the table above, verified per tool.
- **none**: no contract. The profile is not admissible as a built-in, and
  its row in the liveness table records the counterexample. A user profile
  may still select such paths; its author owns that risk.

The rule applies to every built-in source, including those shipped in the
development library before it existed (Mark, 2026-10-23). TASK-122
classified them in the liveness table; each built-in source's class is also
data, its `liveness_class` in builtin.yaml, and a release catalog
(`releases:` there) holds only built-ins whose row is Shipped, which the
loader (no `releases` on class *none*) and a sync test enforce. Before any
release, a built-in of class *none* is either given a contract or
withdrawn; until Mark decides, it stays in the development library as a
proposed withdrawal.

A contract only narrows: the eligibility walk, ownership, the in-use scan,
the directory `flock` and the checkout guard all still apply. A contract
is verified against a pinned producer version, recorded in
`verified_with`. That field is metadata: diskgc does not check at run time
which version produced a candidate. The container tests run the version
pinned in the flake, so a bump of that pin that changes the lifecycle
fails them; a different version on a user's machine is covered only by
the documented contract.

`app_not_running` (above) is a presence-marker contract that refuses on any
present lock. It does not use Owner PID: Chromium writes the host name as
well as the pid, and a home directory can be shared between hosts.

**Owner PID.** The engine behind every PID-based verdict. It reverses
the in-use scan's reading of the process list: there a missing process
only means "not seen using it"; here a missing pid is taken as evidence
that the recorded process has ended. A match is only a number match: a
reused pid looks like the owner, and that only retains. The engine may
answer *no process carries the pid* only where the list is provably
complete; every precondition below fails closed (`producer_unknown`).

1. *The number.* From the candidate's own name (a protocol fixes where in
   the name) or from a protocol's marker file (pytest's `.lock`). It is
   decimal ASCII, no sign, no leading zero, from 1 to 4194304 (Linux's
   `PID_MAX_LIMIT`). A marker is a regular file directly in the checked
   candidate, opened below its fd without following symlinks, at most 16
   bytes, holding the number and at most one trailing newline. A name
   without a valid pid is not an instance of the contract (`filtered`); a
   malformed marker is `producer_unknown`.
2. *Precondition: one kernel.* The candidate's filesystem type (`fstatfs`
   on its fd) is one of tmpfs, ext4, xfs, btrfs, bcachefs and f2fs; every
   other type is `producer_unknown` (virtiofs, 9p, overlay, ZFS, FUSE and
   any type not yet verified). Deployment assumption, stated in the
   release notes: a filesystem of a listed type that holds candidates is
   written only by processes of this kernel. Exporting it to other hosts
   (an NFS or Samba server) or handing its directory to a VM breaks the
   assumption, because a pid written there names a process of another
   kernel. diskgc cannot detect an export; the assumption is the
   operator's.
3. *Precondition: diskgc runs in the initial PID namespace.* At startup
   diskgc checks that `self/ns/pid`, read through its held `/proc`, is on
   nsfs (`fstatfs` gives `NSFS_MAGIC`) and has the kernel's fixed inode of
   the initial PID namespace (`PROC_PID_INIT_INO`, `0xEFFFFFFC`,
   4026531836; observed on Linux 7.2.9 on the host, and a different inode
   inside `unshare --pid`). Every PID namespace descends from the initial
   one, and startup already requires the held `/proc` to be diskgc's own
   namespace's, so that `/proc` can list every process of every namespace.
   In any other PID namespace (a container, a sandbox) a sibling
   namespace's processes are invisible.
4. *Precondition: no hidden processes.* The held `/proc` is mounted without
   `hidepid` (its `mountinfo` super options), whatever diskgc's
   privileges. With `hidepid=2` or `invisible`, the kernel hides every
   process diskgc may not ptrace-read, the user's own included: a
   non-dumpable process, or a shell started with `newgrp` or `sg` and every
   child of it (a `nix-shell`, a `pytest`), since the kernel also compares
   groups. Such a producer would never be listed. Root does not lift the
   precondition: whether its capabilities reach a process depends on that
   process's user namespace, which diskgc cannot read for a hidden
   process.
5. *Lookup.* The settled in-use scan also reads the `NSpid:` line of each
   listed process's `/proc/<pid>/status`, readable for every user's
   process without `hidepid`: its pid in every namespace from diskgc's
   down to its own. The pid *may be in use* when the number appears at
   **any** level of any listed process's `NSpid`, of any user, zombies
   included. Any level, not only the innermost that the producer's
   `getpid()` returns: a supervisor may record a pid as its own namespace
   sees it, and a coincidence only retains. When no level matches, diskgc
   also opens a pidfd for the number in its own namespace: only `ESRCH`
   lets the answer stand, as In use requires the kernel's word for "gone".
   A number that is a live thread's id but no thread-group leader's is
   not listed (`/proc` lists leaders), and `pidfd_open` refuses it with
   an error that is not `ESRCH` (`ENOENT` observed on Linux 7.2.9):
   `producer_unknown`. The candidate is kept, as for a
   reused pid, under the less precise reason (TASK-131).
   *No process carries the pid* then holds when the scan settled and every
   listed process, of any user, gave a readable `NSpid` line. A process
   whose status cannot be read goes through In use's re-reads and pidfd
   classification: one confirmed gone drops out, so processes exiting
   during the scan do not block; any process that stays unreadable,
   whatever its user, makes every PID answer of that scan
   `producer_unknown`. Its user is no excuse: a process can create a
   directory and then change its uids without changing its pid, and under
   `--allow-root` candidates belong to several owners.
6. *PID reuse* makes an ended owner look present: the candidate is
   retained, never deleted. No start-time comparison is made; it could
   only turn retained candidates eligible, and it would depend on the wall
   clock. With `pid_max` at 4194304 and a thousand processes, about one
   ended owner in four thousand is retained per scan; small
   namespace-local pids (a container's 9) collide more often.
7. *Stability.* Once the preconditions hold, an ended process never comes
   back, and a later process with the same number is not it. Unlike
   `app_not_running`, where a browser can start after the check, the
   answer cannot turn between the check and the deletion; a candidate
   replaced under the same name is caught by the identity checks of
   Deletion.
8. *When.* During planning and tallying, again under the candidate lock,
   and immediately before deletion or a command action after the process
   scan, from the same scan as the in-use check, under its Freshness bound.
   The index may order by an earlier verdict; it never decides.
9. *Scope.* It shows that the recorded process ended, not that every
   consumer did. The in-use scan still runs, but a consumer that outlives
   the owner and keeps only a path string (a `setsid` or `nohup` child, an
   orphaned background job) is not seen. Each protocol says whether its
   candidates have such consumers.

A pid that may be in use is reported `in_use` with evidence `owner_pid`;
the reported pid is the matching process's pid in diskgc's namespace (the
first `NSpid` level), whichever level matched. An answer that cannot be
reached is `producer_unknown`, its detail naming the failed rule (when
both `hidepid` and the namespace fail, `hidepid`); it
carries a `process_blockers` entry (evidence `unreadable`) only when one
process caused it ([reference.md](reference.md#liveness-contract-predicate)).

**A process's paths.** The pytest and project contracts ask whether a
process uses a path inside a *sought tree* (a pytest session, a project).
The kernel renders the link text of `/proc/<pid>/{cwd,root,fd/*}` and the
paths in `/proc/<pid>/maps` with `d_path` relative to the *reader's*
root, not the target's (a process in a `chroot` or another mount
namespace shows host paths to diskgc). Only the environment and the
command line are strings in the process's own view. Nothing outside the
sought tree is walked or resolved by name, except the temp-directory and
`--basetemp` values pytest itself resolves (below); the `..` climbs of
held directories read only the device and inode of ancestors, and a held
file's link text is translated through mount tables, not resolved. The
pytest contract's sought tree is the session, the candidate itself, whose
held objects In use already matches by device and inode (a directory of
the session bound elsewhere included, since every directory of the walk is
matched); of the rules below it uses the process set, the strings and
the values pytest resolves (with a climb from a resolved `--basetemp`
value). The climbs of held directories and the translation of held files
serve only the project contracts and land with them (TASK-79): they are
not implemented, and nothing implemented relies on them (TASK-112 step 5,
2026-10-24). So:

- *The process set* of a contract is the processes with any of their four
  uids equal to the owner of the directory the contract judges (pytest
  itself refuses a `pytest-of-<user>` not owned by its uid), plus every
  process whose uids cannot be read. Other users' processes cannot use
  that directory's producer state and are not consulted, so a root
  daemon's unreadable environment blocks nothing. A magic link or entry
  that cannot be opened or read is treated exactly as In use treats it:
  excused at a privilege boundary (a non-dumpable `ssh-agent` or
  `gpg-agent` of the user), re-read, or, if it stays unreadable without
  that excuse, the answer of that scan is `producer_unknown` with
  evidence `unreadable`.
- *The tree's devices and directories.* The walks of the sought tree
  record every directory's device and inode, and the set of devices seen.
  Mounts whose mount point lies under the tree (from `mountinfo`) add
  their devices; their content is not walked.
- *Directories a process holds* (its `cwd`, its `root`, an fd on a
  directory) are opened through the magic link, which reaches the object
  whatever the text says, and climbed with `..` by fd. Each step's device
  and inode are looked up in the set of the tree's directories, so a bind
  mount of a subdirectory of the tree is caught too. The climb ends as
  *not under* at its fixpoint: when `..` returns the same device and
  inode, every ancestor in the process's view has been visited (this
  covers `pivot_root` containers and Flatpak). Each held directory's
  ancestor chain is computed once per scan and every candidate is looked
  up in it, so the cost is one climb per held directory, not one per
  candidate. A defensive bound of 4096 levels without a fixpoint is an
  error that retains.
- *Files a process holds* (an fd on a non-directory, a mapping) are
  matched first by device and inode against the inodes the walk saw, as
  In use does (including its inode-only match for mappings on overlayfs
  and btrfs). For the path test, the object's device and mount id come
  from a `statx` of the magic link with `STATX_MNT_ID` and
  `AT_STATX_DONT_SYNC` (for a mapping, a `statx` of
  `/proc/<pid>/map_files/<range>` with `STATX_MNT_ID`; when that link
  cannot be read, the mapping has no device or mount id for the path
  test: it is matched only by In use's identity rules, including the
  inode-only rule, and otherwise is untranslatable and retains). A
  device that is not one of the tree's devices cannot be inside the tree,
  and the entry is not inside. On one of them, the link text (a
  ` (deleted)` suffix removed) is turned into the object's *path within
  its filesystem*, with no name resolved:
  - when the process's mount namespace (`/proc/<pid>/ns/mnt`) is diskgc's,
    the text is a path in diskgc's view and no root is stripped; the line
    of diskgc's own `mountinfo` whose id is the object's mount id gives
    the mount point and root field, and the same conditions as below make
    it untranslatable;
  - otherwise the text is *translated* through the process's mount table:
    the text and the `/proc/<pid>/root` link text are rendered in the same
    frame (the reader's), while the mount points in `/proc/<pid>/mountinfo`
    are relative to the process's root. So, unless the root link text is
    `/`, it is removed from the front of the text; the line of
    `/proc/<pid>/mountinfo` whose id is the object's mount id gives that
    mount's mount point and its root within the filesystem; the rest of
    the text after the mount point, appended to that root, is the path
    within the filesystem. This was probed on Linux 7.2.9 for a separate
    mount namespace with and without `chroot` and `pivot_root`, a
    container-like `tmpfs` root with the tree bound at `/work`, and a
    `chroot` inside it; in each case `STATX_MNT_ID` on the magic link
    equalled the `mountinfo` id (TASK-112 notes). A mount id missing from
    the table, an unreadable `mountinfo`, a different filesystem (its
    `major:minor`) than the tree's, or text that does not start with the
    root link text and then the mount point, cannot be translated. Mount
    points and root fields are unescaped (`\040`, `\011`, `\012`,
    `\134`) before any comparison, as Path traversal (step 1) does for a
    configured path: `mountinfo` escapes a space as `\040`, link text does
    not (probed: a directory `my proj` bound at `work dir` shows
    `my\040proj` and `work\040dir` in `mountinfo`, and `work dir/src/a.js`
    as link text).

  The tree's own path within its filesystem comes from diskgc's
  `mountinfo` the same way (its mount's root field and the tree's path
  below the mount point). Text whose path within the filesystem does not
  have the tree's as a prefix is not inside. Otherwise only the *suffix*
  is resolved, from the tree's directory fd with
  `openat2(RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS |
  RESOLVE_NO_MAGICLINKS)`, and the entry is inside when the object
  reached has the magic link's device and inode. A suffix whose
  resolution fails or reaches another object, or text that cannot be
  translated, in either namespace, is unresolvable and retains
  (`in_use`). This makes a mapping whose `map_files` link cannot be read,
  and that In use's identity rules do not match, an availability cost
  with rootless containers whose files are mapped from the tree.
- *Strings* (environment values, command-line elements) are compared as
  bytes, after lexical normalisation as Python's `os.path.abspath` does
  (`.`, `..` and repeated `/` folded; a relative string is not made
  absolute), with the path sought followed by its end or a `/`. Only the
  values pytest itself resolves are also resolved by identity:
  `TMPDIR`, `TEMP`, `TMP` and `PYTEST_DEBUG_TEMPROOT`, and `--basetemp`
  values on the command line or in `PYTEST_ADDOPTS`. Such a value, after
  normalisation, is opened in one `openat2` call from the process's root
  fd with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`, and the result
  `statx`ed with `AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC`. `RESOLVE_IN_ROOT`
  clamps `..` at the process's root, as the kernel does for the process
  itself, so no value can escape it (`/../tmp/x` is `/tmp/x`). `EAGAIN`
  (a rename raced the lookup) is retried at most three times, then the
  value is unresolvable. Nothing else is resolved: resolving arbitrary
  strings could trigger an automount or hang on a dead network mount.
- An *empty* value is ignored, as Python ignores it. A *relative* value of
  one of the variables pytest resolves is never resolved: Python makes it
  absolute against the working directory at the time it looked, and the
  process may have changed directory since. It makes the verdict
  `producer_unknown` with `process_blockers` evidence `unresolved_path`
  where the contract says so.
- *Several causes.* `process_blockers` is an array: when several
  processes each make an answer undecidable or block it, each has one
  entry, sorted by pid.
- *Hangs.* The climbs and `statx` calls inherit In use's limit for a
  process using a dead network mount: they can block in the kernel and
  overrun the deadline, never cause a deletion.

**nix-shell rc directories** (`liveness_contract: nix-shell`). The
candidate's own name is `nix-shell-<pid>-<suffix>` or
`nix-develop-<pid>-<suffix>`; Owner PID on `<pid>`; no process carries it:
reclaimable. A name of another shape is `filtered`. Facts from Nix
2.34.8 on the host (2026-10-23) and pinned by the container test
`nix_shell_rc_lifecycle_is_pinned` on the image's Nix 2.34.9 (2026-10-24),
for both commands, interactive and `-c` / `--run`:

- the pid in the name is the shell's own: Nix makes the directory and
  execs the shell, which keeps that pid;
- `nix develop`'s directory is empty once the rc script has run; no shell
  or child of it holds anything in it or names it in its environment (the
  rc file is a separate temporary file the rc script removes, and
  `TMPDIR` points at a separate `nix-shell.XXXXXX`); it is left behind
  after a normal exit, `SIGHUP` and `SIGKILL`;
- legacy `nix-shell`'s directory holds `rc` (and `env-vars` when a
  `stdenv` setup runs) and is the `TMPDIR` and `NIX_BUILD_TOP` of the
  shell and so of every child, which can write temporary files there at
  any time and names it in its initial environment; the rc script's
  `EXIT` trap removes it at a normal exit or `SIGHUP`, even while a
  `setsid` child that inherited it still runs; only a `SIGKILL` (or any
  death that skips the trap) leaves it behind.

So for legacy directories Nix does give children a reason to use the
directory after the shell's start, and every such child carries it in its
initial environment, which the in-use scan reads (`environment`); and the
producer itself deletes the directory under such a child when it exits
normally. A detached child (`setsid`, `nohup`) that outlives the shell
and uses the directory through a remembered path without it in its
initial environment (started with a cleared environment) is Owner PID's
scope limit. A shell in a private PID namespace sharing `/tmp` names its
directory after its namespace-local pid; Owner PID finds that pid at an
inner `NSpid` level and keeps the directory while the shell lives
(`nix_shell_rc_shell_in_private_pid_namespace_is_kept`, Nix 2.34.9). The bytes are small (about 350 MiB in 65,000 directories in TASK-68's dry run); the
value is the directory count, and proving Owner PID on the simplest
producer first.

**pytest sessions** (`liveness_contract: pytest`). The candidate is one
session directory (Mark, 2026-10-23, replacing the per-test-directory unit
of 2026-10-18: the liveness evidence, the lock and the number, belongs to
the session, and pytest's own cleanup removes whole sessions; with
`checkouts: allow` decided, the reason for the finer unit is gone, and a
session is about a tenth as many candidates). Facts from the pytest 9.1.1
sources pinned in the Nix test image (`_pytest/pathlib.py`,
`_pytest/tmpdir.py`), each observed by the container test
`pytest_lifecycle_is_pinned` (2026-10-24):

- Without `--basetemp`, a session directory is
  `<temp root>/pytest-of-<user>/pytest-<N>`, with `N` one more than the
  largest existing number, and `pytest-current` links to the newest. The
  temp root is `$PYTEST_DEBUG_TEMPROOT` or Python's
  `tempfile.gettempdir()` (`TMPDIR`, `TEMP`, `TMP`, then `/tmp`,
  `/var/tmp`, `/usr/tmp`, then the working directory), made absolute
  against the working directory and resolved.
- Right after creating that directory, pytest creates `.lock` in it
  holding its pid, unless `keep` is 0. `keep` is
  `tmp_path_retention_count` (default 3);
  `tmp_path_retention_policy=none` sets it to 0, and then there is no
  lock. The lock is removed when the session ends; a killed session leaves
  it.
- When a session ends, pytest deletes the sessions with `N ≤ M − keep`
  (`M` the largest number) whose `.lock` is absent or more than three days
  older than its own session. With `keep` 0 this includes its own session
  and any concurrently running session without a lock.
- `--basetemp=DIR` (on the command line, in `PYTEST_ADDOPTS`, or in a
  project's `addopts`) uses `DIR` itself: pytest empties and creates it,
  makes no number, no `pytest-current` and no `.lock`. Any path is
  accepted. A `DIR` not shaped `pytest-of-<user>/pytest-<N>` is outside
  the matching set: not a candidate, so not a hole. One shaped like a
  numbered session is treated below.
- pytest's own cleanup (`make_numbered_dir_with_cleanup` →
  `cleanup_numbered_dir` → `try_cleanup` → `ensure_deletable`) runs when a
  session ends, after that session's own directory exists, so its `M`
  counts it. It deletes every entry of `P` whose name starts with
  `pytest-` (case-insensitive) and whose suffix Python's `int()` reads as
  at most `M − keep`, unless the entry is a symlink, or its `.lock` is a
  file whose mtime is at most three days older than the new session's
  directory, or the lock cannot be examined. It does not look at the pid
  in the lock, and it does not know who made the directory: a lockless
  `--basetemp` run shaped like a session is deleted like any other.

*Terms for the verdict.* For a session `S = P/pytest-<N>`, `P` its
`pytest-of-<user>` directory, `U` the owner uid of `P`, and `R` its temp
root (the parent of `P`):

- *Session name.* `pytest-` followed by ASCII digits, no leading zero
  (except `0` itself), whose value fits in 64 bits. Anything else in the
  matching set is `filtered`.
- *`M`* is the largest value among `P`'s entries named `pytest-` followed
  by ASCII digits that fit in 64 bits, leading zeros allowed. Every such
  name is one pytest's `int()` also reads, so diskgc's `M` is never above
  pytest's. A listing of `P` that fails or is cut short is `unreadable`.
- *Below the rank*: `N ≤ M − 3`. *Above*: `N > M − 3`.
- *Dead-lock age*: three days and one minute. pytest counts a lock dead at
  three days, measured from a new session's mtime; the extra minute
  covers coarse filesystem timestamps and a new session's clock being a
  little behind diskgc's.
- *Old lock*: a `.lock` that is a regular file whose mtime is older than
  the dead-lock age. *Old session*: the newest mtime or ctime anywhere in
  `S`'s tree, as the mandatory eligibility walk measures it, is older than
  the dead-lock age. Row 7's age condition is therefore evaluated after
  the walk; the other rows can be decided before it.
- *The user's processes* and the path tests: A process's paths, above,
  with `U` as the owner.
- *A pytest is running*: one of the user's processes has an element of
  its command line containing `pytest` or `py.test`; or has the command
  line of an execnet worker bootstrap, an element `-c` followed by the
  element `import sys;exec(eval(sys.stdin.readline()))` exactly
  (`popen_bootstrapline` in execnet 2.1.2's `gateway_io.py`; its
  `popen_args` are `[python, -u, (-B,) -c, popen_bootstrapline]`), which
  pytest-xdist's workers run; they set their `PYTEST_XDIST_*` variables inside the
  process, never in their own initial environment); or has
  `PYTEST_ADDOPTS`, `PYTEST_CURRENT_TEST`, `PYTEST_VERSION` or a variable
  beginning `PYTEST_XDIST_` in its initial environment (the middle two
  are inherited by a test's subprocesses). A command line or environment
  that cannot be read counts as running.
- *Named*: one of the user's processes names `S`: a string test (above)
  finds `S`'s path in its command line or environment, or a `--basetemp`
  value resolved by identity is `S` or lies in it (found by climbing
  `..` by fd from the resolved value, as for directories). *Naming
  undecidable*: no process names `S`, but one has a relative or
  unresolvable `--basetemp` value, or an unreadable command line or
  environment.
- *Root used*: one of the user's processes could use `R` as its temp
  root: one of `TMPDIR`, `TEMP`, `TMP`, `PYTEST_DEBUG_TEMPROOT` in its
  initial environment resolves (by identity) to `R`; or `R` is `/tmp`,
  `/var/tmp` or `/usr/tmp` (Python's fallbacks, always counted, an
  over-approximation), by diskgc's path or as one of them resolves in the
  process's own root (a container whose `/tmp` is a shell's `TMPDIR`;
  step 5, 2026-10-24); or its current working directory is `R` (an extra
  that can only retain). *Root use undecidable*: none of those, but one
  of the user's processes has a relative or unresolvable value of those
  variables, a fallback whose lookup in its root failed for another
  reason than its absence (`ENOENT`, `ENOTDIR`, or `ESRCH` when the
  process's root is on procfs, checked with `fstatfs`: the root is a dead
  process's `/proc` directory, where nothing resolves for the process
  either; Chromium's sandbox chroots its renderers into one, 66 processes
  on the development host; `ESRCH` anywhere else is unresolvable), or an
  environment that cannot be read and is not excused at a privilege
  boundary (In use).
- *Every read only adds* (as in In use; step 5, 2026-10-24, after the
  step's Codex review): the facts above (uids, command line, initial
  environment, working directories and the lookups in the process's root)
  are read with each process's other entries within the in-use scan's
  rounds, never after its last listing, so a child a process forks before
  it ends is listed and read in a later round, as for its fds. A process
  that ends during its own read is gone, as In use takes it; the latest
  read that gave its facts stands, also for a process found gone later,
  but uids or a command line that two reads disagree on (a failed read
  included) are unknown, never an earlier value; so is an initial
  environment two reads disagree on, and a lookup in the process's root
  whose answer differs between reads (its mount view changed: an earlier
  match by identity is never lost to a later miss, nor the reverse) is
  unresolvable; a lookup only an earlier read made keeps its answer (step
  7, 2026-10-24, after its Codex review). A reader that fails
  leaves that fact unreadable or unresolvable (rows 8, 9), never
  harmless; a root that cannot be opened at all (a leader that
  called `pthread_exit`) is unresolvable too, never the absence of a path
  in it. (Read after the last listing, as first implemented, a process
  ending in that window either lost its evidence or made the scan
  undecidable: 7 of 30 scans on the development host.)
- When an Owner PID precondition other than the filesystem one fails (the
  PID namespace, `hidepid`), the process list is not complete: a pytest
  counts as running, and naming and root use as undecidable.
- *Before the table* (each `producer_unknown`, never eligible): `P`'s owner
  `U` must be one of the users diskgc judges for (diskgc's uid, or the
  `owners:` under `--allow-root`), since only their processes' facts are
  read; `R`, reached again from the root by path, must hold `P` (same
  device and inode), so a temp root renamed or replaced meanwhile is never
  compared with the processes' roots; and `S` must be the walked object
  (otherwise `changed`). A refusal by the kernel (`EACCES`, `EPERM`,
  `ENOENT`, `ENOTDIR`) while `S` or its `.lock` is reopened keeps the
  session (`unreadable`, or row 3 for the lock, as pytest's own cleanup
  keeps a lock it cannot examine); any other error is an error of the run.
  These checks come before the table, so their detail names no row
  (`liveness_contract pytest: ...`); every verdict of the table names its
  row (`liveness_contract pytest row N (rule): ...`).

*The verdict*, ordered: the first row that applies decides. Every session
of `P` gets exactly one row.

| # | Condition | Verdict | Rule |
|---|---|---|---|
| 1 | `S`'s name is not a session name | `filtered` | matching |
| 2 | the listing of `P` failed | `unreadable` | listing |
| 3 | `S` holds a `.lock` that is not a regular file of at most 16 bytes holding a pid (Owner PID, step 1) | `producer_unknown` | lock |
| 4 | the lock's pid may be in use | `in_use`, evidence `owner_pid` | lock |
| 5 | the lock's pid cannot be answered (an Owner PID precondition fails, a process stayed unreadable) | `producer_unknown` (evidence `unreadable` when one process caused it) | lock |
| 6 | `S` is named | `in_use`, evidence `command_line` or `environment` | named |
| 7 | below the rank, no `.lock` or an old lock, and (no pytest is running, or `S` is an old session) | eligible | rank (producer-equivalent or stricter) |
| 8 | naming undecidable | `producer_unknown`, evidence `unresolved_path` or `unreadable` | named |
| 9 | root use undecidable | `producer_unknown`, evidence `unresolved_path` or `unreadable` | unused root |
| 10 | root used | `in_use`, evidence `temp_root` | unused root |
| 11 | otherwise | eligible | unused root (heuristic) |

Rows 3 to 5 apply only when `S` holds a `.lock`; with no lock, or a lock
whose pid no process carries, the table continues at row 6. An ended lock
owner alone never permits: an xdist worker or a detached child can outlive
a killed controller and keep writing in `S`. Row 7 does not depend on the
naming or root-use tests deciding, so a session below the rank can be
reclaimed when they cannot; rows 8 to 11 are then never reached for it.
Every session that row 7 does not permit, and that rows 8 and 9 leave
undecided, is `producer_unknown`, never eligible.

*Row 7, the rank rule, is producer-equivalent or stricter.* The invariant:
**row 7 never permits a session that pytest's own next cleanup would
keep.** "Next cleanup" means: a session with the default retention
(`tmp_path_retention_count` 3) created at a time `t_new` at or after the
check, ending, and cleaning the same snapshot of `P`. Against pytest
9.1.1: that cleanup's `M` is at least diskgc's (it counts the new session,
and reads every name diskgc reads); it deletes any entry numbered at most
its `M − 3` that has no `.lock` or a lock at least three days older than
the new session, which the dead-lock age with its margin implies; it does
not look at the lock's pid or at who made the directory, so a lockless
`--basetemp` run shaped like a session is deleted too. Row 7 adds two
conditions pytest does not have: a lock that is not a regular file is
`producer_unknown` (row 3, where pytest's `is_file()` would call it
absent), and, Mark's decision of 2026-10-23, while a pytest of `U` is
running only an old session qualifies. That second condition is why the
class is "producer-equivalent or stricter" and never "the same, but
sooner while live": a lockless session below the rank (`keep` 0, or a
`--basetemp` run) is not removed while a pytest that could own it runs,
until it is older than pytest's own dead-lock age. The three-day rule
itself is pytest's; diskgc does not claim to know more than pytest about
a lock that old whose owner it cannot see. Row 11 (unused root) is not
covered by the invariant: it deliberately removes sessions above the rank
that pytest would keep.

*Stability.* Under rows 3 to 5 an ended owner stays ended, and a lock and
a session only age. Under row 7 new sessions only lower `S`'s rank; a
`keep` 0 session that deletes the newest number can raise it again, which
only retains, and the table runs again before deletion. Under rows 6 to
11 pytest never adopts an existing session; a pytest started after the
check makes a new number. A verdict can only be wrong because a process
was invisible to the check, or named `S` only in a configuration file, not
because of what happens after it.

*Startup window.* Between pytest's `mkdir` and its `.lock` (one function,
microseconds) a live session looks lockless. The built-in's `min_idle_for`
([Liveness contracts](reference.md#liveness-contracts)) covers this window.

*Residuals* (the heuristic part, admitted by Mark on 2026-10-23; the loss
in each is that run's temporary files, so the run may fail and is run
again):

- a session below the rank in a `P` whose project configures
  `tmp_path_retention_count` above 3: pytest would keep it, row 7 removes
  it. Accepted by Mark (2026-10-23); diskgc cannot read the setting;
- a live session without a lock at any rank that row 11 reaches, whose
  pytest process did not take its temp root from its initial environment:
  `TMPDIR`, `tempfile.tempdir` or `PYTEST_DEBUG_TEMPROOT` changed inside
  the process by a `conftest.py`, a plugin, or a long-lived Python program
  that then calls `pytest.main()`; or whose `--basetemp` comes from a
  configuration file (`addopts` in `pytest.ini`, `pyproject.toml`,
  `tox.ini` or `setup.cfg`); or that is not visible to diskgc or is
  excused by a privilege boundary. A configuration file would have to hard-code a
  path below a temp root that none of the user's processes uses: the
  shared roots in the matching set are in use (`/tmp`,
  `/tmp/claude-{uid}`), and the others have random names (`nix-shell.*`,
  `tmp.*`);
- a detached child of an ended session that keeps writing in `S` above the
  rank, with a temp root other than `R` in its initial environment;
- a lockless session below the rank whose live pytest has no `pytest` on
  its command line and none of the variables above (`pytest.main()`
  embedded in a Bazel `py_test`, a custom runner, a frozen binary): row 7
  does not see it running and removes the session if it is otherwise
  below the rank. This is bounded by producer-equivalence: pytest's own
  next cleanup with the default retention deletes that session too
  (orchestrator decision, 2026-10-23, within Mark's "producer-equivalent
  or stricter" ruling); diskgc may act sooner.

Not matched: `garbage-<uuid>` directories that a pytest killed during its
own cleanup leaves in `P`; a known gap of the matching set.

**Project dependency directories** (`liveness_contract: node-project`
and `python-project`). An unchanged `node_modules` or `.venv` can belong
to a project used today: a reader changes neither mtime nor ctime (TASK-79,
container probe). The evidence therefore comes from the project, not from
the dependency directory. Mark accepted this heuristic on 2026-10-23.

*Which candidates.* A candidate with an ancestor below its source root
that is itself a dependency directory (named `node_modules`, or holding
`pyvenv.cfg`) is `filtered` and never deleted: a nested
`node_modules/a/node_modules` belongs to a package, and deleting it makes
Node resolve the hoisted copy instead, possibly another version, a silent
change rather than a failed run. The candidate's parent must hold the
ecosystem's manifest as a regular file: `package.json` for `node-project`;
for `python-project` one of `pyproject.toml`, `setup.py`, `setup.cfg`,
`Pipfile` or a `requirements*.txt`, and the candidate must hold
`pyvenv.cfg`. Without one the candidate is `filtered`. These are checked
with `statx`.

*The project* is the nearest directory, from the candidate's parent up to
the source root, that is a workspace root (for `node-project`: a
`package.json` with a `workspaces` field, or a `pnpm-workspace.yaml`) or a
version-control top level (holding `.git`, `.hg` or `.svn`); when there is
none, the parent. The only file content diskgc reads for these contracts
is such a `package.json`, with `O_NOATIME`, at most 1 MiB (a larger or
unparsable one is `producer_unknown`). A workspace member's dependencies
are then judged by the whole workspace, whose activity can drive them.
The climb fails closed at a bind mount: when it reaches the root of a
mount whose `mountinfo` root field is not `/` (a subdirectory bound
elsewhere, which can hide the real workspace root above it), the answer
is `producer_unknown`.

The candidate is reclaimable when, for the term's idle bound `T` (the
source's `min_idle_for` combined with the tier's `idle_for` filters, the
longest):

1. *No change.* The newest mtime or ctime in the project tree is older
   than `T`. Both count: package managers extract files with old,
   normalised mtimes, so after an install only the ctime is fresh. The
   tree is the candidate (its eligibility walk already measures it) and
   the project walked like a candidate (fd-relative, no symlink followed,
   no mount crossed, sorted, bounded by the same entry budget), leaving
   out VCS metadata (`.git`, `.hg`, `.svn`: a fetch is not project use)
   and, for cost, other directories named like the candidate. A project
   tree over the budget is `too_many_entries`; one that cannot be read is
   `unreadable`. Failure: `not_idle`, detail naming the newest path.
2. *No read.* The newest atime of a regular file in the same trees is
   older than `T`. Under the default `relatime` the kernel updates a read
   file's atime when the old atime is at or before its mtime or ctime, or
   more than a day old, so a project whose code or dependencies were read
   in the last `T` keeps its dependencies even without a change.
   Directory atimes do not count: any lister (`git status`, an indexer,
   `rg`) refreshes them. Under `noatime` this rule says nothing. A process
   that reads every file's content (a backup) makes every project look
   used: dependencies are then kept, the cost side. Invariant: diskgc
   itself never refreshes a file's atime here; it reads content only with
   `O_NOATIME` (the files are the user's, so the flag is permitted) and
   otherwise only lists and `statx`es. Failure: `not_idle`.
3. *No visible use.* None of the user's processes (A process's paths,
   with the project directory's owner) uses the project: the union of In
   use's device-and-inode match on every inode the walks saw, and the
   path tests of A process's paths for its `cwd`, `root`, directory fds,
   file fds and mappings, and the string test for its environment values.
   The path tests cover what the walk leaves out, VCS metadata and other
   dependency directories. An unresolvable entry retains (`in_use`); a
   process that stays unreadable makes the answer `producer_unknown`,
   evidence `unreadable`. From the same scan as the in-use check.
   Failure: `in_use`.

The checkout guard keeps its default, `checkouts: refuse`: an editable
install can put a real checkout under `.venv/src`. A symlinked
`node_modules` or `.venv` is never a candidate (discovery reports no
symlink and follows none). In a pnpm layout, `node_modules` holds
symlinks and hard links into pnpm's store; the walk follows no symlink,
and removing a hard link leaves the store's copy, so only the project's
view goes.

*Floor.* The 30-day `min_idle_for` of the built-ins
([Liveness contracts](reference.md#liveness-contracts)) is chosen because
a reinstall costs minutes and a network fetch; a pause of a few weeks (a
holiday, a project parked for a sprint) is common and should not cost a
reinstall on return. A
tier may demand longer.

*Dated exception* (Mark, 2026-10-23). A dependency file edited by hand
inside `node_modules` or `.venv` is the user's own work, which a reinstall
does not recreate; the admission rule's "never a user's own work" is
waived for these two contracts for that case only. `patch-package` and
similar tools keep their patches in the project and reapply them, so that
common case is rebuildable; a manual edit is not. The operator's remedy
is a `.diskgc-keep` file in the dependency directory or a `protect` entry.

*Residuals* (loss: a reinstall from the project's manifest, which can
resolve different versions where no lockfile pins them; Mark accepted
this class of loss on 2026-10-23):

- a project used without any change, without an atime update (`noatime`)
  and without a process visible at the check: a tool installed in a
  project directory and run from a timer, a project about to be resumed
  after the floor;
- the `relatime` boundary: the last read can be up to a day older than
  the newest atime shows; with a 30-day floor this moves the boundary by
  at most a day;
- hand-edited dependency files, under the dated exception above;
- use through a path outside the project (a symlink into
  `node_modules`, `NODE_PATH`, a `.venv` activated from elsewhere) by a
  process not running at the check, and an environment value naming the
  project in another mount namespace's paths (strings are compared as
  bytes);
- a change only inside another dependency directory of the same name or
  only in VCS metadata;
- a workspace that is neither declared (`workspaces`,
  `pnpm-workspace.yaml`) nor a version-control top level: its members are
  judged by their own directories only.

*Stronger alternative (not designed further).* A cooperative protocol:
every consumer holds a shared project lock while it uses dependencies;
reclaim takes the exclusive lock and rechecks under it. It is a producer
lock in the terms above, sound for consumers that take it, and gets a
schema only with its first producer.

**With the other checks.**

- *Order.* A contract is a source predicate in two parts. Its rule on the
  candidate's name alone (no valid pid: `filtered`) runs where
  `app_not_running` runs, before the eligibility walk. Its process
  evidence (Owner PID) needs the process scan: it runs after the in-use
  check, from that same scan, in the plan and again immediately before
  deletion or a command action, under the candidate lock. In a union,
  every admitted source that found the candidate and whose contract
  recognises it must permit it
  ([reference.md](reference.md#liveness-contract-predicate)).
- *PID namespaces* are handled by the precondition and the any-level
  `NSpid` lookup. Neither the name nor the marker says which namespace the
  pid belongs to; the lookup does not need to know.
- *`hidepid`* can hide the user's own processes (precondition 4): a
  `/proc` mounted with it gives `producer_unknown` for every PID-based
  answer and for pytest's rows 8 and 9; row 7 then needs an old session.
- *Mount namespaces.* Owner PID compares numbers, not paths. Held
  directories and files are matched by identity (A process's paths); the
  temp-root values pytest uses are resolved in the process's own root, so
  a container that binds a shell's `TMPDIR` as its `/tmp` counts. Other
  strings are compared as bytes, in the process's own view.
- *Deadlines.* The reads are part of the in-use scan or the walk. A
  temp-root `statx` on a dead mount can block like the scan's other
  `statx` calls (In use, A process using a dead mount).
- *Cost.* Only when a selected source has a contract: one more read of
  each listed process's `status` (Owner PID's `NSpid`). For `pytest`,
  within the scan's reads of every process it can read (the user's; every
  process under `--allow-root`, filtered by uid afterwards): one more
  `status` read (its uids), one `cmdline` read (the leader's; through a
  live task only when that reads empty), and one `RESOLVE_IN_ROOT` lookup
  per temp-directory or `--basetemp` value it carries and per Python
  fallback root (three), each with its own pidfd and open of
  `/proc/<pid>/root` (per process and value, not shared across processes:
  two processes' roots can differ). Measured on Linux 7.2.9 with about 660
  processes, 350 of the user, load about 10 (2026-10-24, glibc release
  build, 12 interleaved one-candidate dry runs): median 1.69 s against
  1.59 s without the contract; the scan itself, about 2.1 million
  syscalls, dominates; per
  check, one listing of the session's `pytest-of-<user>` (at most 65,536
  names; more is a listing cut short, row 2), made afresh for every
  check and never reused from an earlier one (step 5, 2026-10-24: a
  reused `M` can be higher than the current one after a `keep` 0 session
  deleted the newest number, and a higher `M` lowers `S`'s rank; reusing
  it within the plan alone, which the check before deletion repeats, is a
  possible optimisation, not done), one lookup of `S`'s `.lock`
  and one safe resolution of `R`; byte comparison for every other string.
  For the project contracts: one climb per directory a process holds,
  once per scan; one `statx` per held file, and, only for files on one of
  the tree's devices, a lexical translation (one read of
  `/proc/<pid>/mountinfo` per (mount namespace, root device and inode)
  and scan) and a suffix resolution beneath the tree; and a walk of each
  project, bounded by the entry budget.

**Limits, stated plainly.**

- Outside the initial PID namespace, under `hidepid`, or on a filesystem
  not on the local list, no PID-based answer permits a deletion; those
  candidates are kept as `producer_unknown`. diskgc in a container with
  its own PID namespace reclaims no `nix-shell` rc directory and no pytest
  session that needs the process list (rows 3 to 5 with a lock, rows 8 to
  11); pytest's row 7 does not use the process list beyond "a pytest is
  running", which it then assumes, so an old lockless session below the
  rank is still reclaimed there (producer-equivalent;
  `pytest-tmp-nested-namespace`).
- The local-filesystem list rests on a deployment assumption diskgc
  cannot check: no listed filesystem holding candidates is written by
  another kernel.
- A reused pid keeps an ended owner's candidate until that pid's holder
  exits.
- Owner PID shows that the recorded process ended, not that every
  consumer did.
- A contract holds for the producer versions and configurations it was
  verified for; another version may name, lock or reuse its directories
  differently, and diskgc does not detect that at run time.
- The pytest and project residuals above.
- A process can forge a name, a `.lock` or a manifest; diskgc is not a
  defence against a hostile local user (Principles).

### Source-anchored CMake output

*Dated exception* (mark-emulator for Mark, 2026-10-24, TASK-122). A file
a user placed by hand in a build directory (notes, test data, a script) is
the user's own work, which a rebuild does not recreate; the admission
rule's "never a user's own work" is waived for the anchored build profiles
(`cmake-build-anchored`, `meson-build-anchored`,
`autotools-build-anchored`, `west-build-anchored`) for that case only. So
are configure choices kept only in the build directory (`-D` values,
Meson options, `./configure` arguments), which must be entered again. The
operator's remedy is a `.diskgc-keep` file in the build directory or a
`protect` entry. Vectors: `cmake-build-anchored-hand-placed-file-residual`,
`cmake-build-anchored-configure-choices-and-dormant-ide-residual`.

`cmake-build-anchored` accepts an output directory of any name only when
it has a regular `CMakeCache.txt`, a direct `CMakeFiles/` directory, no
entry named `CMakeLists.txt`, and the cache records exactly one
`CMAKE_HOME_DIRECTORY:INTERNAL=` path. The cache is read through a checked
fd, never through a later path lookup, and may be at most 2 MiB. The
recorded path must be normalized, lie under the bound root, and be a
strict ancestor of the candidate. This excludes in-source builds and
sources that would be deleted with the output. A safely reached regular
`CMakeLists.txt` must still exist at that source path on the candidate's
filesystem. Missing, ambiguous or unsafe evidence never makes the output
eligible. The source file's identity is recorded at discovery and checked
again during judgement. Reclaim reads the cache through the fd of the
locked candidate and compares that fd's identity with discovery; a path
swap cannot redirect the source check to a different build directory.
The candidate must also be idle for at least three days by the
ordinary filesystem age measure, pass the checkout guard, and pass every
general eligibility and in-use check.

This verifies a surviving source file, not that every external build
dependency is available. It also cannot prove that an uncooperative
process starts using the output after the in-use scan; that is the general
deletion race limit below. The profile matches at most five levels below
each supplied root. Generic Ninja, OpenWrt, Buildroot, Yocto and Bazel
markers alone do not establish a surviving source and are not shipped as
deletion sources.

`west-build-anchored` uses the same recorded CMake source validation and
three-day idle floor for Zephyr `build/` directories. It also requires a
direct `zephyr/` directory. A missing source, source outside the supplied
root, or source without a safely reached regular `CMakeLists.txt` keeps the
output even when the CMake cache and Zephyr marker remain. The old
`cmake-build` and `west-build` marker-only names are retired. Configurations
that still select either name fail validation until the operator chooses an
anchored profile.

### Source-anchored Meson output

`meson-build-anchored` applies the same ownership, checkout, in-use and
three-day idle checks. It requires a regular `build.ninja`, a direct
`meson-private/` directory with a regular `coredata.dat`, no entry named
`meson.build` in the output, and a regular
`meson-info/meson-info.json` of at most 1 MiB. The generated JSON must have
`error: false`; its `directories.build` must equal the candidate and
`directories.info` must equal its `meson-info/` directory. Its normalized
`directories.source` must be a strict ancestor under the bound root.
That source must still hold a safely reached regular `meson.build` on the
same filesystem. JSON with duplicate required fields, malformed paths,
missing files or mismatched paths is refused. Discovery and judgement
read the JSON through the reached candidate fd; reclaim rechecks it and
the source file identity under the candidate lock. A copied or moved
Meson output does not match until Meson reconfigures it for the new path.

These checks establish a surviving build description. They cannot prove
that external dependencies, local configuration files stored only in the
output, or generated artifacts can all be recreated. The profile covers
Meson's Ninja backend only; other backends lack the required
`build.ninja` marker.

### Source-anchored Autotools output

`autotools-build-anchored` covers GNU Autoconf out-of-source Makefile
builds. It requires regular `config.status`, `config.log` and `Makefile`,
and no `configure.ac` entry in the candidate. The status script is read as
data through the reached candidate fd; diskgc never executes it. Its
single-quoted `ac_pwd` assignment must name that exact normalized build
directory. Its `srcdir` must name a strict ancestor under the bound root,
either as a normalized absolute path or only `..` components. A regular
`configure.ac` must survive at that source on the same filesystem. Unusual
shell quoting, duplicate assignments, paths outside the bound root, or
status files above 2 MiB are refused. Source identity and the status file
are checked again under the candidate lock. A copied or moved build retains
its old `ac_pwd` and is refused.

The ordinary ownership, checkout, in-use and three-day idle guards also
apply. The profile accepts only the generated assignment form tested with
GNU Autoconf 2.73 and only builds with a generated `Makefile` and a source
`configure.ac`; other Autotools layouts stay unselected. A surviving
`configure.ac` establishes a rebuild description, but cannot prove that
external dependencies, local build-only edits or generated deliverables
are recoverable.

### Deletion

For the `delete` action of kind `fs` (`fs_kind` in the core). A dry run,
the tally that orders a tier and the reclaim share one discovery and one
judgement, so a plan (`--dry-run`) predicts what a reclaim does, `locked`
aside (a plan takes no lock), and the same checks run whichever of them
asks.

**Discovery** walks the roots by the patterns (Before the walk;
docs/reference.md, Profiles) and records each candidate with the root it
was found under and its identity (device, inode, owner, type, birth time
where reported). It judges nothing inside a candidate but the source's
own `where`, which decides whether it is a candidate at all. A reclaim
acts only on a discovery that completed: one stopped by the deadline or an
error of the run reclaims nothing of the tier. With the index
([architecture.md](architecture.md#the-index)), a candidate taken from it
is discovered again at reclaim time: an index unit by reaching it from
its root (step 2) and a `statx` that must match the stored (`st_dev`,
inode, birth time), otherwise `stale`; a block's members by the tier's
discovery restricted to the block's site, under the same completion
rule. The identity from that reclaim-time `statx` or listing (device,
inode, owner, type, birth time) is what steps 2 to 4 compare against;
an identity stored by an earlier tally is never trusted as such.

**The tally** (the tier's order, docs/architecture.md, The reclaim walk)
runs steps 1 and 2 below and, of step 4, the source's `where` and the
eligibility walk on each candidate, read-only. A single-source tally does
not apply the idle requirement; a filesystem union tallies every term with
its idle requirement so only admitted terms determine the effective checkout
policy before queueing. Neither form takes a lock or performs an in-use
check. A candidate it refuses is skipped for the run and never locked.

**Per candidate**, in the tier's order, every check is made again on the
filesystem as it is then, whatever the tally or a plan said:

1. **Place.** The deadline; the mount table, read now; the checkout
   guard's path rule (`contains_checkout`, No source checkout), another
   filesystem than `PATH`'s (`other_fs`), a mount at or inside the
   candidate (`contains_mount`, never touched).
2. **Reached again** from its root, one directory at a time (Path
   traversal; the root's `mountinfo` line judged first), and it must be
   the object discovery found (identity; `changed`, or `missing` when
   gone). No keep marker in the root or any directory on the way (`kept`):
   a `statx` of the name in each directory reached, never a listing. A kept
   candidate is never locked.
3. **Lock.** Take an exclusive `flock` on the candidate (directories),
   opened below its parent, without waiting, as systemd-tmpfiles does, so
   cooperating tools see it is being removed: held by anyone else, shared
   or exclusive, it is `locked`. The locked directory must be the object
   discovery found (`changed`). A candidate that is a single file is not
   locked.
4. **Judged again under the lock:** the mount table read again; the
   **path** again (below); the place again on it (a mount made since
   step 1 is `contains_mount`, never looked into); the keep markers on the
   way again; the source's own `where` (no longer
   passing it: `changed`); the eligibility walk (above), whose root must be
   the object found and the one locked (`changed`); the idle requirement
   on its newest change (none known: never idle). Then the in-use check
   against a process scan within the freshness bound (In use: Freshness).
   An unsettled scan is retried once after 20 ms; a persistently blocked
   scan can only skip candidates for half a second before it is replaced.
   Right after that check, on its scan, and before the delete starts (or a
   command action's program, which is given the path), the mount table is
   read once more and the path, the place and the keep markers are judged
   once more, then `protect` and `!profile:` (Not protected, not excluded)
   and the other checks on that scan, and the path and keep markers once more as the last check before
   acting. Any failure: release, skip.

   *The path* (TASK-165): the directories step 2 reached stay open, and
   the lock, the walk and the delete go through them, so a directory on
   the way renamed since step 2 (into a directory holding a keep marker,
   into a checkout's metadata, out of the root) leaves them valid while the
   candidate's path names something else. Every rule judged on the path
   (the checkout path rule, `protect` and `!profile:`, the keep markers on
   the way) is therefore judged only after the path is shown to lead,
   now, to what was reached: the root resolved anew (Path traversal) must
   be the directory held for it, each name on the way `statx`ed (not
   followed) in the directory held above it must be the directory held for
   it, and the candidate's name in its parent the object found (identity:
   device, inode, owner, type, birth time where reported). Anything else,
   a name gone included, is `changed`. The keep markers are then looked
   for in the directories held, which are the directories on the path. One
   `statx` per level and a transient resolution of the root; nothing
   stays open, so the fd budget is step 2's. Each level is checked at its
   own instant: the guarantee is that every level led to the next during
   the check; a rename after its level was checked is the check-to-delete
   window below.
5. Delete depth-first exactly the entries the walk recorded, right after
   the check, through the clear that check made for exactly that record
   (an expired clear is checked once more against a new scan), reopening
   each directory relative to its parent (`openat2`, see Path traversal)
   and checking its identity against the walk's record, with a bounded
   number of fds (the shape above). Before each `unlinkat`, check the
   deadline, then `statx(AT_SYMLINK_NOFOLLOW)` the name and compare its
   identity (device, inode, owner, type, birth time where reported) with
   what the walk recorded, then read the clock once more (the `statx` can
   block, the machine can sleep: no change starts past the deadline); the
   `unlinkat` follows with no filesystem operation in between. A
   mismatch (or the name gone, or a mount met on the way down, `EXDEV`, or
   the `unlinkat` itself finding the name gone, mounted on or of another
   type) stops the delete: nothing more is unlinked and the candidate is
   `changed`; a later run walks it afresh.
   `make_writable` (if the profile allows it) adds owner write permission
   to a directory right after it is opened and matched, before anything in
   it is unlinked, after all the checks. Its readable fd is reopened and
   matched; the gated permission port reads its current mode, then reads
   the deadline immediately before `fchmod`, which can only add owner write.
   It is not undone: a delete that stops part-way (`changed`, an
   error, the deadline) leaves the directories it entered with owner write
   permission added. Linux has no atomic "OR this mode bit" operation: a
   concurrent chmod between the port's mode read and `fchmod` can still
   lose that other chmod's change. The kernel may also clear setgid under
   its normal permission rules; the requested mode preserves all other bits.
6. Release (by closing), whatever the outcome.

Each candidate is one action, measured as every action is (the observed
change in available space around it, docs/architecture.md, Measuring
space; `null` when the sample after it failed, which stops the run: the
goal can no longer be observed). A candidate deleted part-way is an action
and a skip or an error. The deadline is checked before each candidate
(in the walk over the tier and again in step 1), before each directory the
reach (step 2) opens and before its `statx` of the candidate and of each
keep marker, before each `where` predicate, before each `openat2`,
listing and `statx` of the eligibility walk and of the delete's reopening
of directories, and right before each `unlinkat` and `fchmod` (step 5).
Not before: the reads of the mount table and the open and `flock` of step 3 (the lock does
not wait). The in-use scan checks it before each of its listings and after
each pause (In use, The run's deadline and signals), so it overruns by at
most one batch of reads. A dry run checks the deadline again before
judging its candidate; a reclaim checks it in the delete engine before
each mutation.

**Scope** means *the objects the walk checked*. Every directory diskgc
descends into is matched by (device, inode) against the walk's record, and
the candidate itself is held open while it is deleted, so if a checked
directory is renamed elsewhere mid-delete, diskgc keeps deleting that same
object (the cruft it checked), never whatever now sits at the old path.
The in-use check returns a single-use `Clear` that borrows the exact
candidate and `WalkRecord` it checked. The deleting protocol consumes that
pair while the scan is fresh; it cannot substitute a second walk or another
hard-linked name with the same inode. The lower-level
`engine::delete_unchecked` entry point is explicitly for isolated engine
and host tests; production protocols use `inuse::clear_and_delete`.
Two windows remain and are accepted: an entry replaced between the `statx` and the
`unlinkat` of its name (Linux has no unlink-by-inode), and a file opened or
created inside the candidate after its check in step 4. A third limit is separate from
them: an object removed and recreated between the walk and its check with
the same device, inode number, owner and type (inode number reuse) passes
the identity check where the filesystem reports no birth time; where it
reports one (tmpfs, ext4, btrfs, xfs), the birth time differs and the
candidate is `changed`. The inode generation is not compared (`statx` does
not report it). A file created mid-delete is
never unlinked (the walk did not record it) and makes the delete partial
(`ENOTEMPTY` on its directory); that is reported as an error for the
candidate, never retried silently. So is any other refused unlink (e.g.
`EACCES` in a read-only directory without `make_writable`): the delete
stops there, partial.

**Accepted limits of the `fs` protocol**, by the race policy (Mark,
2026-10-17: the window between a check and the deletion, and races that
need several events, are stated limits; one event that makes diskgc take
a candidate in use for unused, where a fix is known, is not accepted).
Each with the number of events outside diskgc it needs:

- *Check to delete* (one event, the window): a change made after step
  4's checks and before or during the delete is not looked for again. A
  process that starts using the candidate is In use's limit 5 (at most
  the freshness bound, usually milliseconds); a keep marker put on the way
  to it, a directory on the way renamed after the last check of the path
  (under a keep marker, into a checkout's metadata, out of its root; the
  delete keeps deleting the objects it checked, Scope), or a change that
  makes its source's `where` fail, is not seen and the candidate is
  deleted. The window runs from that last path check, after the in-use
  check, to the end of the delete, so it grows with the candidate's size; a file, socket or FIFO created inside it is
  never unlinked and stops the delete (`ENOTEMPTY`, an error).
- *Mounts made after a table was read* (one event): the table is read at
  step 1 and again at step 4, and a mount can appear after either read
  (and after discovery's or the tally's). A live mount made inside the
  candidate then is still found by whatever reaches it next (the reach's
  `RESOLVE_NO_XDEV`, the walk's mount id, the delete's `EXDEV`:
  `contains_mount` or `changed`); a dead FUSE or network mount made in that
  window can hang the first `statx` that reaches it (the deadline cannot
  interrupt it; an overrun, never a deletion). The same for a mount
  stacked on a root between the reading of its `mountinfo` line and its
  resolution: the resolution refuses it (`unsafe_path`), but its `statx`
  of a dead mount's root can hang first.
- *A process using a dead NFS mount* can hang the in-use scan: a limit of
  the scan, for both kinds (In use).
- *Inode number reuse* (two events: the candidate removed, and another
  object created at its name with its inode number, owner and type, after
  discovery): it passes the identity checks of steps 2 to 4 where the
  filesystem reports no birth time (the third limit above). Any other
  replacement is `changed` at whichever of steps 2, 3 or 4 first sees it:
  each compares with the identity discovery recorded, and the walk's root
  with the locked directory.

### Other properties

- **Hard links:** removing a name that is hard-linked elsewhere frees
  nothing; the signed `avail_delta` shows that.
- **Deleted but open:** a file deleted while still open keeps its space
  until the process closes it. Every report lists such files on the
  requested filesystem (`held_by_deleted_files`): the open fds of visible
  processes on regular files with link count 0, per process, each file's
  allocated bytes counted once across all processes. Every holder is listed;
  when several processes hold one file, its bytes are attributed to the
  lowest pid and the other holders show 0 bytes. On btrfs, mounted
  subvolumes with the same mountinfo source are treated as one space pool.
  An unmounted sibling subvolume, a subvolume visible only in another mount
  namespace, or an alias of the same device with a different source string
  may be omitted from the report. A deleted file that is only mapped (its fd
  closed) is not listed. diskgc never kills processes.
- **tmpfs:** if `/tmp` is a tmpfs, profiles under it only help a request for
  that tmpfs (filesystem matching), which is correct: it frees memory, not
  disk.

## `cargo-target`

The filesystem checks above apply (mounts, ownership of every entry, in
use, protection), with Cargo's own rules replacing the generic deletion
steps.

### Lock files

Cargo's lock set changes between versions. Measured on 2026-10-15 by
sampling `/proc/locks` during real Cargo commands, and on 2026-10-17 by the
Cargo toolchain tests in the containers ([testing.md](testing.md#cargo-toolchains-under-test)):

- Cargo 1.90 (stable) holds an exclusive `flock` on `<profile>/.cargo-lock`
  during a build.
- Cargo 1.96 (nightly) holds `.cargo-build-lock` *and* `.cargo-lock` during
  a build; a second build waits on `.cargo-build-lock` first; `cargo check`
  waits behind a running build.
- Cargo 1.99.1 (stable) and the 2026-10-16 nightly (1.101) create all
  three lock files with a plain `build`, and while a build script runs hold
  `.cargo-build-lock` and `.cargo-artifact-lock` exclusively and
  `.cargo-lock` *shared*. With `.cargo-lock` held exclusively by another
  process, a build waits for it holding `.cargo-build-lock`.
- **No lock is held while the program runs** under `cargo run` or
  `cargo test`: measured for 1.90 and the 1.96 nightly over the whole
  runtime of the binary, and for the toolchains pinned in
  [testing.md](testing.md#cargo-toolchains-under-test) between compilation
  and the start of the binary and while a `cargo test` binary runs (race
  tests 4 and 8); their locks during a `cargo run` binary's runtime were
  not sampled.
- `.cargo-artifact-lock` exists in some targets on the development machine;
  1.90 and the 1.96 nightly did not create it with plain `build` / `run` /
  `test`.
- **Separate build directory** (`CARGO_BUILD_BUILD_DIR` or
  `build.build-dir`): 1.99.1 and the 2026-10-16 nightly split the lock set.
  The target's profile directory keeps `.cargo-artifact-lock` (exclusive),
  `.cargo-lock` (shared) and the final artifacts; the build directory's
  `debug/` holds only `.cargo-build-lock`, next to the intermediate files;
  both directories get a `CACHEDIR.TAG`. `cargo test` runs the test
  binaries from the build directory. 1.90 ignores the setting.
- **New build-dir layout:** the 2026-10-16 nightly uses it by default. The
  profile directory has no `.fingerprint/` and no `deps/`; each unit gets
  `build/<package>/<hash>/` holding its `fingerprint/` and `out/` (a build
  script's run also `run/`), libraries and binaries included, and test
  binaries run from that `out/`; the final binaries are hard-linked into
  the profile directory as before. The lock files are those of 1.99.1.
- **Lock order** (measured 2026-10-18 in the containers with `strace -e
  flock,openat` of `build` and `check`, fresh and repeated): 1.99.1 and the
  nightly open each lock file with `O_RDWR|O_CREAT` and take
  `.cargo-build-lock` (exclusive) first, then `.cargo-lock` (shared), then,
  for `build` only, `.cargo-artifact-lock` (exclusive); 1.90 takes
  `.cargo-lock` (exclusive) alone. A Cargo creates the lock files it takes
  when they are missing: a 1.99.1 build on a target 1.90 made creates
  `.cargo-build-lock`, takes it, and then waits for a `.cargo-lock` held by
  someone else, writing nothing else meanwhile. Cargo releases its locks
  with `LOCK_UN` (diskgc only ever closes, Host lock).
- **What a Cargo writes before `.cargo-lock`**, the invariant of the
  Reclaim protocol (step 3). Measured on the three pinned toolchains, in
  the containers, 2026-10-18:
  - `build` on a classic target 1.90 made, while another process held
    `.cargo-lock`, traced as `tester` in the project directory with
    `strace -f -tt -y -s 200 -e trace=open,openat,openat2,creat,mkdir,mkdirat,unlink,unlinkat,rmdir,rename,renameat,renameat2,link,linkat,symlink,symlinkat,flock,utimensat,futimesat,truncate,ftruncate,chmod,fchmod,fchmodat,chown,fchown,fchownat,lchown,write,pwrite64,fallocate,copy_file_range,setxattr,fsetxattr -o FILE -- cargo build`:
    below the target only `target/.rustc_info.json` (rewritten in place,
    outside every profile directory), a `mkdir` of the profile directory
    (`EEXIST`; profile directories are never deleted) and the `O_CREAT`
    open of the lock files it takes first (`.cargo-build-lock` on 1.99.1
    and the nightly); then it waits for `.cargo-lock`;
  - by race test 2 ([testing.md](testing.md#cargo-race-tests)), against
    diskgc paused in its deletion holding every lock: `build`, `check`,
    `test --no-run` and `doc` wait for a lock diskgc holds and change
    nothing in the profile directory but the lock files they create and
    hold, on each toolchain's own target (classic on 1.90 and 1.99.1,
    per-unit on the nightly) and, for 1.99.1 and the nightly, on a classic
    target 1.90 made (`.cargo-lock` waited for); a split-mode `build`
    (`CARGO_BUILD_BUILD_DIR`) on a target first built whole takes the build
    directory's `.cargo-build-lock`, creates no lock file in the target's
    profile and waits there too.

  Inferred, not measured: the other commands that compile (`run`, `bench`,
  `fix`, `rustc`, `rustdoc`, `install` and `package` building in a target)
  go through the same locked layout (Cargo's `BuildRunner`, which takes the
  locks before it prepares the profile directories). Untested: `run`
  (never started during a deletion; only paused before its binary),
  `fix`, `bench`, `rustc`, `rustdoc`, `install`, `package`.

  **The exception: `cargo clean`** without `-p` (the whole target, or a
  whole profile directory with `--profile dev`) takes no lock and deletes
  (measured by race test 2's clean case on the three toolchains: it runs to
  its end while diskgc holds every lock, lock files included; `--release`
  and other profiles are inferred to do the same). That is safe for
  diskgc: both only delete, and no new build output is at risk. In the
  order tested (the clean finishes while diskgc is paused inside a part),
  diskgc's next identity-checked unlink finds nothing (`ENOENT`): the
  target is skipped as `changed` and nothing more is unlinked; its
  directory descriptors are then of removed directories, which a later
  build cannot write into. When the two overlap, diskgc may still unlink
  a few objects it recorded before the clean removes them, and the clean
  may itself fail on an object diskgc removed first (`ENOENT`); within the
  check-to-delete and multi-event policy that is harmless, as both sides
  only delete what both may delete. A clean that lands between two parts
  meets the lock re-listing first (step 3): the lock files are gone, so
  the target is `changed` before any unlink (follows from the order of
  step 3; not tested). `cargo clean -p` waits for the locks like a
  build.
- **Lock sets by command** (measured 2026-10-18 in the containers on the
  three pinned toolchains): `cargo check` on a fresh target of 1.99.1 and
  the nightly creates only `.cargo-build-lock` and `.cargo-lock`; `build`,
  `test`, `doc`, `--release` and `--target` create all three in every
  profile directory (`<triple>/debug` included); 1.90 creates `.cargo-lock`
  alone. With a separate build directory (also `--release`) the target
  directory's profile holds `.cargo-artifact-lock` and `.cargo-lock`, the
  build directory's only `.cargo-build-lock` (on the nightly in the
  per-unit layout).
- **What Cargo gives the programs it runs** (same measurement; `cargo run`
  and `cargo test` through a runner, and a test binary): `CARGO_MANIFEST_DIR`
  (the package's directory), `CARGO`, and an `LD_LIBRARY_PATH` naming the
  profile directory (1.90.0 and 1.99.1 also its `deps/`; with a separate
  build directory 1.99.1 names the build directory's `deps/` instead, the
  nightly only the target directory's profile directory); 1.90.0 and
  1.99.1 also pass the package's `OUT_DIR` (inside the profile directory;
  the nightly passes none to a runner: race test 8's logs). A
  `CARGO_BUILD_BUILD_DIR` set in the environment is inherited.

diskgc has an explicit **allowlist of known lock file names** (`.cargo-lock`,
`.cargo-build-lock`, `.cargo-artifact-lock`). A profile directory containing
any other `.cargo-*lock*` file is an unknown layout: the target is skipped
(`unsupported_layout`). A lock file is a regular file (`statx`, not
followed): a directory, symlink, FIFO, socket or device with a lock-like
name is no Cargo's and makes the target `unsupported_layout` too, and a
lock file of another type at a later check than when found is `changed`.
A `.cargo-*lock*` name anywhere deeper in a profile directory (not a lock
file the protocol takes) refuses the target as `contains_special`
(eligibility walk). Lock files are never deleted or moved.

**Lock sets.** Only the measured whole sets are reclaimed: `.cargo-lock`
alone, with `.cargo-build-lock`, or with both newer locks. A profile
directory holding only `.cargo-build-lock` is the build directory's half of
a **split build directory**, one holding `.cargo-lock` and
`.cargo-artifact-lock` without `.cargo-build-lock` the target directory's
half; any other combination was never measured. All of them are
`unsupported_layout`: **neither half of a split pair is reclaimed.** Both
halves would have to be locked together, and nothing on disk links them
(the build directory comes from the environment or `.cargo/config.toml`,
and its path can be a hash of the workspace's), so diskgc cannot find a
half's partner. Locking one half alone is not enough: between compiling
and starting a test binary Cargo holds no lock and works in the project
directory, which is not the build directory's parent, so a build directory
reclaimed then makes `cargo test` fail. Limit: a project with a separate
build directory is not reclaimed at all until pairs can be recognised;
both halves are listed with the reason. A target directory once used
without a separate build directory keeps all three lock files and counts
as whole; a Cargo using it as a half then still conflicts with diskgc on
`.cargo-lock` and `.cargo-artifact-lock`.

### Discovery, idleness, eligibility

- **`/tmp` root decision (TASK-100):** a tier may explicitly bind the
  built-in `cargo-target` profile to `roots: [/tmp]`; it then finds signed
  targets up to six directory levels below `/tmp`, including detached
  `CARGO_TARGET_DIR` targets. We do not add `/tmp` as an implicit or fixed
  root: a tier's caller must choose the filesystem it wants to free and the
  idle period, and the existing profile keeps one matching definition.
  This is a policy choice, not a different reclaim protocol. On the
  2026-10-20 host, nine top-level `*review-target` directories in `/tmp`
  had a valid `CACHEDIR.TAG`, a `debug/.cargo-lock`, and no adjacent
  `Cargo.toml` (3.9 GiB total by read-only `du`); none was deleted to make
  this observation.
- **Discovery:** walk each root for directories with a valid `CACHEDIR.TAG`
  (a regular file starting with the specification's signature, read
  through the descriptor of the object checked) and at least one profile
  directory, `<target>/<profile>/` or `<target>/<triple>/<profile>/`
  holding a `.cargo-*lock*` file, known name or not (a target whose only
  lock name is unknown is listed as `unsupported_layout`, not missed). A target is at most six directories
  below its root (the root itself is depth 0). Do not descend into a found
  target. Hidden directories are walked like any other (agent worktrees
  hold targets). Never follow symlinks or cross mounts: a mount below a
  root is not entered. A root on a network filesystem is refused without
  being read (`unsupported_fs`): Cargo does not lock there. A target
  reached through several roots (overlapping roots, a bind mount of the
  same directory) is one target, by (device, inode). Roots are walked
  outermost first (fewest path components), so such a target is recorded
  under the outermost root that reaches it, whose way down to the target
  passes every inner root's: the keep markers that count (below) do not
  depend on the order the roots are given in. Limit: for a target reached
  through two unrelated paths (a bind mount), only the markers on the way
  from the root that recorded it count. A directory discovery cannot read
  is reported (`unreadable`); one that vanished meanwhile is not.
- **Reached again at every check:** from its root, one directory at a
  time; it must be the directory found (`changed`, or `missing` if gone).
  Its profile directories and lock files are listed again: one created,
  removed or replaced since discovery is `changed`. A directory of another
  mount one or two levels inside the target (where a profile directory
  could hide) is `contains_mount`.
- **Keep markers** (`kept`): in the root or any directory between it and
  the target, in the target directory or a directory directly in it, or in
  what would be deleted (a profile directory's entries, walked). A marker
  deeper in a part the kind never deletes (`target/doc/x/`) keeps only that
  part.
- **Ownership:** besides every entry that would be deleted (the walks
  below), the target directory, the triple directories holding profile
  directories, the profile directories and their lock files (`not_owner`).
- **Layouts:** where a profile directory keeps its fingerprints, which the
  reclaim protocol deletes first. *Classic*: a directory `.fingerprint/` or
  `deps/` in it (1.90, 1.99.1, a build directory's half); its fingerprints
  are `.fingerprint/`. *Per-unit*: neither, and a directory `build/` (the
  nightly's new build-dir layout); its fingerprints are every
  `build/<package>/<hash>/fingerprint`. A profile directory can mix both
  (a target that 1.99.1 and the nightly both built has `.fingerprint/`,
  `deps/` and per-unit units in `build/`): it counts as classic, and its
  fingerprints are both `.fingerprint/` and every
  `build/<package>/<hash>/fingerprint` (Reclaim protocol, step 3).
  Only real directories count, judged by `statx` without following: a
  regular file or a symlink with one of these names is no Cargo layout. A
  profile directory that holds anything but none of the three is an
  unknown layout (`unsupported_layout`): the protocol would not know what
  to delete first, and a non-Cargo cache with a tag and a lock-like name
  must not pass by its names. An entry whose type changes between the
  check and its walk is `changed`.
- **Eligibility walk:** every entry of every profile directory except the
  lock files is walked as a candidate of its own (Filesystem candidates:
  ownership, keep markers, mounts, sockets and FIFOs, Cargo lock names and
  nested targets, VCS checkouts as the profile's `checkouts` says, by
  name, and by the path from the root down to each profile directory,
  judged once on the target as found); the first
  refusal is the target's. The entry budget is
  one for the whole target: all its entries together, not each entry, may
  hold at most `DEFAULT_MAX_ENTRIES` (`too_many_entries`), so one target
  holds no more records in memory than one candidate may.
- **Last build** (the kind's `idle_for` measure): the newest mtime or ctime
  of the profile directories, their lock files and everything the walks
  recorded in them. The same measure on every layout: it reads no
  layout-specific directory. It changes on real rebuilds but not on no-op
  builds, so an up-to-date, actively used target can look idle; the lock
  and process checks still protect visible running work. A time the
  kernel does not report, or a last build in the future, passes no idle
  filter (`unsupported_fs`, `future_time`): an unknown age is never idle.
- **Empty:** a target whose profile directories hold nothing but lock files
  is `empty`. It remains discoverable, but a live `ensure` omits it from
  `skipped` and the human skipped count. A `--dry-run` plan may list it as
  ineligible with reason `empty`.
- The check made for a plan (`--dry-run`, `survey`) takes no lock; whether
  the locks are free is the Reclaim protocol's step 1.

### In use

- any known lock file cannot be `flock`ed without waiting (`locked`;
  Reclaim protocol, step 1);
- a visible process uses (open fd, working directory, root, executable,
  mapping) an object the walks recorded, or names a path inside one of
  them in its initial environment (Filesystem candidates, In use), or uses
  the target directory, a directory directly in it, a profile directory or
  a lock file, matched by (device, inode) (`in_use`); an unsettled scan or
  an unreadable process of the user blocks as there (`in_use_unknown`);
- a visible process has its working directory or executable inside the
  target, by path text (a test binary still running; parts the walks do
  not read, such as `target/doc` and `target/tmp`);
- a visible process whose executable name, after normalising Nix wrapper
  names (`.cargo-wrapped` → `cargo`), starts with `cargo` has its working
  directory in the target's project directory (the target's parent) or
  below it, by path text, or in the project directory itself by (device,
  inode);
- a visible process's initial environment names a path inside the target,
  whatever the variable: `CARGO_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR`, and
  the `LD_LIBRARY_PATH` and `OUT_DIR` Cargo gives what it runs;
- a visible process's initial environment has `CARGO_MANIFEST_DIR` in the
  project directory or below it: a process Cargo started for a package of
  the project. `cargo run` execs its runner (or the binary) in its own
  process, so no process named `cargo` is left; `cargo test`'s runner and
  test binaries carry it too;
- the target is **detached** (its parent holds no `Cargo.toml` that is a
  regular file, judged by `statx` without following, or that cannot be
  told; a mount point at that name is treated as unknown without statting
  it; a directory or a symlink of that name, dangling or not, does not
  make it attached: it was put there by `--target-dir`, `CARGO_TARGET_DIR`,
  a `target-dir` in `.cargo/config.toml`, or it is a tool's nested target)
  and a visible Cargo process (named as above) runs, wherever it works:
  where a detached target's Cargo runs cannot be told from its path, so
  any Cargo may be using it. A shared target directory is therefore
  reclaimed only while no visible Cargo runs.

TASK-100 keeps this detached-target rule unchanged. `--target-dir` and
`.cargo/config.toml` can direct a Cargo working in an unrelated project to
the same target without leaving a path in its initial environment. Cargo
also releases its build locks before starting `cargo run` and `cargo test`
binaries, so finding the locks free cannot prove the target idle then. Age
is no proof either: a previously idle target can be selected by a new Cargo
process. Until Cargo exposes a reliable target identity throughout the
run, an unrelated visible Cargo must continue to block a detached target.

Paths are compared byte for byte, never through a lossy text conversion
(a name that is not UTF-8 stays as it is).

Because Cargo holds no lock while a binary runs, nor between compiling it
and starting it, these process checks are the only protection during
`cargo run` / `cargo test`. They are heuristics with these remaining blind
spots between compilation and launch (afterwards the binary or runner
names the target in `LD_LIBRARY_PATH`): a Cargo started outside the
project directory of an attached target, with `--manifest-path` and the
target next to that manifest, or in another project whose `target-dir`
(`.cargo/config.toml`, `--target-dir`, `CARGO_TARGET_DIR` set after
`exec`) points at an attached target (the detached-target rule above
sees only detached ones); a Cargo the scan cannot see (another user's, a
process diskgc cannot read and excuses, or one in another PID namespace);
and a Cargo in another mount namespace, whose working
directory and paths are not this namespace's (only its uses of the
target's own objects, matched by identity, count). A deleted binary then
makes `cargo run` fail with "No such file". A relative
`CARGO_TARGET_DIR`, a variable exported after `exec`, and a process whose
environment is empty (`env -i`) give no environment evidence. A strict
guarantee would need Cargo itself to hold a lock until the run ends.

### Reclaim protocol

The step that deletes (`cargo_target::reclaim` in the core).

1. **Lock.** A completed tally's measured last build is checked against
   the tier's idle window first: one it refuses (`not_idle`, `too_idle`,
   or an unknown-age reason) is skipped without attempting a Cargo lock.
   Untallied targets continue to the locked check. Reach the target again
   from its root, as found (Discovery);
   refuse before locking a target whose layout or lock set is not one
   diskgc reclaims (`unsupported_layout`, `empty`: an unknown lock name
   would not be taken), and, first, when the profile refuses checkouts, a
   target with a profile directory inside a checkout's metadata by its
   path, from the root down to each profile directory, the target's, a
   triple's and the profile directory's own names included
   (`contains_checkout`, No source checkout). The recheck under the locks
   need not repeat it: it finds the same profile directories (`changed`
   otherwise), so the same paths. Then take every lock file of every
   profile directory, exclusively, without waiting. Holding every lock is what
   makes the deletion safe, not the order they are taken in: diskgc only
   ever tries a lock (`flock` with `LOCK_NB`) and never waits for one, so
   it can never be part of a deadlock. The order is Cargo's own
   (`.cargo-build-lock`, `.cargo-lock`, `.cargo-artifact-lock`; Lock
   files) only to spare a Cargo that started first a brief wait, within
   one profile directory: diskgc's first attempt there then fails before
   it holds anything that Cargo waits for. Each profile directory is
   opened once, below the target, and must be the one found; it serves
   the walks and the deletion below. Each lock
   file is opened `O_PATH` below it and must be the one found (identity,
   type included: `changed`); it is then reopened read-only through that
   fd (`Host::reopen_regular`), never by name, so nothing but that regular
   file is ever opened for the `flock` (a FIFO or device that took its name
   would not be). Any lock held by someone else, shared or exclusive:
   `locked`, release all, skip. A Cargo that holds `.cargo-build-lock`
   while waiting for `.cargo-lock` makes this fail, which is intended.
2. **Recheck under the locks.** Inspect the target again: the same profile
   directories and lock files as when found (one created, removed or
   replaced since, a `cargo build --release` or a newer Cargo's lock file
   started meanwhile: `changed`), no keep marker, ownership, the layout.
   Walk every entry of the locked profile directories (the eligibility
   walk, one entry budget for the target). Measure the last build and
   apply the tier's idle requirement to it again, if it has one, even if
   the tally's earlier measurement passed (with a requirement, an unknown
   age is never idle). `ensure` hands the protocol no earlier last build
   (TASK-168), so a target rebuilt between its tally and its locks is
   caught by this idle check only when the tier has an idle requirement (a
   rebuild inside it is `not_idle`); without one, only the in-use check
   below guards it. A target rebuilt but still idle enough is reclaimed
   like a target first found in that state: under the locks no Cargo of
   its lock set builds, so what is measured here is what is deleted. The
   core can also be given the last build the caller chose the target by
   (`ReclaimPolicy::judged_last_build`); a newer one or none is then
   `changed`, but no production caller passes one (a model test does).
   Then
   the in-use check of the whole target against a scan made now, under
   the locks (a scan from before could miss a process started meanwhile):
   every entry's record and the Cargo process checks (In use). Then the
   target is **reached again from its root** (Discovery: one directory at
   a time, each listed for a keep marker, `kept`) and the directory reached
   must be the target whose locks diskgc holds; the path no longer leading
   to it (a directory between the root and the target renamed since step 1,
   under a keep marker, into a checkout's metadata or out of the root) is
   `changed` (TASK-165). Only then are `protect` and `!profile:` judged on
   the path (Filesystem candidates: Not protected, not excluded; the
   target's last build is the walk's newest change), and the checkout path
   rule of step 1 holds for the target where it is. Any hit: release,
   skip.
3. **Delete inside the locked profile directories,** each part with the
   per-entry identity check of Deletion, in this order:
   - **fingerprints first**, of every profile directory before anything
     else of any, whatever its layout or mix of layouts (a target that
     1.99.1 and the nightly both built has `.fingerprint/`, `deps/` and
     per-unit units in `build/`): the entry `.fingerprint/` if there is
     one, and every `build/<package>/<hash>/fingerprint` (each taken out
     of the walk record of `build/` as a part of its own and deleted below
     its unit directory, which is opened and matched against the record).
     Once any other entry is gone, no fingerprint in either of those
     places is left, so a profile deleted part-way always rebuilds instead
     of reusing a half-deleted unit;
   - then every other entry in name order, with `build/` and then `deps/`
     last: the directories that tell the layout, so that a run stopped
     part-way leaves a profile directory still recognised (classic,
     per-unit, or lock files only) and the next run reclaims the rest.

   Each part is deleted right after its own in-use check (`Checker::check`
   on exactly the record then deleted, with the Cargo process checks on the
   same scan, within Freshness) through `delete_cleared`; a hit stops the
   deletion (`in_use`). One scan serves every part whose deletion starts
   while that scan is younger than the one-second Freshness bound, from
   its first read: the scan made under the locks in step 2 serves the
   first parts, and a part that would start on a scan one second old or
   older gets a new one, which then serves the parts after it. This
   replaced, for the parts of one target, the 500 ms window in which a scan
   then served separate candidates (In use: Freshness; since TASK-203 a
   `delete` of `fs` candidates follows the same one-second rule). Decided 2026-10-25 for TASK-173 on
   Mark's delegation ("let one scan serve all parts of a target as long as
   each part's delete starts inside the 1 s bound. dont touch the 1 s
   rule"): with that window a scan slower than ~450 ms on a busy host
   served no part, so each part made its own scan, (parts + 1) scans per
   target, 5 to 13 s of held locks per target at load 18 to 34 (TASK-145).
   It is safe because nothing the bound protects changes: every part's
   deletion still starts on a scan younger than one second, its own in-use
   check and the Cargo process checks still run against that scan right
   before it, a stale scan still clears nothing, and the parts share the
   target's three attempts as before. What is reused longer is only the
   answer about processes, while diskgc holds every lock of the target,
   and the locks, not the scan, are what keep a Cargo of this lock set out
   (below). What changes is availability: a Cargo of the same lock set
   that starts waiting meanwhile is seen at a part's check up to one second
   after it started instead of up to half a second, so diskgc may delete
   one or more further parts before it stops for it (`in_use`), and that
   Cargo waits that much longer. The locks do not cover a process that
   takes no Cargo lock (a binary of the target run directly, a debugger,
   a proc-macro server loading `deps/*.so`, an indexer): one that starts
   using a later part while earlier parts are deleted is certain to be
   seen only if it started at least one second before that part's deletion
   starts, where the shorter window made that typically half a second; one
   that started closer to the deletion may be missed, the check-to-delete
   race of In use, limit 5. That margin is what the shorter window gave
   between parts; the Freshness bound is unchanged. Before each part its profile directory's lock
   files are listed again: one created, removed or replaced while diskgc
   holds the locks (a Cargo of another lock set starting, Lock files) stops
   the deletion (`changed`). That listing is a detector and a backstop,
   not the protection. The protection is an invariant: every lock set
   diskgc reclaims (whole sets, Lock sets) holds `.cargo-lock`, and the
   pinned Cargos measured (`build`, `check`, `test --no-run`, `doc`, a
   split-mode `build`; Lock files: what a Cargo writes before
   `.cargo-lock`) take it, or wait for a lock diskgc holds, before they
   write into the profile directory; other compiling commands are inferred
   to do the same. So a Cargo that starts during the deletion and creates
   a lock file diskgc does not hold (a 1.99.1 build on a target 1.90 made
   creates `.cargo-build-lock`) then waits on the `.cargo-lock` diskgc
   holds, having written nothing else, and diskgc stops at its next part
   as `changed` (race test 2). A visible Cargo waiting there is also seen
   by the next part's in-use check (`in_use`), which is how diskgc stops
   for a Cargo of the same lock set. The exception is a `cargo clean`
   without `-p`, which deletes without any lock: it only removes. If it
   lands while diskgc deletes a part, diskgc stops as `changed` at its
   next unlink; if it lands between two parts, the listing of the lock
   files before the next part finds them gone first (Lock files). A
   future Cargo that wrote into the profile before taking `.cargo-lock`
   would break the invariant; race test 2, which runs on every toolchain
   in the list, fails for any such write still there while the Cargo
   waits (one created and removed again in between is seen only by
   repeating the strace measurement; [testing.md](testing.md#cargo-race-tests),
   Limits). diskgc does not create missing lock files itself: that would
   be a write into the target, and the invariant does not need it.
   A part that stops part-way (`changed`, an
   error, the deadline) stops the target; what was deleted stays deleted,
   fingerprints first. Lock files are never in a record: the profile
   directory's own are not walked, and every walk refuses any
   `.cargo-*lock*` name, known or not, at any depth (`contains_special`,
   Filesystem candidates), so no lock file is ever unlinked, replaced or
   moved; the profile directories themselves are never deleted.
4. **Release** by closing the lock files' descriptors, whatever the
   outcome (never `LOCK_UN`, Host lock).

Nothing outside the locked profile directories is touched: not
`CACHEDIR.TAG`, not the triple directories, not top-level entries such as
`target/doc`, `target/tmp` (written by running tests without a lock) or
`target/criterion` (benchmark baselines, which are data). Because lock
files never move, a Cargo blocked on a lock and a Cargo started afterwards
lock the same inodes and stay mutually exclusive; a Cargo started during
step 3 prints `Blocking waiting for file lock` and continues after step 4.

**How long the locks are held:** steps 2 to 4, the walk, the scan and the
delete of the whole target; every Cargo of the target waits that long.
Measured 2026-10-18 in a container on tmpfs (Cargo 1.99.1, a real target
plus 100,000 files in `deps/`): 0.49 s for 100,026 entries untraced, 3.4 s
under the e2e suite's strace (`cargo_reclaim_lock_hold_time_on_a_large_target`,
which reports it); a real build of the race fixture, about 260 entries,
13 to 23 ms. It is reported (`locks_held`), not bounded: a deadline still
stops the protocol between operations.

**Limits.** The path from the root is checked once, at the end of
step 2, not before each part: a directory between the root and the target
renamed during step 3 is not seen, and the parts are deleted where they
are (the `fs` kind's check-to-delete window, Deletion). The lock files are
listed again before each part, not before
each unlink: a lock file created during a part is seen at the next one
(safe by the `.cargo-lock` invariant above, which this listing does not
replace). An entry renamed out of a locked profile directory while it is
being deleted is still deleted at its new place (object scope,
Deletion: Scope); that takes a deliberate rename of the very object
being reclaimed. Resumability has a gap: a classic profile whose only
layout directory is `.fingerprint/` (no `deps/`, no `build/`) holds
neither once a run stopped after its fingerprints went, so later runs
skip it as `unsupported_layout` (safe: its fingerprints are already
gone; the next Cargo build makes it recognisable again).
Directories inside a profile are not made writable (`make_writable` is
off): a read-only directory a build script left in its output makes that
part's delete fail (`EACCES`), an error for the target after its
fingerprints went.
Each part's in-use check is fresh (Freshness), not atomic with its
delete (In use, limit 5), and may answer from a scan that also served the
parts before it (step 3). The order "fingerprints first" relies on Cargo
treating a unit whose fingerprint is missing as dirty: race test 6
([testing.md](testing.md#cargo-race-tests)) kills the reclaim at the
first unlink of every part and in the middle of every larger one
(`.fingerprint/`, per-unit fingerprints, `incremental/`, `build/`,
`deps/`), on every pinned toolchain and on a target that 1.99.1 and the
nightly both built, and the next build always succeeds (compiling again
once the fingerprints are gone).

## Docker kinds

Each Docker operation is bound to **one backend**, determined in the
preflight and used for tally, preflight and execution alike:

- **Endpoint:** the active Docker context must be the default context on the
  system daemon's local Unix socket. A named context is skipped
  (`unsupported_backend`); a remote `DOCKER_HOST` or default-context endpoint
  is skipped (`remote_backend`). Rootless Docker (a socket under
  `$XDG_RUNTIME_DIR`) is not supported in v1 (`unsupported_backend`).
- **Builder** (`docker-build-cache`): the prune child sets
  `DOCKER_BUILDKIT=0`, selecting the Docker CLI's Engine command. That
  command calls `BuildCachePrune` on the pinned daemon, independently of
  Buildx's selected builder. Querying Buildx would write client config
  during a dry run. The read-only binding admits only Docker CLI 29.8.2,
  whose dispatch was verified in the disposable fixture. Other client
  versions skip as `unsupported_backend` until tested. Image profiles do
  not depend on this builder dispatch and do not carry the client-version
  limit.
- **Storage root:** `DockerRootDir` and its `buildkit` child for build cache;
  for images, both `DockerRootDir` and the image store's root (the
  containerd root when the containerd image store is in use, which is the
  default on Docker 29). If the storage that an operation
  frees is not on the requested filesystem, the tier is skipped
  (`other_fs`); if it cannot be determined, or spans several filesystems,
  it is skipped as `unsupported_backend` and reported.
- **Server identity:** the bound server must report a `Platform.Name`
  starting with `Docker Engine`. Podman's compatibility socket linked at the
  system socket path (podman-docker) passes the endpoint rules and is refused
  here as `unsupported_backend`.

A missing `docker` client skips the profile (`missing`). Any other failing
client query is currently a run error (exit 3): a daemon that is not
running, `EACCES` on the socket, or a `docker` command that is not the
Docker CLI. On a host whose `docker` is Podman's CLI shim, `docker context
show` exits 125 and every Docker profile reports that error (observed with
Podman 5.8.7, TASK-35). This fails closed, but it contradicts the backend
rule in [reference.md](reference.md#backend-skips-and-errors): once each
case is identified narrowly, a stopped daemon and `EACCES` become
`backend_unavailable` and the Podman shim `unsupported_backend` (TASK-113).

The Docker 29 storage-root spike uses one `docker info --format '{{json .}}'`
response from the bound server. `DriverStatus` containing
`["driver-type", "io.containerd.snapshotter.v1"]` identifies the
containerd image store. When `Containerd.Address` is exactly
`/run/docker/containerd/containerd.sock`, Docker manages that containerd;
its image root is `<DockerRootDir>/containerd/daemon`. This was verified
with pinned Docker Engine 29.8.2 inside the disposable dind fixture: after
an offline image import, actual image blobs appeared under that derived root
and the independent `/var/lib/containerd` tmpfs remained empty. The same
fixture with the classic `vfs` driver instead put image data under
`<DockerRootDir>/image/vfs` and `<DockerRootDir>/vfs`; selecting `vfs` had
silently disabled the default containerd image store. The fixture now
requires the active snapshotter and checks for imported blobs, so that
regression fails the test.

When Docker reports a different containerd socket (for example the system
`/run/containerd/containerd.sock`), `DockerRootDir` does not identify its
image root. The system containerd configuration may move it away from
`/var/lib/containerd`; without an independently verified root, diskgc
reports `unsupported_backend` and does not prune images. The verified classic
`vfs` driver uses `DockerRootDir`; other classic drivers remain unsupported
until their storage roots are proven. diskgc rechecks the daemon ID, endpoint
and storage roots immediately before deletion. At execution it opens
the local Unix socket with `O_PATH | O_NOFOLLOW`, queries `docker info`
through that pinned socket inode, compares the complete daemon identity and
storage facts, then gives the prune child the same inherited socket fd via
`/proc/self/fd/N`. Replacing the socket pathname between query and prune
cannot redirect the prune to another daemon. A disposable dind race test
replaces the pathname with a 503 decoy after the child starts and verifies
that the original daemon receives the prune while the decoy receives no
connection. The service user also
needs search permission on `DockerRootDir` and every component down to the
reported image store root or `<DockerRootDir>/buildkit` for build cache, so
diskgc can verify the actual filesystem. The
usual `0700` Docker data root gives an unprivileged caller `EACCES`; diskgc
fails closed without pruning. The disposable dind test grants execute-only
`0711` on those directories, then proves that an unreferenced image is removed
while a stopped container, its image, and a volume remain. This prerequisite
does not grant read or write access to the store.
See [Docker daemon storage configuration](https://docs.docker.com/engine/daemon/#configure-the-data-directory)
and [Docker storage backends](https://docs.docker.com/engine/storage/drivers/select-storage-driver/).
The pinned CLI's [builder dispatch](https://github.com/docker/cli/blob/v29.8.2/cmd/docker/builder.go)
forwards `docker builder` to Buildx by default but stays with its Engine
command when `DOCKER_BUILDKIT=0`. That [Engine prune command](https://github.com/docker/cli/blob/v29.8.2/cli/command/builder/prune.go)
calls `BuildCachePrune` directly. A native regression selects a separate
Buildx builder and proves the pinned daemon's own BuildKit records are
preserved by `until=1h` then removed by the unfiltered action.

Operations:

- `docker-build-cache`: `docker builder prune --all --force
  --filter until=<idle_for>`. Without `--all`, Docker removes only dangling
  cache. The `until` filter counts from **last use** (verified on Docker
  29.9 in a disposable `docker:dind`: entries used 5 s earlier survived
  `until=30s`; after 45 s idle they were removed). The pinned 29.8.2 native
  fixture also checks that recent cache IDs survive `until=1h` and cache IDs
  aged beyond `until=1s` disappear.
- `docker-images`: `docker-dangling` runs `docker image prune --force`;
  `docker-unreferenced` runs `docker image prune --all --force`, which also
  removes tagged images no container references. No age option: the image
  `until` filter is never "last used" (with the containerd store it is the
  local pull time, verified; with the classic store the build date, per
  Docker's documentation).
- One action prunes everything matching; there is no per-candidate stop.
  Volumes and containers are never touched.

## `nix-store`

Status: designed (TASK-118), unshipped. The profile loader refuses kind
`nix-store` until TASK-36 implements this protocol with the vectors and
container scenarios in [testing.md](testing.md#nix-store-backend-vectors)
and admits the first version to the allowlist
([reference.md](reference.md#backend-version-allowlist)).

The kind runs one Nix garbage collection through the **system Nix daemon,
as diskgc's own user**, bounded by the remaining need:

```
/nix/store/<hash>-<nix>/bin/nix-store --store unix:///proc/self/fd/<N> --gc --max-freed <need>
```

Nix decides liveness: GC roots, profile generations, temporary roots of
running builds and runtime roots of processes the daemon can see. diskgc
decides only whether and when the GC runs, which daemon runs it, and the
byte bound. It never names a store path to delete.

### Scope: one daemon, one store

- **Multi-user Nix only**, with the store at `/nix/store` and the daemon
  socket at `/nix/var/nix/daemon-socket/socket`. The daemon's root
  database is the one that counts on such a host: every unprivileged
  client registers its out-links (`gcroots/auto`), its temporary roots
  and its profiles through it, and root's system generations under
  `/nix/var/nix/profiles` are roots only if the daemon's state directory
  is `/nix/var/nix`. Binding means proving both: the GC goes to that
  daemon, and that daemon's root database counts the system's
  generations.
- Refused, as skips ([reference.md](reference.md#backend-skips-and-errors)):
  a store the caller selects other than that daemon (`local`, a chroot
  store, a socket elsewhere, `file://`, `dummy://`: `unsupported_backend`;
  `ssh://`, `ssh-ng://`, `http(s)://`, `s3://`, `gs://`:
  `remote_backend`), a single-user layout (`unsupported_backend`), and
  any client or daemon not on the version allowlist, including Lix,
  Determinate Nix and other variants (`unsupported_backend`).
- Refused under `--allow-root` as a static invocation error (exit 2),
  like command actions: root's `store = auto` is the direct local store,
  not the daemon, and a GC has no per-entry owner for `owners:` to check.

### Binding protocol

**Preflight** (every reclaim, for all tiers, before the first deletion;
also `--dry-run`): read-only queries on `Host`, ungated. The
first failing step decides the reason.

1. **Client.** `nix-store` is resolved on diskgc's `PATH` and logged, like
   a command action's program. Following its links must end in a store
   path's `bin/` directory, `/nix/store/<hash>-<name>/bin/`, in which
   `nix` is a regular executable owned by root and `nix-store` is a
   symlink whose text is exactly `nix` (CppNix ships one multi-call binary
   that dispatches on `argv[0]`; observed on 2.34.8). Not found:
   `missing`. Anything else (a distribution's `/usr/bin/nix-store`, a
   wrapper script): `unsupported_backend`. diskgc runs
   `<that dir>/nix-store` and `<that dir>/nix`, never a `PATH` lookup
   again: the directory lies inside an immutable store path, so the
   version queried is the binary that runs, and `argv[0]` keeps the
   dispatch. Every `nix` command below carries
   `--extra-experimental-features nix-command` in its constant argv: a
   stock `/etc/nix/nix.conf` does not enable it, and the child reads no
   other configuration.
2. **Layout**, reached as in [Path traversal](#path-traversal) (no
   symlink on the way):
   - `/nix/store` is a directory owned by root with the sticky bit set
     (the multi-user layout, `1775 root:nixbld`). Absent: `missing`.
     Another owner or no sticky bit (single-user Nix):
     `unsupported_backend`.
   - `/nix/var/nix/daemon-socket/socket` is a socket owned by root, and
     `/nix`, `/nix/var`, `/nix/var/nix` and `/nix/var/nix/daemon-socket`
     are directories owned by root, writable by neither group nor others.
     A symlink, another owner or a writable ancestor:
     `unsupported_backend`. The socket absent from a multi-user layout:
     `backend_unavailable`. Only root can then have bound or replaced the
     socket, which is what makes the pin below mean something.
3. **Filesystem.** The device of `/nix/store` must be the requested
   filesystem's, otherwise `other_fs`. Matching is by device: on NixOS
   `/nix/store` is a read-only bind mount of the root filesystem (same
   device, its own mount).
4. **Connection.** diskgc pins the socket (`O_PATH | O_NOFOLLOW`), checks
   on that fd that it is a socket owned by root with the device and inode
   of step 2, then connects, sends the worker protocol's opening magic
   number and waits, at most 10 s, for the daemon's magic answer, then
   closes (a read-only query on `Host`). The answer proves a daemon is
   serving and accepted diskgc's user. `ECONNREFUSED`, `EACCES`, end of
   file before the answer (the daemon refused the user under
   `allowed-users`, or a socket-activated service failed to start) and the
   time bound: `backend_unavailable`, the detail saying which. Another
   errno or a wrong answer is an error. A bare connect is not enough:
   under systemd socket activation (NixOS, this development machine)
   systemd accepts the connection even when the daemon cannot start.
   This comes before any Nix command, so these cases are classified here
   and not by parsing a client's error message. The probe relies on
   nothing after the two magic numbers, which open the worker protocol in
   CppNix (`WORKER_MAGIC_1`, `WORKER_MAGIC_2`); each admitted daemon
   version is checked for them by the container scenarios.
5. **The caller's store selection.** The variables that locate a store
   without showing in the `store` setting must be unset, empty or their
   default: `NIX_STATE_DIR` (`/nix/var/nix`), `NIX_STORE_DIR` and
   `NIX_STORE` (`/nix/store`), `NIX_DAEMON_SOCKET_PATH`
   (`/nix/var/nix/daemon-socket/socket`); otherwise `unsupported_backend`
   without running Nix. They redirect `daemon` and `auto` (observed for
   `NIX_DAEMON_SOCKET_PATH` on 2.34.8; `auto` takes a local store
   whenever its state directory is writable, `resolveStoreConfig`). Then `nix config
   show store` runs in diskgc's own environment: it reflects `NIX_REMOTE`,
   `NIX_CONFIG` and the user's `nix.conf` and opens no store (observed:
   exit 0 for `ssh-ng://…` and `local?root=/nonexistent`). The value must
   be `auto`, `daemon` or `unix:///nix/var/nix/daemon-socket/socket`.
   `auto` then selects that daemon: with the default state directory,
   which step 2 found writable only by root, and diskgc not root, Nix
   2.34.8 picks the daemon socket when it exists. Values
   are compared in Nix's normalised form (it prints
   `local://?root=/x`); remote schemes (`ssh`, `ssh-ng`, `http`, `https`,
   `s3`, `gs`) are `remote_backend`, every other value
   `unsupported_backend`. A failure of this command is an error. A user
   who points Nix elsewhere is not collected behind their back, and the
   preflight never connects to the store the user selected.
   Then `nix store info --json --store unix:///proc/self/fd/<N>` runs with
   the child environment below, through the pinned fd, and gives the
   daemon's version and whether diskgc's user is trusted.
6. **Versions.** `nix-store --version` must print exactly
   `nix-store (Nix) <version>` and one newline, with the version on the
   client list, and the daemon's version must be on the daemon list
   ([reference.md](reference.md#backend-version-allowlist)). Anything else,
   including `(Lix, like Nix)` and `(Determinate Nix …)`:
   `unsupported_backend`, the detail naming both versions.

A tier that passes is bound: its plan row has candidate `/nix/store`,
`estimated_bytes: null` and the operation text. A dry run stops here; it
cannot run step 8, so a reclaim can still skip a tier the plan showed
eligible.

**Immediately before the GC** (reclaim only, under the host lock, when the
tier's turn comes and the goal is not yet met):

7. **Recheck.** Pin the socket again and compare device, inode, owner and
   type with preflight; the client directory and the pinned query's daemon
   version must be unchanged. Any difference: `changed`,
   nothing run.
8. **Root database.** diskgc lists `/nix/var/nix/profiles` and each
   `/nix/var/nix/profiles/per-user/*` (one level each, at most 4,096
   entries per directory, no symlink followed) and takes every generation
   link: a symlink named `<profile>-<N>-link` whose text is an absolute
   path directly in `/nix/store` that exists. None: `unsupported_backend`
   (nothing to check the daemon against). More than 1,024 generation links
   in total, or a directory over its entry bound: `unsupported_backend`
   too, rather than a sample, the detail naming the bound so the operator
   can delete old generations (diskgc never does). It then runs
   `nix-store --store unix:///proc/self/fd/<N> --query --roots <targets>`
   and requires each generation link to appear exactly as
   `<link> -> <target>`. Nix reports a root under the path where its own
   root discovery found it, below its own state directory's `profiles`
   (`findRoots`, `findRootsNoTemp` in 2.34.8), so a daemon with another
   state directory reports other paths. A missing one:
   `unsupported_backend`, the detail naming the generation the daemon does
   not count. What this establishes is the property the guarantee needs:
   every system and per-user generation diskgc can see is a root of the
   daemon that will collect. It does not identify the state directory
   itself: a daemon whose own `profiles` held, for each of those
   generations, a symlink to the system's link would report the same
   lines (a root-made configuration; then those generations are roots
   for it too, but its other roots are not the system's). This is a
   gated deleting run, not a query: Nix's root discovery unlinks
   registrations in `gcroots/auto` whose out-link no longer exists
   (TASK-98), which the GC that follows does too, but a dry run must not.
   A generation removed between the listing and the query fails the
   check, closed. It repeats the GC's root discovery, including the
   runtime-root scan of `/proc`, so its time counts against the tier's
   `timeout` before any byte is freed; the container scenarios report it.
9. **GC.** `need` = goal − a fresh sample of available space, at least 1
   (Nix deletes nothing with `--max-freed 0`). The child is the canonical
   client with the constant argv above, in its own process group, cwd `/`,
   holding the inherited host lock, its output on stderr with the tier and
   profile prefix. Its exit, signal or timeout is recorded; a nonzero exit
   or a signal is an error of the action.
10. **Measure.** A fresh sample gives the action's `avail_delta_bytes`; the
    tier is done after one GC (Overshoot below).

**Child environment** (the pinned queries of steps 5 and 8, and step 9):
exactly `HOME` (diskgc's) and
`NIX_USER_CONF_FILES=` (empty: no per-user configuration file). Clearing
the rest removes `NIX_REMOTE`, `NIX_CONFIG`, `NIX_STORE_DIR`,
`NIX_STATE_DIR`, `NIX_DAEMON_SOCKET_PATH` and `NIX_CONF_DIR`, so only the
root-owned `/etc/nix/nix.conf`, which the daemon reads too, and the command
line apply. A trusted user's connection applies settings the client
forwards; whatever is forwarded then comes from that system file and the
constant command line, never from the user's environment or files. Which
settings 2.34.8 forwards from a system file is not verified; the
container scenarios run both an untrusted and a trusted diskgc user. On Nix 2.34.8, `--store
unix:///proc/self/fd/<N>` with an `O_PATH` socket fd connected to the
daemon and won over a conflicting `NIX_REMOTE` and `NIX_CONFIG` (host
check, TASK-118 notes); the daemon logs that it ignores the forwarded
`store` setting from an untrusted client, which is harmless.

**What the binding proves:** the GC is run through the inode at the fixed
socket, which only root can have bound, by a serving daemon that accepted
diskgc's user and that the caller's own Nix selects; client and daemon
are allowlisted CppNix versions; that daemon, moments before the GC,
counted every generation link diskgc sees under `/nix/var/nix/profiles`
as a root; and diskgc's `/nix/store` is on the requested filesystem.

**What it does not prove, stated plainly:**

- **A second root database over the same physical store.** A container's
  own daemon sharing the host's `/nix/store`, or root running a
  `local?state=…` store over it, can register paths there that the bound
  daemon considers dead; its GC collects them, exactly as
  `nix-collect-garbage` or `nix.gc.automatic` on the same host would. An
  unprivileged client cannot see another database. Deployment rule: one
  root database per physical store. The evidence for this class is below.
- **The daemon's physical store directory.** Nix collects under its
  `realStoreDir`, which an unprivileged client cannot see. A daemon
  configured with `real=` elsewhere would collect there; the effect on the
  requested filesystem is measured, not assumed.
- **One daemon from step 8 to step 9.** The root check, the GC and every
  query are separate connections; under socket activation, root can
  restart the daemon with another configuration between them without
  changing the socket inode or the version. Only speaking the worker
  protocol itself would put the check and the GC on one connection; that
  cost is not taken. This is a root-made change in a window of seconds.
- **The state directory's identity**, beyond the generations (step 8).
- **Runtime roots outside the daemon's PID namespace.** Nix protects store
  paths open or mapped by processes it can see; a process in another PID
  namespace that uses a store path without a root is not protected. This
  is Nix's limit, the same for its own GC.

### GC roots, profiles and generations are never removed

- diskgc removes nothing in the store or under `/nix/var/nix` itself. Its
  Nix commands are a closed set in `LinuxHost` (the version and store-info
  queries, `--query --roots`, `--gc --max-freed`), mapped from an enum and
  covered by the source scan, so no profile or configuration adds one. Not
  in the set: `--delete`, `--ignore-liveness` (the 2.34.8 daemon refuses
  it from every client anyway), `nix-collect-garbage` and its `-d` /
  `--delete-older-than`, `nix-env --delete-generations`, `nix profile
  wipe-history`. Removing generations or profiles is a non-goal (PRD).
- Generations are links in the daemon's `profiles` directory, which Nix's
  root discovery scans, so neither they nor their closures are dead. Step 8
  is what makes that the system's `profiles` directory.
- Nix's root discovery (step 8 and the GC) unlinks `gcroots/auto`
  registrations whose out-link is gone; they root nothing. Out-links that
  still exist stay roots; removing stale ones is TASK-98's protocol, not
  this kind.

### `--max-freed` and the free-space goal

- **A lower bound on Nix's own count.** Nix deletes dead paths in the
  order `readdir` returns the store directory (on ext4 an order by a hash
  of the name; never by age), and after
  each path stops if its count exceeds `--max-freed`. With enough garbage
  it frees at least `need` by its count, plus the rest of the last path.
- **Nix's count is not available space.** It adds the apparent size of each
  deleted file with link count 1 or 2 (2 is assumed to be the `.links`
  copy) and nothing for 3 or more; small files take more blocks than
  their size; compression and a store elsewhere make it overstate; the
  final `.links` phase also frees unused links left by earlier, interrupted
  GCs, which nobody counted. Only the measured sample decides.
- **Overshoot is accepted:** the rest of the last path plus the
  undercount. Everything deleted was dead to Nix; the cost is rebuild or
  download time, not data. **Undershoot** (Nix's count reached, space not)
  moves on to the next tier: one GC per tier per reclaim, because a second
  repeats root discovery and the `.links` scan for what the first left.
- Nix's own `min-free` / `max-free` auto-GC is independent; Nix's global GC
  lock serialises both.

### Freed space lags in an optimised store

With `auto-optimise-store`, or after `nix store optimise`, a dead path's
files are hard links into `/nix/store/.links`. Deleting the path frees
their blocks only in Nix's last phase, which scans all of `.links` and
unlinks entries with link count 1, after the deletion loop and also when
`--max-freed` stopped it. Space therefore appears near the end of the GC,
not path by path. diskgc samples after the child exits, so the lag is
inside the action; a GC terminated before that phase leaves those blocks
allocated until a later GC completes it, and the report shows the small
delta. On the development machine (2026-10-24) `.links` held 1.6 million
entries, and a read-only `lstat` of all of them took 4.6 s with a warm
cache, although `auto-optimise-store` was off at the time. Nix notes its
own race in that phase with concurrent optimisation (a link count read as
1 just before `optimisePath` raises it).

### Host lock, deadlines and signals

- The GC runs under the host lock; the child inherits its fd
  ([Host lock](#host-lock)), so a SIGKILLed diskgc leaves the lock held
  until the client exits.
- The work happens in the daemon's worker, not in the child. When the
  client goes away, the 2.34.8 daemon notices the hang-up and stops at its
  next interruption check (per store entry and per file deleted), usually
  promptly, but diskgc does not wait for it: a following reclaim may start
  while that worker finishes. Its GC then waits for Nix's exclusive GC
  lock, so two collections never overlap.
- A GC can take minutes (root discovery including runtime roots from
  `/proc`, liveness per path, deletion, the `.links` scan). Deadlines are
  cooperative ([Deadlines](#deadlines)): at the tier's or the run's
  deadline the client's process group gets SIGTERM, then SIGKILL after
  10 s; SIGINT and SIGTERM to diskgc do the same and exit 130 / 143. The
  action is reported with its measured delta.
- What an interruption leaves: deleted paths were dead; Nix invalidates a
  path before deleting its files, so one interrupted mid-deletion is an
  invalid directory the next GC removes; unused `.links` entries wait for
  the next completed GC. No live path becomes collectable because of an
  interruption. A tier `timeout` shorter than a whole GC frees little in an
  optimised store.

### Failure modes

| Situation | Outcome |
|---|---|
| No `nix-store` on `PATH`, or no `/nix/store` | skip `missing` |
| Client not a CppNix multi-call store path; single-user layout; symlinked or writable socket ancestor; `NIX_STATE_DIR`, `NIX_STORE_DIR`, `NIX_STORE` or `NIX_DAEMON_SOCKET_PATH` not the default; another local store selected; unlisted version or variant; a generation the daemon does not count as a root, none to check, or too many | skip `unsupported_backend` |
| Remote store selected (`NIX_REMOTE`, `NIX_CONFIG`, `nix.conf`) | skip `remote_backend` |
| `/nix/store` on another filesystem | skip `other_fs` |
| Daemon socket absent; connection refused or `EACCES`; no magic answer within 10 s or end of file first (daemon not started, user refused by `allowed-users`) | skip `backend_unavailable` |
| Socket inode, client or daemon version changed between preflight and GC | skip `changed` |
| `nix config show` or a pinned query fails; a wrong magic answer; unparseable output; host I/O error; sandbox gate | error |
| GC exits nonzero or by a signal | error, action recorded |
| Deadline or SIGTERM during GC | 124 or 143, action recorded with its delta |
| GC frees less than need | next tier; exit by the final sample |
| `--allow-root` with a `nix-store` tier | exit 2 |

### Evidence: why URL and store-directory checks are not a binding

`NIX_REMOTE` or the `store` setting can point `nix-store` at another
store, and `nix store info --json`'s `url` and `builtins.storeDir` do not
identify the root database. On Nix 2.34.8, two direct local stores with
distinct `NIX_STATE_DIR` values both report `url: "local"` and
`/nix/store`, although their state directories hold different roots;
`builtins.storeDir` still says `/nix/store` when `NIX_REMOTE` points
elsewhere. A disposable Nix 2.34.8 container showed the consequence:
`--store 'local?store=/nix/store&real=/tmp/real&state=/tmp/state-a'`
overrode conflicting `NIX_REMOTE` and `NIX_CONFIG` for both the root query
and `nix-store --gc --max-freed 1`, which preserved a root in state A and
removed an unrooted path. State B then recorded that same physical path as
valid without a root in B; collecting through `state=/tmp/state-b` removed
it despite its root in A (TASK-36). Hence the protocol above binds a
daemon rather than a URL, takes the store selection out of the child's
environment, and checks the daemon's roots against the system's
generations. Nix documents `real` and `state` as separate settings for the
[local store](https://nix.dev/manual/nix/2.34/store/types/local-store) and
[local daemon client](https://nix.dev/manual/nix/2.34/store/types/local-daemon-store.html).

Verified on Nix 2.34.8 (2026-10-15): an untrusted, non-root user can
delete dead store paths through the daemon (`nix-store --delete` on a
freshly added path); `--gc --max-freed` uses the same daemon operation but
has not been run on the development machine, so as not to delete its real
garbage. The GC semantics above (`--max-freed` check, deletion order,
byte count, `.links` phase, hang-up interruption, the daemon's refusal of
`--ignore-liveness`) are read from the Nix 2.34.8 sources (`gc.cc`,
`local-store.cc`, `unix/file-system.cc`, `daemon.cc`) and are to be
confirmed by the container scenarios.

## Command actions

### Rustup and Elan toolchains (unshipped)

There is no built-in `rustup` or `elan` toolchain profile. A generic
`command` profile is not a safe substitute: it can pass a candidate path to
an executable, but `rustup toolchain uninstall` and `elan toolchain uninstall`
take manager names. An Elan name such as `leanprover/lean4:v4.1.0` is stored
as `leanprover--lean4---v4.1.0`; a linked toolchain can instead be a symlink.
The candidate directory name alone is not an authoritative uninstall target.

The managers also cannot certify all pins from their own metadata. A project
root supplied to diskgc may contain `rust-toolchain`, `rust-toolchain.toml`,
or `lean-toolchain`; another project or a tool (for example, Kani) may pin a
toolchain outside those roots. Rustup also has defaults and directory
overrides. The [Rustup override rules](https://rust-lang.github.io/rustup/overrides.html)
describe the distinct precedence of the environment, directory overrides,
project files and defaults. Elan's `toolchain gc --help` in 4.2.1 says a
project-file pin counts only after Elan has been used in that directory.
Its [4.2.1 changelog](https://github.com/leanprover/elan/blob/v4.2.1/CHANGELOG.md)
states the same limit. Neither manager can certify an unseen tool-created
pin. A configured-root scan can narrow this uncertainty but cannot erase it.

In an offline, disposable `diskgc-e2e-base` container, Elan 4.2.1 saw a
valid `lean-toolchain` in an unvisited `/tmp/project` naming an installed
`leanprover/lean4:v4.1.0`, yet `elan toolchain gc --json` reported that
toolchain in `unused_toolchains`; `gc --delete` uninstalled it. In a second
disposable container, Rustup 1.29.0 accepted `toolchain uninstall custom`
after `default custom`: it warned that it was removing the default and then
removed it. Both checks used fresh manager homes under `/tmp`, no network,
and a read-only `/nix/store` bind. They demonstrate that a zero-exit manager
cleaner does not prove preservation of defaults or pins.

A shippable toolchain source needs a manager-backed identifier tied to the
candidate, a complete pin inventory, explicit authorization of each
uninstall target, and a recheck while the candidate lock and host lock are
held. A default or pinned toolchain must be kept even when its name was
authorized. The recheck must include defaults, directory overrides, every
configured project root, linked toolchains and declared tool-created pins;
unknown or unreadable pin state must keep the candidate. Its manager binary
and home must be bound at preflight and at action time. TASK-94 tracks this
work. Its real-machine dry-run acceptance criterion remains unverified under
the current instruction to test only on fake filesystems and in containers.

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
- **Configured executables** are resolved at run time, in the preflight. The
  first executable uses diskgc's `PATH`; an `env` wrapper's inner executable
  uses any `PATH` assignment in its arguments. Their resolved paths are logged
  to stderr before the command runs (for `[env, GOCACHE={path}, go, ...]`, this includes
  `go` as well as `env`). The resolved `go` path is passed to `env`, so its
  lookup cannot select a different tool after preflight. Programs launched
  later by the configured tool are outside this preflight.
  A tool that is not installed makes the profile `missing` (skipped),
  not an error.
- Runs in its own process group; killed after the tier's `timeout`;
  inherits the host lock (below).

The deleting child-start port takes the already checked working-directory fd,
an absolute executable from preflight, argument and environment arrays, and
the tier's monotonic deadline. It changes directory with `fchdir` in the
child, so a replacement at the path cannot redirect the working directory.
At the deadline it sends `SIGTERM` to the process group, waits 10 s, then
sends `SIGKILL` to the group and reaps the leader. It keeps the leader waitable
during that interval so the process-group ID cannot be reused. A descendant
can close both output pipes and outlive its leader, so early pipe closure
does not shorten the grace period. The host lock fd has close-on-exec cleared
before starting the child; descendants inherit
it too unless the cleaner explicitly closes it.
On normal leader exit, the port still kills any surviving group members
before reaping the leader, including descendants that closed both output
pipes. The waitable leader pins the group ID until that cleanup is done.

The child's stdout and stderr are read through separate nonblocking pipes and
forwarded to diskgc's stderr with the action label on every line. An output
line longer than 4096 bytes is emitted in 4096-byte segments, each with its
own prefix, so even a child that never writes a newline cannot grow diskgc's
output buffer without bound. The action label is limited to 256 bytes, so a
forwarded segment also has a fixed maximum size. Failure to read or forward
output fails the action; if the group leader is still held, diskgc kills its process group
before returning the error. The result on diskgc's stdout is never mixed
with child output. One process-wide writer thread forwards complete segments
through close-on-exec duplicates of stderr. No inherited stderr flags are
changed, and no stdio mutex is held during a write. Its zero-capacity channel
holds no queued segments: a stalled consumer can block at most that one
writer thread, while the action fails after at most 50 ms of backpressure
(or at the earlier tier deadline) and kills its child group. Later actions
fail to hand off output while the writer remains blocked; they cannot start
more blocked writers. A leader-exit drain stops at the tier deadline, even
when the output pipes remain open.

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
   and opening (refused, not retried). A final symlink at the mount point
   can make `O_DIRECTORY | O_NOFOLLOW` report `ENOTDIR`; a no-symlink stat
   distinguishes it from a missing or non-directory path so it is also
   `unsafe_path`.
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
We do not use a direct `statx` on the full path to find the mount: that
would follow symlinks in intermediate components.
The root-relative `statx` host primitive first opens with
`RESOLVE_NO_SYMLINKS`, then stats the held fd; it reports a final symlink
as a symlink but refuses intermediate symlinks with `ELOOP`.

The walk and the deletion descend with
`openat2(... RESOLVE_NO_XDEV | RESOLVE_NO_SYMLINKS)`; `statx` results are
trusted only when the kernel says it answered them: each field only when
its bit is set in `stx_mask`, each attribute only when its bit is set in
`stx_attributes_mask`. A check the kernel cannot answer skips the candidate
(`unsupported_fs`).

A readable `openat2` uses `O_NONBLOCK | O_NOCTTY`. If a FIFO appears at
the name before the caller checks its type, opening it cannot wait for a
writer; opening a terminal cannot acquire it as the controlling terminal.
These flags do not make arbitrary device opens free of side effects. In
the built-in filesystem and Cargo reclaim paths, `openat2` requests a
readable fd only with `O_DIRECTORY`; lock and marker files are opened
`O_PATH`, checked as regular files, and reopened through that fd.

Minimum kernel: Linux 5.8 (`STATX_MNT_ID` and `STATX_ATTR_MOUNT_ROOT`;
`openat2` is 5.6); older kernels exit 2 at startup. Every command checks it
except `--version`, `--help`, `check` and `profiles`, which do not inspect
candidates or processes. The check reads `major.minor` of the `uname`
release, so a vendor kernel numbered below 5.8 is refused even if it has
these features backported, and an unparseable release is refused too.

## Validation

- **Static configuration** (`check` and `ensure`): YAML syntax, schema,
  unknown keys, `version`, unique tier and profile names, absolute paths,
  normalized and unique roots, nonzero `idle_for`, no `~`, duration syntax
  in the config, the selection grammar
  (qualifiers, one source per term, no unparenthesised mixing, `!` only
  narrows), one kind and one action per expression, predicates supported by
  the kind, roots fixed vs required, `protect` entries, `override:` on
  shadowed built-ins, `owners:` list shape, and profile `examples`. `check`
  needs nothing from the deployment machine, so it runs in a build sandbox
  and when cross-building.
- **Static invocation** (`ensure`, before planning or deletion): size and
  command-line duration syntax; the same selection grammar and root binding
  for an explicit `--select` and its `--roots`; compatibility of `owners:`
  with `--allow-root`; and command actions refused under `--allow-root`.
  `check` has no space goal, `--select`, or `--allow-root` option, so it
  cannot validate those invocation-specific rules. Failure in either static
  phase: exit 2, nothing deleted.
- **Supplied roots exist** (`ensure`, after the static phases, before
  planning or deletion, also in a dry run and when space is already met):
  every root the operator supplied (tier and `protect` `roots:`,
  `--roots`) exists; one that does not is exit 2 naming it and where it
  was written (TASK-152), because a typo or an unedited example root
  otherwise reads as one skipped candidate and exit 0. Mounts first: a
  root whose mount chain is refused from mountinfo (FUSE, network, an
  untriggered automount, PrivateTmp, unclear) is not opened by this check
  and is not "missing"; the later phases refuse it without touching it
  (TASK-178: resolving one on a dead FUSE mount hung `ensure`). `check`
  makes the same decision with the same code (`invocation::check_roots`)
  and warns about each missing root and each root on a refused mount,
  without changing its exit code. Roots a profile fixes itself are not
  held to this: their absence is normal (`missing` below).
- **Environmental** (at the start of every reclaim, for *all* tiers, before
  the first deletion): roots and paths exist, contain no symlinks, resolve
  to a device and mount; `owners:` names resolve to non-system users;
  executables of command actions resolve; Docker and Nix backends are bound
  and reachable.
  - A missing path or tool, another filesystem, or a remote Docker endpoint
    is normal: that tier is skipped with its reason (`missing`, `other_fs`,
    `remote_backend`). A named Docker context is `unsupported_backend`.
  - An unsafe tier root (`unsafe_path`) excludes that tier and is reported
    as an error (exit 3 if the goal is then not met). The other tiers
    still run: refusing all cleanup because one tier is broken would let
    the disk fill.
  - A Docker or Nix backend follows one rule, defined in
    [reference.md](reference.md#backend-skips-and-errors): a host that
    does not offer a supported, reachable backend is a skip with its
    reason (including `unsupported_backend` and `backend_unavailable`, a
    daemon installed but not reachable); with no other error and the goal
    unmet, the run is exhausted (exit 1). Any other backend failure is an
    error.

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

  A reclaim that needs a missing lock exits 2 (JSON `cause:
  provisioning`), printing those two lines in the commands that install
  them in `/etc/tmpfiles.d` and apply them, and the `mkdir`/`touch`/`chmod`
  equivalent for a host without systemd-tmpfiles.
  An `ensure` whose space goal is already met checks the same object
  without taking a `flock`; missing or unusable provisioning warns on
  stderr but preserves exit 0 and the `met` report. The file is reached
  like any configured path (no symlink, no mount crossing: Path
  traversal), opened `O_PATH`, checked to be a regular file and reopened
  read-only through that fd, so a FIFO or a device at the path is never
  opened; anything but
  a regular file there is exit 2 when reclaim is needed.
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
- A lock goes when the last reference to its open file goes, which can be
  after the holder's `close()` or exit has returned: a reader of
  `/proc/<pid>/fd` or `fdinfo` (an in-use scan, `lsof`) holds a reference
  to the file for the length of its syscall, and when its reference is
  the last, the lock is released as that syscall returns. Which reads
  take one (kernel `fs/proc/fd.c`): reading `fdinfo` on every supported
  kernel; statting or reading an fd link from 5.11 (`fget_task` in
  `proc_fd_link`; 5.8 to 5.10 take only a path reference); listing the
  fd directory from 6.7. diskgc's scan lists and stats fd links, so it
  does this from 5.11 on. Measured on Linux 7.2, where listing counts
  too: up to 0.5 ms, and a single `flock` tried right after the close
  found the lock busy in about 1 of 60 attempts under four concurrent
  readers; that rate was not measured on older kernels, where fewer reads
  take a reference. On 5.8.16, listing and statting fd links gave no late
  release in about 1.7 million attempts, as the source predicts; adding
  `fdinfo` reads gave one (in one run, after 772 attempts; the
  kernel-fact test, docs/testing.md). A waiter's next poll takes it;
  tests wait for a release with a bound instead of trying once
  (docs/testing.md). The same holds the other way: while diskgc scans
  (5.11 on), any program on the machine whose lock's last fd closes can
  see that lock outlive the close by one such syscall, so a `flock -n` retried at
  once (a cron job, a single-instance check) can briefly find it busy, as
  under `lsof`. diskgc's correctness does not depend on it.
- Waiting for the lock (until the deadline) retries a non-blocking
  `flock` every 10 to 100 ms rather than blocking in the kernel, so the
  deadline holds without signals. Limits: after the holder releases, the
  waiter can take up to 100 ms to notice, and waiters are not served in
  order: one that retries at the wrong moments can lose to later arrivals
  until its deadline.

## Pre-release sandbox gate

The test-only clock offset uses the same two markers. In a development
build, `LinuxHost::new` reads `/.diskgc-test-clock-offset` only after the
marker check passes. The optional file contains positive decimal seconds
plus one newline, bounded to 366 days; malformed files, symlinks, FIFOs and
read errors fail host construction. The offset is fixed for the process and
advances only the wall-clock input used for age and idleness. It cannot
change `CLOCK_BOOTTIME` deadlines or kernel timestamps. A development build
outside a marked container ignores the file. The markers are checked once,
when the host is constructed: removing them later in the same process does
not drop the offset (the gate likewise checks once, at the first deleting
operation), so the offset is only as trustworthy as the container that
passed that check. The
`release-without-sandbox-gate` feature compiles out the offset; the
`release-excludes-clock-offset` check tests the separate 0.1 candidate
package against a development positive control, for each of four traces:
the offset path, the reader's error context and parse diagnostic, and the
wall-clock overflow message. It is a byte-string check: it would not see an
equivalent offset written without those strings. The default Nix package
remains gated. The `release-01-candidate` output is test-only and unapproved
for deployment until TASK-67 closes. Its /tmp catalog is `nix-shell-rc` and
`pytest-tmp` (TASK-65); operator policy and deployment evidence remain
unresolved.

In development builds, diskgc's deleting primitives (unlink, rmdir, the
delete walk, and starting prune, GC or command children) refuse to act
unless they run inside a test container, so every entry point, including
test drivers, is covered: both the image marker `/.diskgc-test-sandbox` and
Podman's `/run/.containerenv` must exist (the same markers as the test
interlock, see [testing.md](testing.md)). `check`, `survey`, `profiles`,
`--dry-run` and `--tally-only` work anywhere. The gate is compiled in by
default and removed only by release packages, from the 0.1 `ensure` MVP
onwards (PRD, Scope; a dedicated build flag set in one place, by the release
task), so it cannot be dropped
by accident during development, and developers and AI agents cannot
delete real data on their own machines with a work-in-progress build.
The binary does not compile with the gate removed unless it also has the
release catalog (`proposed-release-01-catalog`; a const assert in its
`main.rs` on `diskgc_linux::sandbox::SANDBOX_GATE`), so an ad-hoc
`cargo build` cannot make an ungated binary with every development profile.
The `release-gate-needs-catalog` flake check proves both halves: the
gate-lift alone fails on that assert, and with the catalog it compiles. The
assert guards what ships, the `diskgc` binary; it does not stop test
builds of other crates with `--all-features` (there,
`the_gate_is_compiled_into_this_build` fails instead) or the separate
`diskgc-e2e` project, whose binaries have their own container interlock.

How it holds:

- **Where:** `LinuxHost`'s `Delete` methods (`unlinkat`, and `fchmod` for
  `make_writable`: changing permissions of user data is gated like
  deleting it) ask the gate before anything else; a refusal is a host error of its own kind
  (`Cause::SandboxGate`: no errno, the detail names the marker that is
  missing or wrong) and nothing is attempted. `ModelHost` models it
  (`sandbox_gate: open | closed | refuses_deletes`, open by default; it
  deletes nothing real and is test-only). A refusal during a reclaim, after
  `may_delete` answered open, is an error of the run that stops it (exit 3
  unless the goal was met): only the model can produce it, since
  `LinuxHost` decides once and keeps the answer.
- **Structure:** each gated syscall wrapper in `diskgc-linux` (one for
  `unlinkat`, one for `fchmod`) takes a permit that only the gate can make. A source-scanning test
  (`diskgc-linux/src/source_scan.rs`) reads every production crate (every
  crate in `crates/` except the test-only `diskgc-model` and
  `diskgc-modelgen`, which may never be production dependencies):
  - outside an allowlisted wrapper, none may delete or rename a name, open
    for writing or truncating (also `File::create`, `File::create_new`,
    `File::options`, `OpenOptions`, `fs::write`, `fs::copy`, `set_len`,
    flags built from numbers), punch holes, change permissions (`chmod`,
    `fchmod`, `chmodat`, `set_permissions`), start a child process
    (`process::Command`, `fork`, `exec*`), change namespace or root, use
    `libc`, call `syscall`, or `include!` code. Each wrapper is marked
    gated (it must take the permit) or ungated with its reason; adding one
    is a review point. The core's calls of the `Delete` methods are named
    the same way, so the allowlist also makes the deletion engine's
    `checked_unlink` (the identity check right before the unlink) the only
    caller of `unlinkat`, and its `make_writable` the only one of `fchmod`;
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
- **Fail fast:** a reclaiming `ensure` asks for the gate's decision
  (`Delete::may_delete`: the same cached decision, no permit made) before
  it measures, locks or plans, so outside a test container a development
  build exits 2 naming the gate, even when the goal is already met.
  `--dry-run` is given only a `Host`, so it neither asks nor could delete.
- **Once per process:** the gate decides at the first deleting call (or
  question) and keeps the answer, a refusal included; the markers do not change while
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
  - No test inspects the shipped binary for the gate; the configuration
    is checked, and a test runs the built binary's reclaiming `ensure`
    outside a container on a fixture of its own and expects the refusal
    (`a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate`).

## Privileges

- diskgc refuses to run as root (uid 0) unless `--allow-root` is given;
  the laptop service runs as the owner, CI as the runner user. `profiles
  list` and `show` are exempt: they only read the configuration and
  print, and run no command, so an agent in a root container can still
  learn the profiles. As root they also show a configuration that `ensure
  --allow-root` would refuse: the ownership checks below are not made.
- Docker requires the `docker` group (root-equivalent; Docker's model, not
  ours). Nix GC goes through the Nix daemon as a normal, untrusted user;
  the daemon's `allowed-users` must admit it. A trusted user is
  root-equivalent in Nix's model; diskgc's Nix argv is the same either
  way.
- With `--allow-root`: an `owners:` list is required, command actions are
  refused, and the config file and every directory above it must be owned
  by root and not writable by group or others (like sshd's StrictModes).
  Both the lookup path and the canonical target of each symlink are checked;
  a link into a user-writable tree is refused. Owner names resolve through
  the platform name service at invocation time; explicit `{uid: N}` entries
  avoid that lookup. UID 0 and system UIDs below 1000 are refused. The
  static musl build can resolve local passwd users (and nscd if available),
  while NSS-only LDAP/sssd names are unavailable without nscd; a protected
  policy may instead pin their trusted numeric UIDs. UID reuse changes who
  the policy names and must be controlled by the deployment.
  Docker prune actions are refused too: the daemon's shared cache has no
  per-entry owner check for `owners:` to enforce. So is a `nix-store`
  tier (exit 2), for the same reason and because root's Nix bypasses the
  daemon ([`nix-store`](#nix-store)).

## Deadlines

`--timeout` and per-tier `timeout`s are **cooperative**: diskgc checks the
deadline between operations (each walk step, each unlink, each child wait)
and, at the deadline, starts no new work and terminates a running child
(SIGTERM, SIGKILL after 10 s). A single filesystem operation that blocks in
the kernel (a hung network mount, a disk in error recovery) cannot be
interrupted by a signal and can overrun the deadline; diskgc lists the
overrun in the report's `overruns` when it returns. Network filesystems are never candidates, which
removes the commonest cause, and a mount point is never `statx`ed by
discovery or the judgement of a candidate (Before the walk). The in-use
scan checks the deadline and signals before every listing (In use). What can
still block: a process using a dead mount hangs the in-use scan (In use),
and a dead mount made after the mount table was read (Deletion, accepted
limits). A supervised worker that the caller could
abandon while it stays blocked is not part of v1: it would have to keep the
host lock held while abandoned, which moves the problem rather than solving
it.
