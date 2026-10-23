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
  current filesystem state immediately before acting.
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
    a known Cargo lock file name (`.cargo-lock`, `.cargo-build-lock`,
    `.cargo-artifact-lock`, the allowlist of `cargo-target` below),
    whatever directory holds it: no Cargo lock file is ever deleted, also
    when the candidate is a profile directory or lies inside a target;
  - nothing inside matches diskgc's fixed **structural recogniser** for
    Cargo targets: a directory with `CACHEDIR.TAG` whose profile
    directories (`<t>/<p>/` or `<t>/<triple>/<p>/`) hold any file matching
    `.cargo-*lock*`, known or unknown name alike. `<t>` is the candidate
    itself or a directory inside it; any entry named `CACHEDIR.TAG` counts
    (its signature is not read, which errs on the side of keeping);
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
    settings. Protected matches are enumerated before reclaim and rechecked
    under the candidate lock and immediately before deletion. Changes in a
    separately writable protected tree can still race between that scan and
    deletion; this is not an atomic cross-tree exclusion.
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
- **Not excluded** by overlap with a negated profile
  ([reference.md](reference.md#protect)). Negated profiles are enumerated
  before the first deletion and checked again under candidate locks;
  incomplete enumeration excludes the tier.

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
    skipping every remaining candidate.

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
The embedded `containers-image-tmp` source has a fixed three-day idle floor,
but that floor cannot prove a producer has finished with its spool. A
disposable VM test gives an old `/var/tmp/buildah*` directory genuine old
kernel ctime, then parks a live producer after it closes the spool. The
exact built-in judges the directory eligible while the producer still
needs it on resume. The test only dry-runs that live directory. The
producer is a fixture; whether real Buildah or Podman ever reuses a dormant
spool with this lifetime remains unverified. No liveness contract for such
reuse is currently available, so this profile needs that lifecycle evidence
before its safety can be claimed for a release.
An idle-time filter cannot establish liveness. The rc names
`nix-shell-<pid>-*` and `nix-develop-<pid>-*` also cannot use a host-only
`/proc/<pid>` check: a shell can share `/tmp` from a different PID namespace
and put its namespace-local PID in the name. `nix-shell-rc` remains unshipped.
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
When given the XDG cache directory itself as its required root,
`xdg-cache-entry-v2` excludes the top-level cache roots of Brave,
Chrome, Chromium, Edge and Mozilla. The exclusions are relative to that
supplied root. Supplying a root *inside* an application cache directory
defeats this boundary; do not use this broad profile on such a root.

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
listings that each brought new pids the scan is unsettled. The checker
waits 20 ms and makes one complete new scan before judging a candidate.
If that scan is unsettled too, the candidate is `in_use`; the next candidate
starts a new scan immediately. Other blocking scans can be reused for less
than half a second from completion. Listings made
only for re-reads do not count (they
are bounded by the re-reads each process gets) (a process it did not read could have inherited
anything); a machine that keeps starting processes faster than the scan
reads them costs reclaim that way. diskgc's own process is never counted:
it holds the candidate open and locked while it checks it.

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
- any other unreadable process of the user makes every candidate `in_use`:
  one whose reads fail for another reason (also another user's: a failed
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
polling never reports a hang-up. When it reports readable, diskgc checks
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
A task read is bracketed by two `stat` reads through the same held task
directory. The directory inode holds the kernel `struct pid`, so a recycled
numeric tid cannot redirect the second read to another task. A different
start time or a vanished held task discards that round. The start time's
clock-tick resolution is no longer the sole identity check.
A task directory pinned in an unsuccessful round stays pinned for that
process read. If its tid is recycled during a later round, that read may
reach the 16-round bound and treat the process as unreadable; this can
delay reclaim under heavy thread churn.
The listing temporarily holds one directory fd per listed thread; the
task reads also retain one fd per encountered tid until that process read
ends. A highly threaded process can therefore reach `RLIMIT_NOFILE` and
be treated as unreadable, delaying reclaim. This is an availability limit,
not a measured threshold: other open fds and the thread count determine
when it occurs.
The fd-directory owner used to classify an unreadable process is accepted
only when the task is live and the same held task directory returns the
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
   A low file-descriptor limit can therefore prevent reclamation on a busy
   host. A process that appears after the final identity check belongs to
   the check-to-delete race in item 5;
5. the check and the deletion are not atomic: a process that starts using
   a candidate after its in-use check (including a thread that acquires
   files after a round's count) is not seen. The check is kept fresh (see
   Freshness below), not atomic with the deletion;
6. a file handed to a process that was already read (an fd passed over a
   Unix socket, `SCM_RIGHTS`) is a new use of the same kind, and an fd in
   flight in a socket's queue belongs to no process at all: neither is
   seen.

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
deletion starts. A scan is as old as its first read. One scan serves the
candidates checked strictly before 500 ms from its start. The check that
clears a candidate looks at the clock as its last step before the
deletion starts; if its scan is no longer fresh then (a suspend during the
scan or right after it, a slow scan), the candidate is checked again
against a new scan, and if that one is not fresh either, it is an error of
the run, not a skip. Refusing needs no freshness. Buckets
([architecture.md](architecture.md#the-index)) only group candidates for
scheduling; each member gets its own check under this bound, which a scan
less than 500 ms old may answer: a use that starts after that scan
is limit 5 above.

**Visibility:** without root, other users' `/proc/<pid>/fd` cannot be read,
so their use of *your* files is not seen; nor is the use of the user's own
processes noted above as unreadable (setuid, more capable, non-dumpable).
Fine for one-user machines and runners; weaker on shared hosts.

### Producer liveness

Status: designed (TASK-112), not implemented. The protocols below are
normative for the implementation; until it lands, no source can require
them and the profiles that depend on them stay unshipped.

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
[reference.md](reference.md#producer-liveness-predicate-planned); which
built-in has which contract is in
[reference.md](reference.md#liveness-contracts).

Terms. The **producer** is the process whose life bounds the use of a
candidate: it made the candidate and may come back to it. A **consumer** is
any process that reads or writes it. A **contract** is the evidence a
source requires before its candidate may be judged unused, together with
the producer versions and configurations it was verified for.

**Kinds of evidence.**

| Evidence | What it can show | Trustworthy when | Examples |
|---|---|---|---|
| Kernel use: fd, cwd, root, mapping (In use) | in use | always, for processes visible at scan time; silent on dormant ones | every candidate |
| A path in a process's initial environment (In use) | in use | heuristic: an `export` after `exec` is invisible | `TMPDIR` of a running child |
| A `flock` on the candidate directory (Deletion, step 3) | in use | always; its absence says nothing about a producer that never locks | the systemd-tmpfiles convention |
| Producer lock: a lock the producer holds for as long as it uses the candidate | in use, or gone | the producer takes it for its whole use, in every supported configuration | Cargo's `.cargo-lock` files (kind `cargo-target`) |
| Presence marker: a file the producer creates at start and removes when it ends cleanly | present: maybe in use; absent: gone | absent means gone only if every supported configuration creates it; a crash leaves it present | Chromium's `SingletonLock`; pytest's session `.lock` (not when its retention keeps 0) |
| Owner PID: a pid in the candidate's name or in a marker | gone, or maybe alive | diskgc can see every PID namespace (Owner PID, below) and the recorded process lives as long as the producer uses the candidate | `nix-develop-<pid>-<n>`; the text of pytest's `.lock` |
| Producer retention rule: what the producer's own cleanup removes | gone, relative to the producer | only relative: no safer than the producer's own cleanup | pytest keeps its three newest sessions |
| Temp-root users: no process could be using the temporary root the candidate lies in | gone | heuristic: a producer that changed its temporary root inside the process is not seen | pytest sessions in a finished shell's `TMPDIR` |
| Consumer tolerance: every consumer treats a missing entry as a cache miss | no liveness needed | verified per tool | `__pycache__`, `node-compile-cache` |
| A tool's own cleaner, or a code-backed kind | delegated | per tool or kind | `pip cache purge`; the Cargo lock protocol; Nix GC |
| Nothing: a dormant consumer that keeps only a path string | nothing | never | a shell's own `TMPDIR`; `mktemp -d` directories |

**Contract classes and the admission rule.** Every built-in source has one
contract class, recorded in the
[liveness table](reference.md#liveness-contracts):

- **sound**: a "gone" verdict means the producer can no longer use the
  candidate, given preconditions diskgc checks on every run and fails closed
  on. Owner PID, a producer lock, and a presence marker that every supported
  configuration creates are of this class.
- **producer-equivalent**: diskgc removes only what the producer's own
  cleanup, in its default configuration, removes from the same evidence; it
  may act sooner than the producer would. The claim is relative: no worse
  than running the producer.
- **verified heuristic**: the verdict can be wrong in named residual cases.
  Admissible only when each residual is named in the liveness table; the
  loss in each is rebuildable data or one failed, retryable producer run,
  never a user's own work; every known counterexample has a checked-in
  ModelHost vector and, where the kernel's view matters, a container test
  with the real pinned producer; and Mark's decision to admit it is recorded
  in the task.
- **tolerant** and **delegated**: as in the table above, verified per tool.
- **none**: no contract. The profile is not admissible as a built-in, and
  its row in the liveness table records the counterexample. A user profile
  may still select such paths; its author owns that risk.

A contract only narrows: the eligibility walk, ownership, the in-use scan,
the directory `flock` and the checkout guard all still apply. Contracts are
verified against pinned producer versions (`verified_with`); the container
tests pin the observed lifecycle, so a producer upgrade that changes it
(a new retention default, a different rc script) turns them red and the
contract is verified again. The rule applies to every built-in source; the
shipped built-ins that predate it are classified by TASK-122.

`app_not_running` (above) is a presence-marker contract that refuses on any
present lock. It does not use Owner PID: Chromium writes the host name as
well as the pid, and a home directory can be shared between hosts.

**Owner PID.** The engine behind every PID-based verdict. It reverses
the in-use scan's reading of the process list: there a missing process
only means "not seen using it"; here a missing pid is taken as proof that
the owner is gone. So it may conclude *gone* only where the list is
provably complete, and every precondition below fails closed
(`producer_unknown`).

1. *The number.* From the candidate's own name (a protocol fixes where in
   the name) or from a protocol's marker file (pytest's `.lock`). It is
   decimal ASCII, no sign, no leading zero, from 1 to 4194304 (Linux's
   `PID_MAX_LIMIT`). A marker is a regular file directly in the checked
   candidate, opened below its fd without following symlinks, at most 16
   bytes, holding the number and at most one trailing newline. Anything
   else is `producer_unknown`.
2. *Precondition: one kernel.* The candidate's filesystem type (`fstatfs`
   on its fd) is on a fixed list of local types: tmpfs, ext4, xfs, btrfs,
   bcachefs, f2fs. A pid from another kernel (an NFS `TMPDIR`, a directory
   shared with a VM over virtiofs or 9p) names nothing here. Network types
   are already refused as candidates (No network filesystems); the list
   also refuses the VM-shared and unknown ones for this purpose.
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
   `hidepid` (its `mountinfo` super options), or diskgc runs as root with
   `CAP_SYS_PTRACE`. With `hidepid=2` or `invisible`, the kernel hides
   every process diskgc may not ptrace-read, the user's own included: a
   non-dumpable process, or a shell started with `newgrp` or `sg` and every
   child of it (a `nix-shell`, a `pytest`), since the kernel also compares
   groups. Such a producer would never be listed.
5. *Lookup.* The settled in-use scan also reads the `NSpid:` line of each
   listed process's `/proc/<pid>/status`: its pid in every namespace from
   diskgc's down to its own. The owner is *maybe alive* when the number
   appears at **any** level of any listed process's `NSpid`, of any user,
   zombies included. Any level, not only the innermost that the producer's
   `getpid()` returns: a supervisor may record a pid as its own namespace
   sees it, and a coincidence only retains. When no level matches, diskgc
   also opens a pidfd for the number in its own namespace: only `ESRCH`
   lets the verdict stand, as In use requires the kernel's word for
   "gone". The owner is then *gone* when the scan settled and every listed
   process of the user (as In use defines the user) gave a readable
   `NSpid` line. A process whose status cannot be read goes through In
   use's re-reads and pidfd classification: one confirmed gone drops out,
   so processes exiting during the scan do not block; one of the user that
   stays unreadable makes every owner verdict of that scan
   `producer_unknown`. Another user's process is never the owner of a
   candidate the eligibility walk accepts, so its unreadable status is
   ignored.
6. *PID reuse* makes a dead owner look alive: the candidate is retained,
   never deleted. No start-time comparison is made; it could only turn
   retained candidates eligible, and it would depend on the wall clock.
   With `pid_max` at 4194304 and a thousand processes, about one dead
   owner in four thousand is retained per scan; small namespace-local pids
   (a container's 9) collide more often.
7. *Stability.* Once the preconditions hold, a gone owner never comes back, and a later process with
   the same number is not the owner. Unlike `app_not_running`, where a
   browser can start after the check, the verdict cannot turn from gone to
   alive between the check and the deletion; a candidate replaced under the
   same name is caught by the identity checks of Deletion.
8. *When.* During planning and tallying, again under the candidate lock,
   and immediately before deletion or a command action after the process
   scan, from the same scan as the in-use check, under its Freshness bound.
   The index may order by an earlier verdict; it never decides.
9. *Scope.* It proves the owner gone, not every consumer. The in-use scan
   still runs, but a consumer that outlives the owner and keeps only a path
   string (an orphaned background job) is not seen. Each built-in's
   lifecycle verification states whether its candidates have such
   consumers.

A maybe-alive owner is reported `in_use`; one that cannot be judged is
`producer_unknown`, its detail naming the failed rule (the names are in
[reference.md](reference.md#producer-liveness-predicate-planned)).

**nix-shell rc directories.** The protocol behind
`producer_gone: nix-shell`: the candidate's own name is
`nix-shell-<pid>-<suffix>` or `nix-develop-<pid>-<suffix>`; Owner PID on
`<pid>`. A name of another shape is `producer_unknown`. Facts from Nix
2.34.8 (2026-10-23): `nix develop` names its directory after its own pid
and execs the shell, which keeps that pid; the directory is empty once the
rc script has run, and it is left behind after a normal exit. Legacy
`nix-shell` leftovers hold `env-vars` and `rc`. To verify before the
profile ships: legacy `nix-shell` keeps the pid through exec, and no live
shell reads the directory after its start. The bytes are small (about 350
MiB in 65,000 directories in TASK-68's dry run); the value is the
directory count, and proving Owner PID on the simplest producer first.

**pytest sessions.** The protocol behind `producer_gone: pytest`. Facts
from the pytest 9.1.1 sources pinned in the Nix test image
(`_pytest/pathlib.py`, `_pytest/tmpdir.py`):

- A session directory is `<temp root>/pytest-of-<user>/pytest-<N>`, with
  `N` one more than the largest existing number; `pytest-current` links to
  the newest. The temp root is `$PYTEST_DEBUG_TEMPROOT` or Python's
  `tempfile.gettempdir()` (`TMPDIR`, `TEMP`, `TMP`, then `/tmp`,
  `/var/tmp`, `/usr/tmp`), resolved. `--basetemp` sessions live elsewhere
  and are not matched.
- Right after creating the directory, pytest creates `.lock` in it holding
  its pid, unless `keep` is 0. `keep` is `tmp_path_retention_count`
  (default 3); `tmp_path_retention_policy=none` sets it to 0, and then
  there is no lock. The lock is removed when the session ends; a killed
  session leaves it.
- When a session ends, pytest deletes the sessions numbered at most the
  largest number minus `keep` whose `.lock` is absent or more than three
  days older than its own session. With `keep` 0 this includes its own
  session and any concurrently running session without a lock.

For a session `S` (the candidate, or the session holding a test-directory
candidate), its `pytest-of-<user>` directory `P` and its temp root `R`
(the parent of `P`):

1. `S` holds a `.lock` entry of any type: Owner PID from its content.
   Maybe alive: retained (`in_use`, evidence `owner_pid`); unknown:
   retained (`producer_unknown`); gone: eligible, a killed session.
   *(sound, under Owner PID's preconditions)*
2. No `.lock`, and `N` is at most `M − 3`, `M` the largest number among
   `P`'s `pytest-<digits>` entries: eligible. pytest's cleanup at the end
   of any later session with the default retention would delete `S`. A
   listing of `P` that fails or is cut short retains
   (`producer_unknown`). *(producer-equivalent)*
3. Otherwise `S` is eligible only when no visible process could be using
   `R` as its temp root (below); else it is retained (`in_use`, evidence
   `temp_root`, the processes listed). *(heuristic)*

*Temp-root users.* A process of the user could use `R` when one of these,
looked up in its own root and mount namespace, is `R` by device and inode.
The lookup opens `/proc/<pid>/root` as a directory and resolves the path
below it with `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)`, so an
absolute symlink resolves in the process's root, as pytest's `resolve()`
does there, never in diskgc's; the result is `statx`ed with
`AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC`. Lookups are deduplicated by the
identity of that root directory plus the path (two processes of one mount
namespace can have different roots). The paths: the absolute values of `PYTEST_DEBUG_TEMPROOT`,
`TMPDIR`, `TEMP` and `TMP` in its initial environment, and always `/tmp`,
`/var/tmp` and `/usr/tmp`. A process of the user whose environment or root
cannot be read, and is not excused by a privilege boundary (In use), could
use every root. Rule 3 needs Owner PID's preconditions on PID namespace and
`hidepid`; elsewhere it never permits. Python's fallbacks are always included, an
over-approximation, so a top-level `/tmp/pytest-of-<user>` relies on rules
1 and 2 alone: on a running system some process always has `/tmp` as a
possible root. Rule 3 is what reaches the newest sessions in a finished
shell's `TMPDIR` (`/tmp/nix-shell.*/pytest-of-<user>`), where most leftover
sessions lie.

*Stability.* Rule 1 is Owner PID. Under rule 2 new sessions only lower
`S`'s rank; a `keep` 0 session that deletes the newest number can raise
it again, which only retains, and the rule runs again before deletion.
Under rule 3 pytest never adopts an existing session; a pytest started
after the check makes a new number. A verdict can only be wrong because a
process was invisible to the check, not because of what happens after
it.

*Startup window.* Between pytest's `mkdir` and its `.lock` (one function,
microseconds) a live session looks finished. The built-in's `min_idle_for`
covers this window.

*Residuals* (the heuristic part; the loss in each is that run's temporary
files, so the run may fail and is run again):

- a live session without a lock (`keep` 0) with at least three newer
  sessions in the same `P`: pytest itself deletes it when any newer session
  with the default retention ends; diskgc may delete it sooner, and does so
  even when the newer sessions keep more than three;
- a finished session a project keeps on purpose with
  `tmp_path_retention_count` above 3 (to inspect failures): rule 2 deletes
  it once three newer sessions exist. A policy loss, not a liveness one;
- a live session without a lock among the newest three, whose pytest
  process did not take its temp root from its initial environment
  (`os.environ` or `tempfile.tempdir` changed inside a long-lived Python
  program that then calls `pytest.main()`), or is not visible to diskgc,
  or is excused by a privilege boundary.

Not matched: `garbage-<uuid>` directories that a pytest killed during its
own cleanup leaves in `P`; a known gap of the matching set.

**With the other checks.**

- *Order.* A producer verdict is a source predicate: it runs where
  `app_not_running` runs, before the eligibility walk, and again
  immediately before deletion after the process scan.
- *PID namespaces* are handled by the precondition and the any-level
  `NSpid` lookup. Neither the name nor the marker says which namespace the
  pid belongs to; the lookup does not need to know.
- *`hidepid`* can hide the user's own processes (precondition 4), so a
  `/proc` mounted with it gives `producer_unknown`, unless diskgc runs as
  root with `CAP_SYS_PTRACE`.
- *Mount namespaces.* Owner PID compares numbers, not paths. Temp-root
  users are resolved in each process's own namespace, so a container that
  binds a shell's `TMPDIR` as its `/tmp` counts.
- *Deadlines.* The reads are part of the in-use scan. A temp-root `statx`
  on a dead mount can block like the scan's other `statx` calls (In use, A
  process using a dead mount).
- *Cost.* Only when a selected source requires a producer verdict: one more
  read of each listed process's `status`, at most seven `statx` per process
  for temp roots (deduplicated per mount namespace and path), and one
  listing of each `pytest-of-<user>` directory per scan.

**Limits, stated plainly.**

- Outside the initial PID namespace, under `hidepid`, or on a filesystem
  not on the local list, no PID-based verdict permits a deletion; those
  candidates are kept as `producer_unknown`. diskgc in a container with
  its own PID namespace reclaims none of them.
- A reused pid keeps a dead owner's candidate until that pid's holder
  exits.
- Owner PID proves the owner gone, not every consumer of the candidate.
- A contract holds for the producer versions and configurations it was
  verified for; another version may name, lock or reuse its directories
  differently.
- The pytest residuals above.
- A process can forge a name or a `.lock`; diskgc is not a defence against
  a hostile local user (Principles).

### Project dependency directories (planned)

An unchanged `node_modules` or `.venv` can still belong to a project used
today: access need not change mtime or ctime. The current `fs` candidate
`idle_for` measures changes inside that candidate and cannot establish
project inactivity. The general in-use scan has known visibility limits
and cannot supply a reliable project-idle signal on its own.

A future project dependency source needs an explicit owning project root
and a cooperative activity protocol. Every consumer would hold a shared
project lock while it uses dependencies and update a last-use marker when
done. Reclamation would take the exclusive project lock and, under it,
recheck the marker age, project identity and candidate contents before
deleting. Missing, unreadable or inconsistent state must retain the
candidate. An uncooperative consumer means the protocol gives no safety
claim; therefore a built-in profile cannot assume participation merely
because a directory has the conventional name. No such source or lock
protocol is implemented. In the terms of
[Producer liveness](#producer-liveness) this is a producer lock held by
cooperating consumers; the schema gets a producer-lock predicate only with
its first user.

Any future `.venv` source must retain `checkouts: refuse`. Editable VCS
installs can place a real checkout under `.venv/src`, which may hold work
that cannot be recovered by recreating the environment.

### Source-anchored CMake output

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
error of the run reclaims nothing of the tier.

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
4. **Judged again under the lock:** the mount table read again and the
   place again (a mount made since step 1 is `contains_mount`, never
   looked into); the keep markers on the way again, on the very
   directories it was reached through; the source's own `where` (no longer
   passing it: `changed`); the eligibility walk (above), whose root must be
   the object found and the one locked (`changed`); the idle requirement
   on its newest change (none known: never idle). Then the in-use check
   against a process scan within the freshness bound (In use: Freshness).
   An unsettled scan is retried once after 20 ms; a persistently blocked
   scan can only skip candidates for half a second before it is replaced. Any
   failure: release, skip.
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
Not before: the reads of the mount table, the open and `flock` of step 3 (the lock does
not wait) and the in-use scan (bounded by its own rounds).
An in-use scan can overrun the deadline, including its one retry after an
unsettled scan. A dry run checks the deadline again before judging its
candidate; a reclaim checks it in the delete engine before each mutation.

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
  to it, or a change that makes its source's `where` fail, is not seen and
  the candidate is deleted; a file, socket or FIFO created inside it is
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
Lock files are never deleted or moved.

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
  holding a `.cargo-*lock*` file, known name or not (the recogniser's
  shape: a target whose only lock name is unknown is listed as
  `unsupported_layout`, not missed). A target is at most six directories
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
  an unreadable process of the user blocks as there;
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
   walk, one entry budget for the target). Measure the last build: newer
   than the one the caller chose the target by (its eligibility check, a
   plan), or no longer known: `changed`; then apply the idle requirement
   again even if the tally's earlier measurement passed. Then
   the in-use check of the whole target against a scan made now, under
   the locks (a scan from before could miss a process started meanwhile):
   every entry's record and the Cargo process checks (In use). Any hit:
   release, skip.
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
   deletion (`in_use`). Before each part its profile directory's lock
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
   fingerprints first. Lock files are never in a record (every walk
   refuses every Cargo lock name), so they are never unlinked, replaced or
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

**Limits.** The lock files are listed again before each part, not before
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
delete (In use, limit 5). The order "fingerprints first" relies on Cargo
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
client query is a run error (exit 3), not a skip: a daemon that is not
running, `EACCES` on the socket, or a `docker` command that is not the
Docker CLI. On a host whose `docker` is Podman's CLI shim, `docker context
show` exits 125 and every Docker profile reports that error (observed with
Podman 5.8.7, TASK-35). This fails closed; whether some of these cases
should be skips is open in TASK-113.

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

`nix-store --gc --max-freed <remaining need>`. Nix decides liveness (GC
roots, temporary roots, runtime roots of running processes); generations and
profiles are never removed. `--max-freed` means "stop after at least this
much"; the effect is measured, not assumed.

Store selection: `NIX_REMOTE` or the `store` setting can point `nix-store`
at another store. Before GC, diskgc must establish that the store **and its
root database** are the ones for the matched `/nix/store` filesystem. The
planned `nix --extra-experimental-features nix-command store info --json`
`url` check and `builtins.storeDir` check are necessary but insufficient.
On Nix 2.34.8, two direct local stores with distinct `NIX_STATE_DIR` values
both report `url: "local"` and `/nix/store`, although their state directories
hold different roots. Likewise, `url: "daemon"` does not identify the
daemon's physical store: its socket may be overridden or reach a daemon in
another mount namespace. Until a binding check covers both the physical
store and the state directory, the `nix-store` kind remains unshipped
(TASK-36); a URL or logical store directory alone must never enable GC.

A disposable Nix 2.34.8 container confirmed a narrower command primitive:
`--store 'local?store=/nix/store&real=/tmp/real&state=/tmp/state-a'`
overrode conflicting `NIX_REMOTE` and `NIX_CONFIG` values for both the
read-only root query and `nix-store --gc --max-freed 1`. That GC preserved a
root in state A and removed an unrooted path. State B then recorded that
same physical store path as valid, without a GC root in B; collecting through
the same physical `real` path with `state=/tmp/state-b` removed it despite
the GC root in A. Explicit command arguments can identify
which database a *direct local* child uses, but they do not identify the
database used by the machine's Nix daemon or rule out another root database
over the same physical store. A shipped daemon-backed profile needs a pinned
daemon connection with independently verified physical store and state/root
database identity for that very connection, maintained through GC; merely
forcing an explicit local URL would also exclude ordinary unprivileged
daemon users. Nix documents `real` and `state` as separate settings for the
[local store](https://nix.dev/manual/nix/2.34/store/types/local-store) and
[local daemon client](https://nix.dev/manual/nix/2.34/store/types/local-daemon-store.html).

Verified on Nix 2.34.8 (2026-10-15): an untrusted, non-root user can delete
dead store paths through the daemon (`nix-store --delete` on a freshly
added path; `--gc --max-freed` uses the same daemon operation but was not
run, to avoid deleting the machine's real garbage). `builtins.storeDir`
alone is not enough: it still says `/nix/store` when `NIX_REMOTE` points
elsewhere. Lix and other Nix variants are not verified.

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
- **Environmental** (at the start of every reclaim, for *all* tiers, before
  the first deletion): roots and paths exist, contain no symlinks, resolve
  to a device and mount; `owners:` names resolve to non-system users;
  executables of command actions resolve; Docker and Nix backends are bound
  and reachable.
  - A missing path or tool, another filesystem, or a remote Docker endpoint
    is normal: that tier is skipped with its reason (`missing`, `other_fs`,
    `remote_backend`). A named Docker context is `unsupported_backend`.
  - An unsafe tier root (`unsafe_path`) excludes that tier and is reported
    as an error (exit 3 if the goal is then not met). An unsupported store
    or backend also excludes its tier, but is reported as a skip; with no
    other error and the goal unmet, the run is exhausted (exit 1). The
    other tiers still run: refusing all cleanup because one tier is broken
    would let the disk fill.
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

  A reclaim that needs a missing lock exits 2, printing those two lines.
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
for deployment until TASK-67 closes. Its /tmp catalog, operator policy and
deployment evidence remain unresolved.

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
  ours). Nix GC goes through the Nix daemon as a normal user.
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
  per-entry owner check for `owners:` to enforce.

## Deadlines

`--timeout` and per-tier `timeout`s are **cooperative**: diskgc checks the
deadline between operations (each walk step, each unlink, each child wait)
and, at the deadline, starts no new work and terminates a running child
(SIGTERM, SIGKILL after 10 s). A single filesystem operation that blocks in
the kernel (a hung network mount, a disk in error recovery) cannot be
interrupted by a signal and can overrun the deadline; diskgc lists the
overrun in the report's `overruns` when it returns. Network filesystems are never candidates, which
removes the commonest cause, and a mount point is never `statx`ed by
discovery or the judgement of a candidate (Before the walk). What can
still block: a process using a dead mount hangs the in-use scan (In use),
and a dead mount made after the mount table was read (Deletion, accepted
limits). A supervised worker that the caller could
abandon while it stays blocked is not part of v1: it would have to keep the
host lock held while abandoned, which moves the problem rather than solving
it.
