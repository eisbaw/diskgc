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
  CIFS, sshfs; the list is `NETWORK_FS_TYPES` in the engine) are never
  candidates' filesystems (`unsupported_fs`).
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
- **No special content** (`contains_special`):
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
- **Not protected or excluded** by overlap with a `protect` entry or a
  negated profile ([reference.md](reference.md#protect)).

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

Or it is a directory locked with `flock` by someone else (`locked`), the
convention systemd-tmpfiles also honours: diskgc opens the candidate
directory below its parent and takes an exclusive `flock` without waiting
(Deletion, step 1); a lock held by anyone else, shared or exclusive, makes
that fail. The walk then runs under the lock and must find the locked
directory (same device and inode): a directory replaced between the open and
the walk is `changed`. Only directories are locked; a candidate that is a
single file is not.

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
listings that each brought new pids the scan is unsettled, and every
candidate is `in_use`; listings made only for re-reads do not count (they
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
   Freshness below), not atomic with the deletion;
6. a file handed to a process that was already read (an fd passed over a
   Unix socket, `SCM_RIGHTS`) is a new use of the same kind, and an fd in
   flight in a socket's queue belongs to no process at all: neither is
   seen.

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
candidates checked within half a second of its start. The check that
clears a candidate looks at the clock as its last step before the
deletion starts; if its scan is no longer fresh then (a suspend during the
scan or right after it, a slow scan), the candidate is checked again
against a new scan, and if that one is not fresh either, it is an error of
the run, not a skip. Refusing needs no freshness. Buckets
([architecture.md](architecture.md#the-index)) only group candidates for
scheduling; each member gets its own check under this bound, which a scan
less than half a second old may answer: a use that starts after that scan
is limit 5 above.

**Visibility:** without root, other users' `/proc/<pid>/fd` cannot be read,
so their use of *your* files is not seen; nor is the use of the user's own
processes noted above as unreadable (setuid, more capable, non-dumpable).
Fine for one-user machines and runners; weaker on shared hosts.

### Deletion

For the `delete` action:

1. Take an exclusive `flock` on the candidate (directories), as
   systemd-tmpfiles does, so cooperating tools see it is being removed.
2. Under that lock, run the eligibility walk and the in-use check (above).
   Any failure: release, skip.
3. Delete depth-first exactly the entries the walk recorded, reopening
   each directory relative to its parent (`openat2`, see Path traversal)
   and checking its identity against the walk's record, with a bounded
   number of fds (the shape above). Before each `unlinkat`, check the
   deadline, then `statx(AT_SYMLINK_NOFOLLOW)` the name and compare its
   identity (device, inode, owner, type, birth time where reported) with
   what the walk recorded; the `unlinkat` is the very next operation. A
   mismatch (or the name gone, or a mount met on the way down, `EXDEV`, or
   the `unlinkat` itself finding the name gone, mounted on or of another
   type) stops the delete: nothing more is unlinked and the candidate is
   `changed`; a later run walks it afresh.
   `make_writable` (if the profile allows it) adds owner write permission
   to a directory right after it is opened and matched, before anything in
   it is unlinked, after all the checks (and never once the deadline has
   passed). It is not undone: a delete that stops part-way (`changed`, an
   error, the deadline) leaves the directories it entered with owner write
   permission added.
4. Release.

**Scope** means *the objects the walk checked*. Every directory diskgc
descends into is matched by (device, inode) against the walk's record, and
the candidate itself is held open while it is deleted, so if a checked
directory is renamed elsewhere mid-delete, diskgc keeps deleting that same
object (the cruft it checked), never whatever now sits at the old path.
Two windows remain and are accepted: an entry replaced between the `statx` and the
`unlinkat` of its name (Linux has no unlink-by-inode), and a file opened or
created inside the candidate after step 2. A third limit is separate from
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

### Other properties

- **Hard links:** removing a name that is hard-linked elsewhere frees
  nothing; the signed `avail_delta` shows that.
- **Deleted but open:** a file deleted while still open keeps its space
  until the process closes it. Every report lists such files on the
  requested filesystem (`held_by_deleted_files`): the open fds of visible
  processes on regular files with link count 0, per process, each file's
  allocated bytes counted once. A deleted file that is only mapped (its fd
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
  and the start of the binary (their locks during the binary's runtime
  were not sampled).
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
  nightly only the target directory's profile directory); 1.99.1 also
  passes the package's `OUT_DIR` (inside the profile directory). A
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
  nested targets); the first refusal is the target's. The entry budget is
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
  is `empty` and not re-listed on every run.
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
  told; a directory or a symlink of that name, dangling or not, does not
  make it attached: it was put there by `--target-dir`, `CARGO_TARGET_DIR`,
  a `target-dir` in `.cargo/config.toml`, or it is a tool's nested target)
  and a visible Cargo process (named as above) runs, wherever it works:
  where a detached target's Cargo runs cannot be told from its path, so
  any Cargo may be using it. A shared target directory is therefore
  reclaimed only while no visible Cargo runs.

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

1. **Lock.** Reach the target again from its root, as found (Discovery);
   refuse before locking a target whose layout or lock set is not one
   diskgc reclaims (`unsupported_layout`, `empty`: an unknown lock name
   would not be taken). Then take every lock file of every profile
   directory, exclusively, without waiting. Holding every lock is what
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
   plan), or no longer known: `changed`; then the idle requirement. Then
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
   diskgc reclaims (whole sets, Lock sets) holds `.cargo-lock`, and in the
   measured lock order every Cargo takes `.cargo-lock` (Lock files: lock
   order); that it writes nothing into the profile directory before then
   is not verified yet (task-11). So a Cargo that starts during the
   deletion and creates a lock file diskgc does not hold (a 1.99.1 build
   on a target 1.90 made creates `.cargo-build-lock`) then waits on the
   `.cargo-lock` diskgc holds and, if that holds, writes nothing. A Cargo
   that wrote into the profile before taking `.cargo-lock` would break
   this; the Cargo race tests (task-11) must show it holds on every pinned
   toolchain: a reclaim of a target 1.90 made paused at an `unlinkat`, a
   1.99.1 build started then, and `/proc/locks` showing that it created
   `.cargo-build-lock` and waits on `.cargo-lock` without writing. diskgc
   does not create missing lock files itself: that would be a write into
   the target, and the invariant does not need it.
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
treating a unit whose fingerprint is missing as dirty (measured: the next
build compiles again, on the three pinned toolchains, also killed right
after the fingerprints or before the last unlink); race test 6
([testing.md](testing.md), task-11) is to check it at seeded positions on
every pinned toolchain.

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
- Runs in its own process group; killed after the tier's `timeout`;
  inherits the host lock (below).

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

  Missing → exit 2, printing those two lines. The file is reached like any
  configured path (no symlink, no mount crossing: Path traversal), opened
  `O_PATH`, checked to be a regular file and reopened read-only through
  that fd, so a FIFO or a device at the path is never opened; anything but
  a regular file there is exit 2 too.
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
