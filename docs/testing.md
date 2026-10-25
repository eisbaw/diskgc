# diskgc testing

Normative for how the PRD's acceptance criteria and the guarantees in
[safety.md](safety.md) are tested.

**No test ever runs diskgc against the developer's live system.** Tests run
against a modelled host (in-process), inside a rootless test container, or
inside a rootful Podman container on a disposable Nix-built QEMU guest.
One named exception proves the pre-release sandbox gate itself:
`a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate`
(`crates/diskgc/tests/cli.rs`) runs the built binary's reclaiming
`ensure` on the host, once for kind `cargo-target` and once for kind
`fs`, and it touches only a fixture of its own: the roots and `PATH` are
a fresh temp directory holding a fake Cargo target and an old directory
the config's `fs` user profile finds, the config a file of its own, the
goal unreachable; the gate is expected to refuse (exit 2), and every
fixture object must keep its inode. The other binary tests stop at usage
errors before the machine is read.

## Grounding: acceptance signals and negative feedback

### Acceptance signals

The strongest end-to-end checks; if these are green, diskgc genuinely works.

1. **Deletion oracle (e2e).** Every container scenario declares, in its
   fixture, which objects are eligible to be deleted. Before and after
   diskgc runs, the harness snapshots the **whole container filesystem**
   (not only the scratch tmpfs) keyed by (path, device, inode, birth time);
   any object that disappeared or was replaced (same path, new inode, e.g.
   a `.cargo-lock` that Cargo recreated) outside the eligible set fails the
   test, whatever diskgc reported. Where the filesystem reports no birth
   time, the key falls back to (path, device, inode) and the harness logs
   which key it used. The fallback also applies on overlayfs (the
   container's root): a copy-up, e.g. of a directory whose child is
   deleted, keeps the overlay inode number but gives the object a new
   birth time. Limit: there, a replacement that reuses the freed inode
   number is invisible to the snapshot; the strace record still shows it
   for traced processes. Objects the harness creates therefore live on
   tmpfs mounts (`/tmp`, `/var/tmp`, `/run/diskgc`, `/home/tester`; Container setup),
   where the full key applies. Pseudo filesystems (`/proc`, `/sys`,
   `devpts`, cgroups, ...) and read-only mounts (the binaries under test,
   toolchains) are not walked, decided by each object's mount id; nothing
   can be deleted through a read-only mount, and a writable mount nested
   below one is still walked. Limit: an object reachable only through a
   read-only mount of a filesystem that some other process writes to
   elsewhere is not snapshotted. Any other unreadable path fails the
   snapshot rather than leaving a blind spot. A second, independent record
   comes from
   `strace --seccomp-bpf -f -e unlink,unlinkat,rmdir,rename,renameat,renameat2`
   on diskgc and its children (the seccomp filter stops the traced process
   only at those calls; a refused filter fails the run); deletions done
   inside daemons (Docker, Nix) are invisible to it and are covered by the
   filesystem snapshot only. A traced call whose path is not a snapshot path
   is logged with its reason, never dropped: created during the run
   (counted), or reached through a symlink, written with `.` or `..`,
   relative to a deleted directory, or relative to a directory fd strace
   could not name (its path longer than a page, in trees deeper than
   `PATH_MAX`) (each logged); the snapshot still judges the object itself,
   and reaches paths of any length. The oracle is computed from the fixture's spec, never
   from diskgc's output; the only allowlisted paths are diskgc's own state
   directory and the fixture's own concurrent writers (declared in the
   fixture). **Judging is mandatory:** every traced run of code under test
   is recorded, and a test that leaves one unjudged by the oracle (or judges
   it with an oracle armed after the run started) fails.
2. **Cargo race tests 1–9** (below) green on every supported Cargo version.
3. **Model property suite** green: never-delete invariants on the call
   trace, the index is never an authority, selection semantics equal the
   reference implementation, exit-code precedence.
4. **JSON contract snapshots** (`insta`) unchanged, or changed together with
   [reference.md](reference.md) in the same commit.
5. **Freeing works:** on the 100,000-entry `/tmp` fixture, `ensure` reaches
   `--min-free`, exits 0, and the first deletion starts within the latency
   bound in the PRD.

### Good and bad, observable

The PRD's "What good / bad looks like" lists are the specification; each
bullet maps to at least one named test in this document or in the task that
implements it. A bullet without a test is a gap, filed as a task.

### Negative feedback: how the system tells us it is wrong

- **Gates:** `just check` (fast, every change) and `just e2e` (before every
  commit). A red gate blocks the commit.
- **Oracles independent of diskgc:** the deletion oracle above, the
  reference implementation of the selection rules, `/proc/locks` sampling in
  the Cargo race tests, `statvfs` read by the harness, not by diskgc.
- **Prove the check bites.** Every safety check in [safety.md](safety.md)
  has a **model test** (so it runs in the workspace) that fails when the
  check is removed. This is verified mechanically in the hardening blocks
  with mutation testing (`cargo-mutants`, workspace tests) on `diskgc-core`
  (which holds path resolution) and on `diskgc-linux`'s statx conversion,
  mountinfo and `/proc` code: a
  surviving mutant in a safety check is a bug in the tests. Checks that can
  only be exercised against the real kernel (the container suite, which
  `cargo-mutants` does not run) get a one-off manual bite proof recorded in
  the task that implements them.
- **Interlock tests bite too:** a do-nothing canary binary behind the same
  run-time interlock as the destructive-test binary is run on the host by
  every `just e2e` and `just e2e-full` before any container starts, and
  must refuse (anything else stops the run); a container test requires it
  to pass inside. A container test also proves the per-operation check
  aborts (absolute path, `..`, a symlink out of the scratch mount, a bind
  mount of the scratch filesystem itself).
- **A release is awaited, never tried once.** A test that closes the last
  fd of a lock or a bound socket and then asserts it is gone waits for that
  with a bound (`RELEASE_BOUND`, 2 s, defined once for the `diskgc-linux`
  host tests and once in `diskgc-e2e/src/lock_release.rs` for every E2E
  check): a `/proc/<pid>/fd` or `fdinfo` reader on the machine can make
  the release lag the close (docs/safety.md, Host lock). This was the cause
  of TASK-64 #37's one-in-600 failure of
  `a_lock_conflicts_until_its_fd_is_closed`; the kernel fact is pinned by
  `a_flock_can_outlive_its_close_while_a_proc_reader_holds_the_file_kernel_fact`,
  which fails if the kernel ever releases synchronously. Each kernel VM
  (below) runs the `diskgc-linux` unit tests whose names end in
  `_kernel_fact` directly in its guest (`diskgcLinuxTests` in `flake.nix`),
  so the claim is checked on each pinned kernel, not only the build host's.
  Its readers read `fdinfo` because on 5.8 fd listing and link stats take
  no file reference: without that read, the 5.8.16 guest saw no late
  release in about 1.7 million attempts and the test failed. A held-lock
  assertion needs no wait: a lagging release only keeps a lock longer.

### Definition of Done (every task)

- Its acceptance criteria are met and demonstrated by tests that run in
  `just check`, `just e2e` or `just e2e-full`; the task names them.
  Review, journey and hardening tasks are exempt where an acceptance
  criterion is a review outcome; their result is recorded in the task.
- `just check` and `just e2e` are green.
- Behaviour that changed is reflected in the one document that owns it
  (PRD map); no rule is stated twice.
- Honest limits found while implementing are written down where the rule
  lives, not left in commit messages.
- Tasks labelled `irreversible` additionally pass the deep review gate
  (mped-architect plus the reviewers named in the task) before they are
  marked done.
- **A task that ships or changes a profile** (built-in, or one the docs
  or the example config ship) ships it with all of the following. This
  rule is their single home (Mark, 2026-10-19); other documents point
  here.
  - Every such profile:
    - its `examples` (`match` and `no_match`, docs/reference.md,
      Profiles);
    - a `ModelHost` fixture modelled on the layout seen on real machines
      (the dogfood surveys in the task notes), with a model test that
      bites: a mutation of the profile's distinguishing pattern, type or
      `where`, or of a safety check it relies on, turns it red. The task
      notes name each mutation tried and the test that went red, so the
      bite can be checked;
    - a container test wherever the kernel's view matters to it (sockets,
      FIFOs, mounts, other users' files, processes using its candidates,
      real tools' layouts);
    - dry-run evidence: a read-only `ensure --dry-run` on a real machine,
      its numbers (candidates, eligible, skip reasons, sizes) in the task
      notes.
  - Built-in profiles also:
    - a row in reference.md's
      [Liveness contracts](reference.md#liveness-contracts) whose class
      admits it ([safety.md](safety.md#producer-liveness)), with the
      producer's lifecycle verified against the pinned version and, for a
      sound or heuristic contract, its vectors (Producer liveness vectors,
      below) and a live-producer counterexample;
    - a golden match set (Profile golden sets, below; task-17);
    - `verified_with`: the tool version the profile was checked against,
      the one of that dry run.
  - For daemon-backed profiles with no pathname matcher, the `examples`
    and path golden-set requirements are met by a checked-in ModelHost
    backend fixture with an exact golden plan root and prune action for
    each profile. Their read-only dry run uses the real daemon inside a
    disposable test container, not a developer or production host path.
    The fixture also proves the backend refuses unsupported endpoints and
    storage roots before any prune. Its Buildx selection test proves the
    fixed Engine `docker builder prune` still acts on the bound daemon.

## 1. Planner tests: plain data

The planner is a pure function (index in; plan, skip reasons, exit-code
precedence and report out), so eligibility, ordering, skip reasons, the
exit-code table, the dry-run plan and the JSON report are tested as data,
with no host at all.

## 2. Model tests: a YAML host model instead of a real disk

All of diskgc's access to the machine goes through the `Host` port
([architecture.md](architecture.md#the-host-port)), so tests substitute
`ModelHost`, which answers from a *host model*. Nothing is intercepted at
the kernel level: no FUSE, no `LD_PRELOAD`.

The model is data, not a second Linux:

- **Inode graph**, not a path map: hard links, renames and "an entry
  replaced between the identity check and the unlink" are expressible.
- **Derived, not duplicated**: available space is computed (filesystem size
  − allocated blocks, each inode counted once − `other_used`), so deleting
  something in the model frees space in the model.
- **Grown on demand**: mounts (ids, parents, types including nfs/btrfs,
  bind and stacked mounts), trees with owners, locks (holders and waiters),
  processes (exe, cwd, root, initial environ, uid, open fds including
  deleted files, memory mappings), kernel capabilities (`statx` masks,
  `openat2`), scripted `docker` / `nix-store`, clock, delayed freeing.
- **Strict loading**: unknown keys are errors, references must resolve, and
  a JSON Schema is generated for editors
  (`crates/diskgc-model/host-model.schema.json`, kept current by a test).
- **Events on the call trace.** A race is an event between two `Host` calls,
  addressed by the call (`at: {op: unlinkat, path: <…>, nth: 1, when:
  before}`), not by a hook in the product. Property tests place an event
  after every call *k* in turn, covering every interleaving of a small
  scenario.

```yaml
host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /,     fs: ext4, dev: "254:1", size: 1000G, other_used: 980G}
  - {id: 2, at: /home, fs: ext4, dev: "254:2", size: 900G,  other_used: 880G, parent: 1}
tree:
  /run/diskgc/lock: {uid: 0, gid: 0}
  /home/mark/src/app/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug:
      .cargo-lock: {}
      deps: {app-1: {size: 8G, mtime: 2026-10-01T00:00:00Z}}
  /nix/store/x-cargo/bin/cargo: {}
locks:
  - {path: /home/mark/src/app/target/debug/.cargo-lock, holder: 4242, mode: exclusive}
processes:
  - pid: 4242
    exe: /nix/store/x-cargo/bin/cargo
    cwd: /home/mark/src/app
    maps: [/nix/store/x-cargo/bin/cargo]
    environ: [HOME=/home/mark, PATH=/nix/store/x-cargo/bin]
events:
  - at: {op: flock, path: /home/mark/src/app/target/debug/.cargo-lock, nth: 1, when: after}
    create: /home/mark/src/app/target/release/.cargo-lock
expect:
  exit: 1
  skipped: [{candidate: /home/mark/src/app/target, reason: locked}]
```

Fixtures live in `tests/models/<area>/<case>.yaml`, one named test per file;
JSON reports are snapshot-tested with durations, timestamps and seed
redacted. The sketch above is the fixture
`crates/diskgc-model/tests/models/sketch/locked-cargo-target.yaml`, run
through the executor (`tests/sketch.rs`: `ensure --min-free 15G --select
profile:cargo-target --roots /home/mark/src /home/mark/src`).

**Vector corpus.** A *vector* is one reviewable `ensure` scenario as data:
the invocation, an ordinary `host_model: 1` under `host:`, and the expected
results, written from the docs and never derived from diskgc's report. The
format is `profile_vector: 1` (it began as one built-in profile per file;
every key added since is optional, so those files are unchanged). One
runner, `crates/diskgc-model/src/profile_vector.rs`, drives
`diskgc_core::invocation::ensure`, the same composition the binary runs
(configuration, privileges, owners, protections, tiers in policy order,
executor), on two fresh `ModelHost`s: one `--dry-run`, one reclaim.
`tests/profile_vectors.rs` runs every YAML file below the corpus roots and
names every failing file:

- `tests/models/profiles/`: one vector per built-in, kept in place because
  the container replays (diskgc-e2e) embed these files;
- `tests/vectors/<area>/`: `walk` (the eligibility walk and its corner
  cases), `inuse`, `select` (filters, negation, unions, selection errors),
  `config` (protect, groups, owners and `--allow-root`, static errors),
  `exit` (exit codes and tier order), `races` (events between checks and
  deletion), `backend` (Docker bindings and prunes), `builtins` (each
  built-in source in each role below) and `release` (the proposed 0.1
  catalog: what it admits and what it refuses).

The invocation keys: `profile` (shorthand for `select: profile:NAME`) or
`select` (`--select`; neither: the configured groups run), `config` (the
configuration file's text; absent: none was found), `catalog` (the
build's built-in catalog: `development`, the default, or
`proposed-release-01`, what the release-01 build's
`proposed-release-01-catalog` feature selects), `user_profiles`
(without a `config`: added to the built-ins), `roots` (`--roots`), `path`
(PATH), `min_free`, `target_free`, `timeout` (default 600s, `0` none),
`tally_budget`, `allow_root`, `seed` (`DISKGC_SEED`) and `entry_budget`
(the walk's entry budget in place of the product's 2,000,000, so
`too_many_entries` fits a small model: `EnsureOptions::test_entry_budget`,
which the binary always leaves unset;
`the_binary_never_sets_the_test_entry_budget` pins that no flag or
configuration key reaches it). The account database `owners:` names
resolve through is host-model state (`host.users`, read by the `Host`
port's `account_uid`). The runner hands `invocation::ensure` the
configuration's bytes, as `main.rs` does after its lookup and read: the
parse, the catalog's checks (`Catalog::config`) and every refusal
before the executor (exit 2) are the product's. Two keys are harness
seams the binary does not have: `user_profiles` without a `config` (how a
mutated built-in is tried against an unchanged vector) and
`entry_budget`.

`expect` holds: `plan` (the complete dry-run plan: `candidate`, `reason`,
`source`), `dry_run_exit` (default 0), `dry_run_errors` and `errors`
(complete lists of `{cause, contains}`), `reclaim_exit`, `actions` (the
complete set), `removed` (paths that existed and must be gone; Cargo keeps
its tagged root while its contents go), `skipped` (complete:
`candidate`, `reason`, and optionally `process_blockers` as `{pid,
evidence}`, `[]` meaning the report's `null`; an empty list in the report
itself is a runner error), `preserved` (paths that
must survive: they existed before reclaim or an event makes them),
`commands` (optional: every command or Docker run in order, `{cwd,
argv}`), and `report` / `dry_run_report` (optional subsets of the JSON
report, e.g. `{status: met, seed: 42}`).

Every vector is also held to invariants it does not state: the dry run
never calls `unlinkat` or `fchmod` and runs no command; every `unlinkat`
lies inside an expected action; every deleting action has a `removed`
postcondition; every command run pairs with exactly one action (a
command action's run in its checked candidate or that candidate's parent,
a Docker action's run of the `docker` client), every command action has
its run, and no run is left over; no event is left unfired.
`runner_invariants_reject_perturbed_vectors` shows these checks fail.

Completeness is enforced by the results, not by tags in the files:
`every_skip_reason_has_a_vector` needs, for each reason in `Reason::ALL`,
a vector whose dry-run plan or reclaim skips show it. The report's skip
reason is a `Reason` (backend skips included), so `Reason::ALL` is every
value the code can produce, and diskgc-core's
`every_status_and_reason_is_a_documented_value` pins `Reason::ALL` plus
`RESERVED_REASONS` to reference.md's enum exactly. `RESERVED_REASONS` are
documented reasons nothing produces yet, each naming the task that adds
its producer (`stale`: TASK-31; `backend_unavailable`: TASK-36, TASK-113);
the test fails once one enters `Reason::ALL` or is shown by a vector, so a
reservation cannot outlive its producer;
`every_builtin_source_has_match_in_use_guarded_and_nested_vectors` needs,
for each built-in source run unchanged, an action of it, an `in_use` or
`locked` skip, a `protected` or `excluded` skip, and a nested-guard skip
(`kept`, `contains_checkout` or `contains_special`); Docker prunes need
only the action; `every_blocker_evidence_has_a_vector` needs every
`ProcessEvidence` value in some vector's matched `process_blockers`.
`wrong_python_matcher_is_rejected_by_the_checked_in_vector`
proves one vector bites a matcher mutation; TASK-121's notes record which
vectors kill which mutation of each safety guard.

To add a vector: copy the nearest one in its area, model the candidate
and its neighbours under `host.tree` (the schema is
`host-model.schema.json`), write `expect` from the docs, then run
`nix develop -c cargo test -p diskgc-model --test profile_vectors`. When
the first run disagrees, decide from the docs which side is wrong; a
known product defect is pinned only with a comment naming its task. An
event addressed by `nth` depends on the call sequence:
`VECTOR_TRACE=<part of the file name>` prints the reclaim run's call
trace. Unknown keys and invalid host references fail loading, a new file
is picked up without registering a Rust test, and any other file type in
the corpus is an error. Then run `nix develop -c just check`.

`checked_in_python_profile_vector_replays_in_container` embeds the Python
positive/checkout YAML case into the destructive-test binary, materializes
its ordinary files and directories on the container's private tmpfs, and
compares the real binary's plan, actions, skips, and surviving paths to
that same expected data. It also uses the mandatory deletion oracle. Run
it with `nix develop -c just e2e-one
checked_in_python_profile_vector_replays_in_container`, then run
`nix develop -c just e2e` before committing. This replay currently accepts
only plain files and directories without modelled time or processes; it
fails explicitly on other model features. The standard replay uses rootless
Podman. The separate
`checked_in_python_profile_vector_replays_under_native_docker` gate starts
the pinned Docker Engine daemon in disposable dind, checks its server
identity, then runs the same vector through real `diskgc ensure` dry-run and
reclaim calls. It compares the complete plan, action, skip, and surviving
paths to the checked-in expectations and runs the deletion oracle while the
daemon remains alive. The daemon's private data and runtime directories are
declared concurrent writers; the profile candidate and its neighbors remain
checked. Run `nix develop -c just e2e-docker-one
checked_in_python_profile_vector_replays_under_native_docker`.

How the format reads (the schema documents every key):

- **Tree:** keys are absolute paths; missing parents are created. A mapping
  of scalars is a file (`{}` is empty; `symlink:`, `link:` for a hard link,
  `type: socket|fifo|char_device`, `cachedir_tag: true` for a file whose
  first bytes are a valid `CACHEDIR.TAG` signature: contents are not
  modelled otherwise, every other file reads as empty); a mapping of mappings is a directory, with its own
  attributes under `.` (`{.: {}}` is an empty directory). Owners default
  to 1000, times to `clock`.
- **Mounts** in mount order, each with a `parent` (a stacked mount's parent
  is the mount it covers); `statx_missing` lists fields the filesystem does
  not answer. A repeated device requires `bind_of: <earlier mount id>` and
  exposes that filesystem's root through a distinct mount ID. Filesystem
  metadata must match; subdirectory binds are not represented.
- **Processes:** `exe`, `cwd`, `root`, `environ`, `fds` (`deleted: true`
  for a deleted-but-open file), `maps`, and `proc: denied|failed` for the
  `/proc` read errors; a pid that exits is `Gone`. What `status` shows,
  readable whatever `proc` says: `name`, `uid` (all four), `euid` (effective
  and saved, as after a setuid exec), `gid`, `non_dumpable: true` (its `fd`
  directory owned by root), `privileged: true` (holds a capability diskgc
  lacks), `uid_map` (the text diskgc reads; default the initial
  namespace's; `none`: absent), `nspid` (its `NSpid:` levels, the first
  its pid; default `[pid]`; `unreadable`: that read fails), and
  `zombie: true` (listed, its `status` readable and its pidfd open, every
  other reader `Gone`; no fds, maps or locks). The top-level
  `pid_namespace: nested` and `proc_hidepid: true` are the held `/proc`'s
  answers to Owner PID's preconditions (default: the initial namespace,
  no `hidepid`); a mount's `fs:` name gives `fstatfs` its magic (unknown
  names: 0, never on the local list). The top-level `own_uid_map` is diskgc's
  own (same default, `none` too), `own_uid` its user id (default 1000; 0
  models running as root), and `sandbox_gate` the pre-release gate as
  `LinuxHost` has it (`open`, the default; `closed`: `may_delete` and the
  deleting calls are refused; `refuses_deletes`: only the deleting calls
  are). `command_exit` is the exit code of a modelled command action
  (default 0); `command_error_after_start: true` models a forwarding or
  wait error after spawn, so the executor must still report an action and
  observed space change. The model records argv, environment and checked
  cwd but starts no real child. The modelled command path is `/usr/bin:/bin`:
  a `docker:` answer is given only while an executable `docker` is there
  (otherwise ENOENT, as `LinuxHost`'s spawn: no client installed). It
  must be a regular file with an execute bit; a symlinked client (normal
  on NixOS) is not followed and reads as absent.
  `locks` give their holder an fd of its own. As on
  `LinuxHost`, an empty fd table, map or environment, or no `exe`, reads
  as `Failed` (an
  empty read of a live process is never the answer there). diskgc itself
  is listed too (`own_pid`, default 65000): its fds are those the code
  under test holds open, its other entries read as `Failed`. A socket file
  can be `bound: true` (reported by the socket diagnostics).
- **Events:** `at: {op, path, nth, when}`, `at: {call: k, when}` or `at:
  {elapsed: 5s}`, and one action: `create`, `mkdir`, `remove`, `rename`,
  `replace` (a new inode at the path), `chown` (a new owner, same inode),
  `open`, `close`, `exit`, `fork` (`{pid, child}`: a copy sharing the
  parent's open file descriptions), `dumpable` (`{pid, value}`: what its
  `fd` directory's owner shows from then on), `access` (`{pid, proc}`: how
  its `/proc` reads go from then on), `advance`
  (a clock jump, e.g. a suspend), `fail` (the call fails with the named
  errno without acting, e.g. `fail: EIO` on a `read_dir`; only with an op
  trigger, `when: before`, on `openat2`, `statx`, `read_link`, `read_dir`,
  `read_head`, `unlinkat`, `fchmod`, `bound_unix_sockets`, `statvfs`,
  `proc_identity`, `proc_identity_released`, `docker_query` or
  `run_command`), `mount` (`{id, at, fs, dev,
  size}`: a fresh filesystem mounted on an existing directory, e.g. a FUSE
  mount appearing in a candidate). `remove` is `rm -r`: every entry below is unlinked, and an
  object still open keeps only itself. An event that never fires fails the
  test.
- **Clock:** virtual; it moves only by `sleep`, `flock_wait` (which jumps
  to the next timed event or its deadline) and `advance`.
- **Checks** (`ModelHost` fails the test at the call): `openat2` without the
  `RESOLVE_*` and `O_*` flags of
  [safety.md](safety.md#path-traversal) steps 2 and 3; `statx` of anything
  but an empty path or a single name below a directory fd; `unlinkat` of a
  name unless the previous port call (clock reads aside: the deadline is
  read right before the unlink) was a successful `statx` of that name in
  that directory (the identity check right before the unlink,
  [safety.md](safety.md#deletion)). Locks live on open file descriptions
  exactly as long as an fd refers to them.
- **Limits:**
  - symlinks are never followed (every allowed call refuses them); `..` is
    not modelled; permissions are not checked;
  - inode numbers are never reused, so a replacement that reuses the freed
    inode number cannot be modelled;
  - `/proc` link text is the path at open time (Linux follows renames);
  - mtime, ctime and birth time are equal at load, so "ctime newer than
    mtime" cannot be expressed;
  - one task per process; diskgc's own process has only its fds;
  - no signals (the port has none yet); `read_dir` lists names in name
    order, while the kernel's order is the filesystem's, so callers must
    not rely on it;
  - processes come from the model or a `fork` event (no exec); memory
    maps show the mapped file's own device (a layer's device, as overlayfs
    can show, is modelled with a second filesystem);
  - lookups cost time linear in the model, so 100,000-entry models need
    indexes first.

Model scenarios required by [safety.md](safety.md) and
[architecture.md](architecture.md), among others:

- an entry renamed out of a candidate, or replaced by another inode,
  between the walk and its unlink: the replacement is never deleted, the
  candidate is `changed`;
- a candidate directory renamed elsewhere mid-delete: deletion continues on
  the checked object only;
- a user-owned directory containing another user's file: `not_owner`;
- a bucket whose later member is opened by a process after an earlier
  member was deleted: the later member is skipped (`in_use`) when the scan
  used for the earlier member is older than the reuse limit or the bucket
  spans a suspend ([safety.md](safety.md#in-use), Freshness); an open within
  the reuse window is the accepted limit 5 and is pinned as such;
- a host too busy for a fresh in-use scan (every scan finishing older
  than the freshness bound): nothing is deleted on a stale scan, the
  candidate is skipped `in_use_unknown` after its three attempts and the
  next one is still tried, and after three in a row the tier's rest is
  skipped unchecked, any other outcome resetting the count
  (`vectors/inuse/busy-host-*`, TASK-144);
- a stale index (deep modification, a new `.diskgc-keep`, a new Cargo
  target inside a `tmp-entry` candidate, a new protected match after the
  tally): the deletion-time checks refuse what they must;
- new candidates of a stale tier created after the snapshot: they are
  deleted before any pressure tier runs (per-tier discovery);
- a `TMPDIR` changed by `export` after `exec`: documented as invisible (the
  test pins the heuristic's known limit);
- deadline overruns: a blocking operation that exceeds the deadline is
  reported as an overrun, and no new work starts after it;
- command actions under `--allow-root`: refused at load.
- `command_action_child_holds_host_lock_after_parent_is_killed`: the
  `Delete::run_command` child starts in a checked directory with the host
  lock inherited; after the driver is killed, the child alone keeps the
  lock until it exits.
- `command_action_never_changes_inherited_stderr_flags`: the harness keeps
  a duplicate of the blocking stderr pipe's write end; its status flags
  stay blocking while a command runs and after the driver is killed.
- `command_action_timeout_kills_descendant_after_leader_exits`: a leader
  exits on TERM while its descendant ignores TERM; the command port keeps
  the leader's process-group ID pinned until KILL has stopped the descendant,
  and forwards output the leader emitted on TERM under a separate bounded
  post-deadline window.
- `command_action_long_output_is_bounded_and_prefixed`: two concurrent
  128 KiB streams without newlines must complete and arrive on stderr as
  bounded prefixed segments, with no child bytes on result-only stdout.
- `command_action_orphaned_output_is_killed_before_reap`: a leader exits
  normally while its descendant holds the output pipes open; the bounded
  drain fails, kills the still-reserved process group, and releases the lock.
- `command_action_stalled_stderr_fails_without_hanging`: an unread full
  parent stderr pipe fails output forwarding promptly and kills the child
  group.
- `command_action_leader_exit_drain_obeys_tier_deadline`: a descendant holds
  output open after leader exit, but draining ends at the tier deadline.
- `command_action_closed_pipe_descendant_is_killed`: a descendant closes both
  output pipes and still gets killed before the successful leader is reaped.

### Producer liveness vectors

Status: the Owner PID (`nix-shell`) and `pytest` vectors and container
tests below are checked in and run (TASK-112); the project-contract
vectors and container tests (`node-project`, `python-project`) are
specified here and land with TASK-79.
TASK-122's classification adds, in the same directory: one
counterexample per withdrawn built-in of class *none*, named
`<profile>-...-eligible` (a live or deliberately kept producer's directory
deleted), now against a user probe profile `<profile>-probe` with the
withdrawn matching set, as `containers-image-age`; and for each admitted
verified heuristic the vectors its row names (`*-residual`, and the version
profiles' executable evidence
`claude-code-versions-running-old-version-exe-in-use`). The withdrawn
built-ins' own vectors are in `vectors/withdrawn/` and every other vector
that selected one uses its probe profile (TASK-122).
The Owner PID cases below are checked-in vectors against the
`nix-shell-rc` built-in in `crates/diskgc-model/tests/vectors/liveness/`,
one file per row and named after it (step 3 wrote them against a user
profile, step 4 moved them to the built-in; the unlisted-filesystem row is
one file each for `overlay` and `zfs`, since `virtiofs` and `9p` are
refused before any contract as filesystems that may block), plus
`nix-shell-rc-owner-pid-taken-after-listing-refused` (the pidfd probe).
Two keep a user profile with `liveness_contract: nix-shell`
(`nix-shell-probe`), since a user profile is their point:
`nix-shell-union-contract-vetoes` (the union rule) and
`nix-shell-command-action-live-owner-refused` (a command action is judged
too). The precondition rows assert the report's `detail`.
`vectors/builtins/nix-shell-rc.yaml` covers the built-in's roles (both
shapes reclaimed, `in_use` by an fd, `protected`, `kept` by
`.diskgc-keep`, a name without a valid pid `filtered`, `nix-shell.XXXXXX`
and rc shapes below a shell temp directory not matched). The load-time
cases and properties are Rust tests. The model has no parent links, so
the "setsid" and "nohup" children of the orphan and detached-child rows
are plain processes with or without the reference; nor process identity,
so the reused-pid row differs from the live-owner row only in intent. The
pytest vectors landed with step 5: every row of the pytest table below is a
file of that name in `vectors/liveness/` (against the built-in), plus
`vectors/builtins/pytest-tmp.yaml` for the built-in's roles (a plain
session and one holding a test's Git fixture reclaimed under `checkouts:
allow`, `in_use` by an fd, `protected`, `kept`, `pytest-05` `filtered`, a
test directory and sessions outside the temp-prefix preset not matched).
The project vectors land with TASK-79. Mutation record (step 3, each vector-corpus run red):
skipping the namespace, `hidepid` or filesystem precondition; tabling only
the first `NSpid` level; ignoring unreadable processes; ignoring the
probe; dropping the contract check in the plan, before the delete or
before a command action; checking only the selected union term in the
reclaim; dropping the union veto in the plan; letting a name that is no
instance veto a union. The name rule before the
walk is a cheaper copy of the contract's own check (equivalent at vector
level).
Tabling only the innermost level is equivalent at vector level (the probe
finds every outer level, which is in diskgc's namespace); the model test
`the_scan_tables_every_level_and_drops_gone_processes` pins it.
Mutation record (step 4, built-in; vector corpus, golden and unit tests):
dropping `liveness_contract` from the built-in (the built-in live-owner,
namespace, reused, zombie, nested and `hidepid` vectors and the frozen
definition), dropping its `min_idle_for` (`nix-shell-rc-fresh-not-idle`,
frozen definition), dropping the `nix-develop-*-*` pattern (its `examples`,
golden set, `builtins/nix-shell-rc.yaml`), skipping `hidepid` (the three
`hidepid` vectors), skipping the namespace (the nested vector), naming the
namespace before `hidepid` (`a_failed_precondition_names_its_rule_hidepid_first`),
tabling only the first level and ignoring a listed carrier
(`nix-shell-rc-namespace-local-pid-refused`), skipping the filesystem,
letting a live probe permit and ignoring unreadable `NSpid` (their vectors
and the Owner PID property) all bite. Through the real binary: dropping the contract
fails `nix_shell_rc_shell_in_private_pid_namespace_is_kept` (deletion
oracle), skipping the namespace fails
`owner_pid_candidates_are_kept_when_diskgc_has_its_own_pid_namespace`
(deletion oracle), naming the namespace first fails
`owner_pid_is_unknown_under_hidepid` (its detail); skipping the
filesystem rule fails `owner_pid_is_unknown_on_an_unlisted_filesystem`
(deletion oracle: the overlay candidate deleted; step 7, 2026-10-24).
Mutation record (step 5, `pytest`; vector corpus, the verdict's unit and
property tests, profile tests; every vector-level bite below):
dropping `liveness_contract` from `pytest-tmp` (44 vectors); an ended lock
owner permitting alone (`killed-newest-session`, both orphan-worker
vectors); ignoring the lock's age in row 7
(`young-lock-ended-owner-below-rank-kept`,
`lock-within-dead-lock-margin-kept`); dropping the dead-lock margin
(`lock-within-dead-lock-margin-kept`); row 7 permitting a young session
while a pytest runs (four vectors); an undecidable naming blocking row 7
(`cmdline-unreadable`, `environ-unreadable`, `basetemp-separate-relative`);
undecidable naming, undecidable root use or an incomplete process list
falling through to eligible (their vectors); counting other owners'
processes (`other-owner-process-not-in-set`); the naming veto after the
rank (`basetemp-within-rank-refused`); `--basetemp` by bytes only (the
three `basetemp` identity vectors); resolving a variable pytest does not
use (`other-variable-not-resolved`); counting an empty value
(`empty-tmpdir-ignored`); keeping two sessions instead of three (32
vectors); dropping `TEMP`, `TMP` or `PYTEST_DEBUG_TEMPROOT` (one vector
each); a relative value treated as resolved (four vectors); ignoring the
process's view for the fallback roots (`root-via-mount-view`); looking the
values up in another process's root (14 vectors; resolving in diskgc's own
root is a `LinuxHost` mutation no model vector can see, a limit); row 6 off (the four naming
vectors); an unreadable environment taken as harmless; a privilege
boundary not excused; the execnet bootstrap not a running sign
(`xdist-bootstrap-counts-as-running`); the old session measured on the
directory alone (`old-dir-young-tree-kept`); leading zeros not counted in
`M` (`rank-ignores-other-names`); the working-directory extra dropped;
`PYTEST_CURRENT_TEST` not a running sign; a lock owner that may be in use
taken as ended (`live-lock-refused`, `lock-namespace-pid-refused`); a
failed listing ignored (`listing-unreadable`); the fallback `/tmp` by path
dropped (`fallback-root-counts-for-a-chrooted-process`); `PYTEST_ADDOPTS`
ignored; an unreadable command line taken as empty; `R` not checked to
hold `P` (`root-replaced-unknown`); `P`'s owner not checked against the
judged users (`pytest-of-owner-not-judged-unknown`); a fallback lookup
error taken as no root (`fallback-root-lookup-error-unknown`); `ESRCH`
from a dead `/proc` root taken as undecidable
(`dead-procfs-root-not-a-temp-root`); the facts read only in the scan's
first listing round (`child-forked-during-a-read-is-read`; all from the
step's reviews). Equivalent at every
level: dropping the `.lock` type check (the lock is opened without
following a symlink and read as a regular file, which refuse the same
objects). Through the real binary: an ended owner permitting fails
`pytest_orphan_xdist_worker_keeps_session`, dropping the contract fails
`pytest_live_policy_none_session_in_shell_root_is_kept`, the naming veto
off fails `pytest_live_basetemp_session_is_kept` (deletion oracle each).
The contracts are in [safety.md](safety.md#producer-liveness), the schema
in [reference.md](reference.md#liveness-contract-predicate). Every
case below is a checked-in profile vector
(`crates/diskgc-model/tests/vectors/liveness/<name>.yaml`, `profile_vector:
1`) run by `tests/profile_vectors.rs`, dry run and reclaim, with candidates
older than the source's `min_idle_for` unless the case says otherwise. A
case's "Expected" column gives the plan reason (none: eligible and
removed in reclaim) and, after a comma, the `process_blockers` evidence.
Each counterexample that kept a profile unshipped is a vector here or, for
an excluded or withdrawn profile, stays as its unsafe user-probe vector.
TASK-121's corner-case corpus takes these as its liveness part.

Extensions they need. Of the profile-vector file:

- none so far: expected evidence is `expect.skipped[].process_blockers`
  (TASK-121's runner); plan rows carry no evidence in the report, so a
  plan expectation checks the reason only.

Of the host model (the test-model schema, tentative). `pid_namespace`,
`proc_hidepid`, `nspid` and `zombie` landed with Owner PID (TASK-112 step
2), `cmdline` and `view` with the pytest contract (step 5; `view` is read
only by `proc_root_lookup`, which resolves a value in the process's view,
following symlinks there with `..` clamped at its root, as
`RESOLVE_IN_ROOT` does; the pytest vectors need no other extension: a
file's `content` and `fail` events on a listing already existed); each
other extension lands with the contract that reads it (no field without a
reader):

- top level `pid_namespace: initial | nested` (default `initial`): the
  answer of diskgc's held `/proc` to the initial-namespace check;
- top level `proc_hidepid: true` (default false): the held `/proc` is
  mounted with `hidepid`; the mounts' existing `fs:` names the type the
  local-filesystem list is checked against;
- `processes[].nspid`: its pids from diskgc's namespace down to its own
  (default `[pid]`; the first must equal `pid`), or `unreadable`;
- `processes[].zombie: true`: listed, state `Z`, its `status` (`NSpid`)
  readable and its pidfd open, every other reader `Gone` (landed);
- `processes[].cmdline`: its argument vector (default `[exe]`), or
  `unreadable`;
- `processes[].view`: path prefixes as its root and mount namespace see
  them (`{/tmp: /tmp/nix-shell.AB}`; default: the model's own paths), used
  by temp-root lookups, which follow symlinks inside that view;
- a tree entry's `ctime:` and `atime:` (default: its mtime; today mtime
  stands for all three times), and a mount's `noatime: true`;
- `processes[].fds[].link_text` and `maps[]` entries with `link_text` or
  `dev_ino`: what diskgc reads when it differs from the object (another
  root's or mount namespace's rendering, a file renamed after open, an
  overlay layer's pair); default the object's own path and identity;
- `processes[].mnt_ns`: a mount namespace label (default diskgc's), with
  `processes[].mountinfo` (its mount table lines: id, mount point, root
  field, device) and per held file a `mnt_id`;
- a mount's `bind_of` extended to a subdirectory (`bind_of: {mount: 2,
  path: /w/packages/a}`); today only whole-filesystem binds exist;
- a file's `content:` (exists) for pytest's `.lock`.

**Owner PID** (`nix-shell-rc`):

| Vector | Setup | Expected |
|---|---|---|
| `nix-shell-rc-dead-owner-eligible` | `/tmp/nix-shell-4242-17/{env-vars,rc}` and an empty `/tmp/nix-develop-4243-18`; no process 4242 or 4243; a neighbouring `/tmp/nix-shell.AB` | both eligible and removed; the neighbour preserved (not matched) |
| `nix-shell-rc-live-owner-no-fd-refused` | process 4242 (a shell) with no fd, cwd or environment path in the candidate | `in_use`, `owner_pid`: the TASK-68 counterexample, refused |
| `nix-develop-rc-live-owner-no-fd-refused` | the same for `nix-develop-4242-9` | `in_use`, `owner_pid` |
| `nix-shell-rc-namespace-local-pid-refused` | `nix-develop-9-77`; process 3025375 with `nspid: [3025375, 9]`; no host pid 9 | `in_use`, `owner_pid`: the TASK-39 PID-namespace counterexample, refused |
| `nix-shell-rc-outer-level-pid-refused` | `nix-develop-3025375-1`; process with `nspid: [3025375, 9]` | `in_use` (any level counts) |
| `nix-shell-rc-reused-pid-refused` | the shell is gone; an unrelated `sleep` now has pid 4242 | `in_use` (reuse retains, never deletes) |
| `nix-shell-rc-zombie-owner-refused` | process 4242 `zombie: true` | `in_use` |
| `nix-shell-rc-nested-namespace-unknown` | `pid_namespace: nested`; no process 4242 | `producer_unknown` |
| `nix-shell-rc-hidepid-unknown` | `proc_hidepid: true`; no process 4242 | `producer_unknown` (a hidden `newgrp` shell would be unlisted) |
| `nix-shell-rc-hidepid-root-unknown` | as above with `own_uid: 0` | `producer_unknown` (root does not lift the precondition) |
| `nix-shell-rc-unlisted-filesystem-unknown` | one candidate each on mounts of type `virtiofs`, `9p`, `overlay`, `zfs`; no owners | `producer_unknown` each (one file per type, so dropping one bites) |
| `nix-shell-rc-unreadable-status-unknown` | owner gone; process 77 of the user has `nspid: unreadable` | `producer_unknown`, `unreadable` 77 |
| `nix-shell-rc-other-user-unreadable-status-unknown` | owner gone; a uid 2000 process 78 has `nspid: unreadable` | `producer_unknown`, `unreadable` 78 (Owner PID's set is every user's processes) |
| `nix-shell-rc-process-exits-mid-scan-eligible` | owner gone; another process becomes `Gone` between its listing and its status read | eligible: a confirmed exit does not make the answer unknown |
| `nix-shell-rc-malformed-pid-filtered` | `nix-shell-0042-1`, `nix-shell-4194305-1`, `nix-shell-abc-1` | `filtered` each (not an instance), all preserved |
| `nix-shell-rc-orphan-consumer-environ-refused` | owner gone; a `setsid` child (process 99, parent 1) has `TMPDIR=/tmp/nix-shell-4242-17` in its environment | `in_use`, `environment`: the contract does not bypass the in-use scan |
| `nix-shell-rc-detached-child-without-reference-eligible` | owner gone; a `nohup` child of the shell lives on with no fd, cwd or environment path in the candidate | eligible: Owner PID's scope limit, pinned so it is visible |
| `nix-shell-rc-owner-exit-after-read-stays-refused` | an `exit` event for 4242 after its status read | `in_use` for that scan (a read only adds) |
| `nix-shell-rc-fresh-not-idle` | owner gone; the directory changed 10 minutes before the clock | `not_idle` (below the built-in's floor) |
| `nix-shell-rc-replaced-before-unlink-changed` | owner gone; a `replace` event on the candidate after its verdict | `changed`; the replacement preserved |

**pytest sessions** (`pytest-tmp`; each candidate is a session; rows are
those of safety.md's verdict table). Layouts: **A** is
`/tmp/pytest-of-tester` (owner 1000), whose temp root `/tmp` is always
used; **B** is `/tmp/nix-shell.AB/pytest-of-tester` (owner 1000). `{0..4}`
means sessions `pytest-0` to `pytest-4`, with no `.lock` and mtime and
ctime ten days before the clock unless stated; then `M` is 4, sessions 0
and 1 are below the rank, 2 to 4 above. *proc*: a uid 1000 shell without
`TMPDIR`; *py*: a uid 1000 process with `cmdline: [python, -m, pytest]`.
"Expected" lists every session of `P`.

| Vector | Setup | Expected |
|---|---|---|
| `pytest-tmp-live-no-lock-newest-refused` | the TASK-65 counterexample with the built-in: A `{0}` holding `test_active0`; *py* alive, no fd in it | 0 `in_use`, `temp_root` (row 10) |
| `pytest-tmp-live-lock-refused` | A `{0}`, `pytest-0/.lock` holds `42`; process 42 | 0 `in_use`, `owner_pid` 42 (row 4) |
| `pytest-tmp-killed-newest-session-refused` | A `{0}`, lock `42`; no process 42; *proc* | 0 `in_use`, `temp_root` (row 10): an ended owner alone never permits |
| `pytest-tmp-killed-session-dead-root-eligible` | B `{0}`, lock `42`; no process 42; *proc* | 0 eligible (row 11) |
| `pytest-tmp-orphan-xdist-worker-refused` | B `{0}`, lock `42` one day old; no process 42; worker 43 with `cmdline: [python, -u, -c, "import sys;exec(eval(sys.stdin.readline()))"]` (the execnet bootstrap; no `PYTEST_XDIST_*` in its initial environment) and `TMPDIR=/tmp/nix-shell.AB` | 0 `in_use`, `temp_root` (row 10) |
| `pytest-tmp-orphan-xdist-worker-within-rank-refused` | A `{0..4}`, `pytest-0/.lock` holds `42`, one day old; no process 42; worker 43 with `cmdline: [python, -u, -c, "import sys;exec(eval(sys.stdin.readline()))"]` (the execnet bootstrap; no `PYTEST_XDIST_*` in its initial environment) and `TMPDIR=/tmp/other`; *proc* | 0 `in_use`, `temp_root` (the young lock fails row 7; row 10); 1 eligible (row 7: the bootstrap counts as a running pytest, the session is old); 2, 3, 4 `in_use`, `temp_root` |
| `pytest-tmp-stale-lock-within-rank-eligible` | as above, the lock four days old, no worker | 0, 1 eligible (row 7; pytest's own dead-lock rule); 2, 3, 4 `in_use` |
| `pytest-tmp-lock-not-regular-within-rank-unknown` | A `{0..4}`, `pytest-0/.lock` a symlink to a four-day-old file; *proc* | 0 `producer_unknown` (row 3); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-beyond-rank-eligible` | A `{0..4}`; *proc* | 0, 1 eligible (row 7); 2, 3, 4 `in_use`, `temp_root` |
| `pytest-tmp-live-lockless-below-rank-young-kept` | A `{0..4}`, `pytest-0` one day old; *py* alive | 0 `in_use`, `temp_root` (row 7 needs an old session while a pytest runs; row 10); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-old-dir-young-tree-kept` | A `{0..4}`; `pytest-0` itself ten days old but `pytest-0/test_x0/out` changed one day ago; *py* alive | 0 `in_use`, `temp_root` (old session is measured over the whole tree; row 10); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-test-subprocess-counts-as-running` | A `{0..4}`, `pytest-0` one day old; the only pytest sign is a child with `PYTEST_CURRENT_TEST=t.py::test_a (call)` in its environment | 0 `in_use`, `temp_root` (row 7 sees a pytest running); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-embedded-pytest-residual` | A `{0..4}`, `pytest-0` one day old, no lock; a live `bazel-bin/t` process with none of the signs | 0 eligible (row 7, the stated residual; pytest's own next cleanup deletes it too); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-young-below-rank-unused-root-eligible` | B `{0..4}`, `pytest-0` one day old; *py* alive with `TMPDIR=/tmp/other` | 0 eligible (row 11, the stated residual); 1 eligible (row 7); 2, 3, 4 eligible (row 11) |
| `pytest-tmp-rank-ignores-other-names` | A `{0..4}` plus `pytest-current` (symlink), `garbage-x`, `pytest-9x`, `pytest-05` (a directory); *proc* | `M` is 5 (`pytest-05` counts, as for pytest): 0, 1, 2 eligible; 3, 4 `in_use`; `pytest-05`, `pytest-9x` `filtered`; the symlink and `garbage-x` never candidates |
| `pytest-tmp-session-number-overflow-filtered` | A `{0..4}` plus `pytest-99999999999999999999`; *proc* | that entry `filtered` and not counted in `M`; 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-dead-shell-root-eligible` | B `{0..2}`; *proc* | 0, 1, 2 eligible (row 11) |
| `pytest-tmp-live-shell-root-refused` | B `{0..4}`; a uid 1000 process with `TMPDIR=/tmp/nix-shell.AB` | 0, 1 eligible; 2, 3, 4 `in_use`, `temp_root` |
| `pytest-tmp-root-named-by-temp-refused`, `-by-tmp-refused`, `-by-debug-temproot-refused` | as above, the root named only by `TEMP`, `TMP`, or `PYTEST_DEBUG_TEMPROOT` (one file each) | 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-empty-tmpdir-ignored` | B `{0..2}`; a process with `TMPDIR=` (empty) | 0, 1, 2 eligible |
| `pytest-tmp-relative-tmpdir-unknown` | B `{0..4}`; a process with cwd `/tmp` and `TMPDIR=nix-shell.AB` | 0, 1 eligible (row 7); 2, 3, 4 `producer_unknown`, `unresolved_path` (row 9) |
| `pytest-tmp-relative-tmpdir-cwd-elsewhere-unknown` | as above with cwd `/home/tester` (where the process may have moved since it read the value) | the same: a relative value is never resolved |
| `pytest-tmp-tmpdir-dotdot-clamped-refused` | B `{0..4}`; `TMPDIR=/../tmp/nix-shell.AB` | 0, 1 eligible; 2, 3, 4 `in_use`, `temp_root` (`..` clamps at the root) |
| `pytest-tmp-cwd-is-root-refused` | B `{0..4}`; a process whose cwd is `/tmp/nix-shell.AB` | 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-root-via-mount-view-refused` | B `{0..4}`; a process without `TMPDIR`, `view: {/tmp: /tmp/nix-shell.AB}` | 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-root-via-symlink-refused` | B `{0..4}`; `TMPDIR=/tmp/ab-link`, a symlink to `/tmp/nix-shell.AB` | 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-root-via-symlink-in-view-refused` | B `{0..4}`; `view: {/tmp: /tmp/nix-shell.AB, /x: /var/x}`, `TMPDIR=/x/link`, `/var/x/link` the absolute symlink `/tmp` | 0, 1 eligible; 2, 3, 4 `in_use` (resolved in the process's root) |
| `pytest-tmp-basetemp-argv-refused` | B `{0..4}`; *py* with `--basetemp=/tmp/nix-shell.AB/pytest-of-tester/pytest-3` and `TMPDIR=/tmp/other` | 3 `in_use`, `command_line` (row 6); 0, 1 eligible (row 7); 2, 4 eligible (row 11) |
| `pytest-tmp-basetemp-via-symlink-refused` | as above with `--basetemp=/tmp/ab-link/pytest-of-tester/pytest-3` | the same (identity, not bytes) |
| `pytest-tmp-basetemp-within-rank-refused` | as above with `--basetemp=` `pytest-0` | 0 `in_use`, `command_line` (row 6 before row 7); 1 eligible; 2, 3, 4 eligible |
| `pytest-tmp-basetemp-addopts-refused` | B `{0..4}`; a process with `PYTEST_ADDOPTS=--basetemp=/tmp/nix-shell.AB/pytest-of-tester/pytest-3`, `TMPDIR=/tmp/other` | 3 `in_use`, `environment`; 0, 1 eligible; 2, 4 eligible |
| `pytest-tmp-basetemp-separate-relative-unknown` | B `{0..4}`; `cmdline: [pytest, --basetemp, pytest-of-tester/pytest-3]`, cwd `/tmp/nix-shell.AB` | 0, 1 eligible (row 7); 2, 3, 4 `producer_unknown`, `unresolved_path` (row 8) |
| `pytest-tmp-basetemp-not-session-shaped-not-matched` | B `{0}` plus `mytmp`; *py* with `--basetemp=/tmp/nix-shell.AB/mytmp` | `mytmp` no candidate; 0 eligible |
| `pytest-tmp-basetemp-config-within-rank-kept-while-pytest-runs` | A `{0..4}`, `pytest-0` two hours old; *py* alive (its `--basetemp` into `pytest-0` comes from a configuration file) | 0 `in_use`, `temp_root` (Mark's fail-closed condition on row 7); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-basetemp-config-within-rank-old-eligible` | as above, `pytest-0` four days old | 0, 1 eligible (row 7: pytest's own cleanup would delete 0 too; the producer-equivalence case); 2, 3, 4 `in_use` |
| `pytest-tmp-basetemp-config-above-rank-residual` | B `{0}` two hours old; *py* with `TMPDIR=/tmp/other`, its `--basetemp` into `pytest-0` from a configuration file | 0 eligible (row 11, the stated residual) |
| `pytest-tmp-cmdline-unreadable-unknown` | B `{0..4}`; a uid 1000 process with `cmdline: unreadable` | 0, 1 eligible (row 7: a pytest counts as running, the sessions are old); 2, 3, 4 `producer_unknown`, `unreadable` (row 8) |
| `pytest-tmp-two-causes-sorted` | B `{0..4}`; processes 91 and 90 of the user, each with a relative `TMPDIR` | 0, 1 eligible; 2, 3, 4 `producer_unknown`, `process_blockers` `[{pid: 90, evidence: unresolved_path}, {pid: 91, evidence: unresolved_path}]` |
| `pytest-tmp-environ-unreadable-unknown` | B `{0..4}`; a uid 1000 process whose environment reads `Failed`, not excused | 0, 1 eligible; 2, 3, 4 `producer_unknown`, `unreadable` |
| `pytest-tmp-other-user-environ-unreadable-ignored` | B `{0..4}`; a uid 0 daemon whose environment is unreadable (`proc: denied`); no other process | 0, 1 eligible (row 7); 2, 3, 4 eligible (row 11): not in the process set |
| `pytest-tmp-excused-process-ignored` | B `{0..4}`; the only other process is the user's `non_dumpable` agent | 0 to 4 eligible (the stated residual) |
| `pytest-tmp-lock-namespace-pid-refused` | A `{0}`, lock `9`; a process with `pid: 5000`, `nspid: [5000, 9]` | 0 `in_use`, `owner_pid` 5000 |
| `pytest-tmp-lock-malformed-unknown` | A `{0..6}` whose `.lock` is: empty; `0042`; `42 43`; 17 bytes; `abc`; a directory; a symlink | 0 to 6 `producer_unknown` (row 3) |
| `pytest-tmp-nested-namespace` | `pid_namespace: nested`; B `{0..4}`, `pytest-0/.lock` holds `42` | 0 `producer_unknown` (row 5); 1 eligible (row 7: a pytest counts as running, the session is old); 2, 3, 4 `producer_unknown` (row 8) |
| `pytest-tmp-hidepid-unknown` | `proc_hidepid: true`; otherwise as above | the same |
| `pytest-tmp-fresh-session-not-idle` | B `{0}` made 10 minutes before the clock; no processes | 0 `not_idle` (below the built-in's floor) |
| `pytest-tmp-listing-unreadable` | B `{0..4}`; a `fail: EACCES` event on the second listing of `pytest-of-tester` | 0 to 4 `unreadable` (row 2) |
| `pytest-tmp-claude-root` | `/tmp/claude-1000/pytest-of-tester` `{0..4}`; a process with `TMPDIR=/tmp/claude-1000` | 0, 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-other-variable-not-resolved` | B `{0..2}`; a process with `XDG_RUNTIME_DIR=/tmp/nix-shell.AB` and no temp-directory variable | 0, 1, 2 eligible (only pytest's variables are resolved) |
| `pytest-tmp-young-lock-ended-owner-below-rank-kept` | A `{0..4}`, `pytest-0/.lock` holds `42`, one day old; no process 42; *proc* | 0 `in_use`, `temp_root` (a young lock fails row 7 with no pytest running); 1 eligible; 2, 3, 4 `in_use` |
| `pytest-tmp-lock-within-dead-lock-margin-kept` | as above, the lock three days and 30 s old | the same (pytest's three days passed, diskgc's extra minute not) |
| `pytest-tmp-xdist-bootstrap-counts-as-running` | B `{0..4}`, `pytest-0` one day old; an execnet worker (bootstrap command line, `TMPDIR=/tmp/other`) and a process 900 with a relative `TMPDIR` | 0 `producer_unknown`, `unresolved_path` 900 (row 7 refused: a pytest runs; row 9); 1 eligible; 2, 3, 4 `producer_unknown` |
| `pytest-tmp-other-owner-process-not-in-set` | `--allow-root`, owners tester and a uid 2000 user; B `{0..4}` owned by tester; only a uid 2000 process with `TMPDIR=/tmp/nix-shell.AB` | 0 to 4 eligible: another owner's process is not in the session owner's set |
| `pytest-tmp-fallback-root-counts-for-a-chrooted-process` | A `{0..4}`; a process with `root: /home/tester` (its own `/tmp` does not resolve) | 0, 1 eligible; 2, 3, 4 `in_use`, `temp_root`: `R` is `/tmp` by diskgc's path, always counted |
| `pytest-tmp-pytest-of-owner-not-judged-unknown` | `/tmp/nix-shell.AB/pytest-of-other` owned by uid 2000 holding the user's `pytest-0` | 0 `producer_unknown`: that uid's processes are not read |
| `pytest-tmp-root-replaced-unknown` | B `{0}`; `/tmp/nix-shell.AB` renamed away just before the contract's listing and a new directory made under its name | 0 `producer_unknown`: `R` by path no longer holds `P` |
| `pytest-tmp-fallback-root-lookup-error-unknown` | as `pytest-tmp-root-via-mount-view-refused`, the lookup of the process's `/tmp` failing with `EIO` | 0, 1 eligible; 2, 3, 4 `producer_unknown`, `unresolved_path` (row 9: not "no such root") |
| `pytest-tmp-process-ending-during-its-read-is-gone` | B `{0..4}`; the only process, with `TMPDIR` the root, ends during its own read (before its `TMPDIR` lookup) | 0 to 4 eligible: it is gone, as In use takes it |
| `pytest-tmp-child-forked-during-a-read-is-read` | B `{0..4}`; a pytest with `--basetemp` into `pytest-3` and `TMPDIR` the root forks a child during its read and ends | the child is listed and read: 3 `in_use`, `command_line` 901; 2, 4 `in_use`, `temp_root` 901; 0, 1 eligible (row 7) |
| `pytest-tmp-missing-absolute-tmpdir-unknown` | B `{0..4}`; a process with `TMPDIR=/tmp/nix-shell.GONE` (absent) | 0, 1 eligible; 2, 3, 4 `producer_unknown`, `unresolved_path` (row 9) |
| `pytest-tmp-dead-procfs-root-not-a-temp-root` | B `{0..2}`; *proc*'s lookups of `/tmp`, `/var/tmp` and `/usr/tmp` in its root fail with `ESRCH` (a sandboxed renderer's dead `/proc` root) | 0, 1, 2 eligible (row 11): not undecidable |

`pytest-tmp-listing-unreadable` fails every listing of `pytest-of-tester`
after discovery's first (`read_dir` calls 2 to 6, one per candidate's
check, in the dry run and in the reclaim alike).

**Project dependency directories** (`node-modules`, `python-venv`; the
project is the candidate's parent; idle bound 30 days; TASK-79's container
probe, where reading the dependencies changed neither mtime nor ctime, is
the counterexample these turn into cases):

| Vector | Setup | Expected |
|---|---|---|
| `node-project-idle-eligible` | `/home/tester/p/{package.json,src/a.js,node_modules/x/i.js}`, all changed and read 60 days before the clock; a uid 1000 shell holding `/home/tester/.bash_history` (same filesystem, outside the project) and nothing in `p` | eligible, removed; `package.json` and `src` preserved |
| `node-project-recent-edit-kept` | as above, `src/a.js` changed 2 days ago | `not_idle` |
| `node-project-recent-install-kept` | only `node_modules/x/i.js` changed 2 days ago | `not_idle` (the candidate's own walk counts) |
| `node-project-install-old-mtimes-new-ctime-kept` | every mtime 60 days old (the normalised dates an install extracts); `node_modules/x/i.js` with `ctime` 2 days ago | `not_idle` |
| `node-project-dir-atime-only-eligible` | all old; only directory atimes in the project recent (a lister) | eligible: directory atimes do not count |
| `node-project-nested-node-modules-filtered` | `p/node_modules/a/{package.json,node_modules/b/i.js}` as well as `p/node_modules`; the project active | the nested one `filtered`, never deleted, also when the outer one is kept |
| `node-project-workspace-member-root-active-kept` | `w/{package.json (with workspaces),src/x.js changed yesterday,packages/a/{package.json,node_modules}}`, `packages/a` itself 60 days old | `w/packages/a/node_modules` `not_idle`: judged by the workspace |
| `node-project-vcs-top-level-is-the-project` | as above, `w` without `workspaces` but holding `.git`; `w/src` changed yesterday | `not_idle` |
| `node-project-read-only-use-kept` | TASK-79's case: nothing changed for 60 days, `node_modules/x/i.js` read (`atime`) yesterday | `not_idle` |
| `node-project-read-only-use-noatime-residual-eligible` | the same on a `noatime` mount | eligible: the stated residual, pinned |
| `node-project-cwd-in-project-kept` | all old; a process with cwd `/home/tester/p/src` | `in_use`, `cwd` |
| `node-project-fd-in-project-kept` | all old; a process with `src/a.js` open | `in_use`, `fd` |
| `node-project-environ-names-project-kept` | all old; a process with `NODE_PATH=/home/tester/p/node_modules` | `in_use`, `environment` |
| `node-project-vcs-only-change-eligible` | all old except `.git/FETCH_HEAD` changed yesterday | eligible |
| `node-project-cwd-in-vcs-metadata-kept` | all old; a process with cwd `/home/tester/p/.git` | `in_use`, `cwd`: inside by path, though the walk leaves `.git` out |
| `node-project-fd-in-sibling-dependency-dir-kept` | all old; `p/other/node_modules/y.js` open in a process | `in_use`, `fd` |
| `node-project-keep-marker-kept` | all old; `p/node_modules/.diskgc-keep` | `kept`: the operator's remedy for hand-edited dependencies |
| `node-project-symlinked-node-modules-not-candidate` | `p/node_modules` a symlink to `/home/tester/shared/node_modules` | no candidate; the target preserved |
| `node-project-pnpm-layout-eligible` | all old; `p/node_modules/{.pnpm/x@1/node_modules/x/i.js (link: to /home/tester/.local/share/pnpm/store/v3/files/ab/cd),x (symlink to .pnpm/x@1/node_modules/x)}` | eligible; the store file preserved with one link fewer |
| `node-project-bind-mounted-member-unknown` | the workspace member `w/packages/a` also bind-mounted at `/home/tester/a` (`bind_of` subdirectory, `mountinfo` root `/w/packages/a`) and supplied as the root; `w/src` changed yesterday | `/home/tester/a/node_modules` `producer_unknown`: the climb reached a bind-mounted subdirectory |
| `node-project-held-file-other-device-not-inside` | all old; a process holds `/home/tester/elsewhere/z` on another filesystem; its link text spoofed in the model to `/home/tester/p/z` | eligible: a device not in the tree is never inside, and nothing outside the tree is resolved |
| `node-project-held-file-same-device-outside-eligible` | all old; a process holds `/home/tester/notes.txt` on the project's device | eligible: text outside the tree's path is not inside |
| `node-project-container-bind-work-kept` | all old; a process with `mnt_ns: c1`, `root: /`, its mount table binding `/home/tester/p` at `/work` (mount id 968, root field `/home/tester/p`), holding `/work/src/a.js` (link text `/work/src/a.js`) | `in_use`, `fd`: translated through its mount table |
| `node-project-container-chroot-bind-kept` | as above plus a `chroot` to `/jail` (root link text `/jail`, link text `/jail/work/src/a.js`, mount point `/work`) | `in_use`, `fd` |
| `node-project-container-chroot-outside-eligible` | all old; the chroot process of `node-project-container-chroot-bind-kept` instead holds `/jail/notes/n.txt` (link text `/jail/notes/n.txt`), its mount table binding `/home/tester/notes` at `/notes` on the project's device | eligible: translated and outside the project |
| `node-project-space-in-path-container-kept` | the project at `/home/tester/my proj`, all old; a container process binding it at `/work dir` (its `mountinfo` holds `/home/tester/my\040proj` and `/work\040dir`), holding `/work dir/src/a.js` | `in_use`, `fd`: mount points and root fields are unescaped first |
| `node-project-detached-mount-retained` | all old; a process in diskgc's namespace holds a file on the project's device through a lazily detached mount whose id is absent from `mountinfo` | `in_use` (untranslatable in the same namespace) |
| `node-project-map-files-unreadable-retained` | all old; a mapping of a project file on the project's device whose `map_files` link cannot be read and whose `maps` pair the walk did not see | `in_use` (untranslatable) |
| `node-project-untranslatable-retained` | all old; a process in another mount namespace holding a file on the project's device whose mount id is missing from its `mountinfo` | `in_use` (cannot be translated) |
| `node-project-held-file-suffix-mismatch-retained` | all old; a held file on the project's device whose link text has the project prefix but whose suffix resolves to another inode (renamed after open) | `in_use` (unresolvable on a tree device retains) |
| `node-project-subdir-bind-held-cwd-kept` | all old; `p/src` also bind-mounted at `/mnt/src` (`bind_of` subdirectory); a process with cwd `/mnt/src` | `in_use`, `cwd`: the climb meets a directory of the tree |
| `node-project-pivot-root-process-not-under` | all old; a process whose `root` is `/srv/c` and cwd `/srv/c/work`, outside the project | eligible: its climb reaches the fixpoint without meeting the tree |
| `node-project-ssh-agent-excused` | all old; the user's `non_dumpable` `ssh-agent` whose magic links cannot be opened | eligible: excused as In use excuses it |
| `node-project-overlay-mapping-retained` | all old; a mapping whose `maps` device and inode are a lower layer's, of a file in the project | `in_use` (the stated availability cost) |
| `node-project-chrooted-process-kept` | all old; a process with `root: /home/tester`, cwd `/home/tester/p/.git`, and `p/other/node_modules/y.js` open (link text is diskgc's view) | `in_use`, `cwd` and `fd`: matched by climbing and by identity, not by the process's own path text |
| `node-project-no-manifest-filtered` | no `package.json` in `p` | `filtered` |
| `node-project-over-budget` | a project tree above the entry budget | `too_many_entries` |
| `node-project-workspace-manifest-oversized-unknown` | `w/package.json` over 1 MiB above a member | `producer_unknown` |
| `node-project-environ-other-view-residual` | all old; a process with `view: {/src: /home/tester/p}` and `NODE_PATH=/src/node_modules` | eligible: environment strings are bytes in the process's view (the stated residual) |
| `node-project-fresh-floor` | everything changed 20 days ago | `not_idle` (below the built-in's floor) |
| `python-project-idle-eligible` | `/home/tester/q/{pyproject.toml,pkg/m.py,.venv/pyvenv.cfg,.venv/lib/x.py}`, all 60 days old | eligible |
| `python-project-read-only-use-kept` | TASK-79's case: `.venv/lib/x.py` read yesterday, nothing changed | `not_idle` |
| `python-project-no-pyvenv-cfg-filtered` | `.venv` without `pyvenv.cfg` | `filtered` |
| `python-project-editable-checkout-refused` | `.venv/src/pkg/.git/HEAD` | `contains_checkout` |
| `python-project-requirements-manifest` | only `requirements-dev.txt` as manifest, all old | eligible |

**Excluded and withdrawn profiles** keep their unsafe user-probe vectors
(`pytest-tmp-unsafe-live-no-fd` and its successors for a plain path
matcher; `containers-image-age`, the withdrawn built-in's matching set
as a user profile) and the container tests that require `mktemp-dir`,
`nix-shell-tmpdir`, `tmp-entry` and `containers-image-tmp` to stay absent
from the built-in library.

**Load-time cases** (Rust unit tests, one per rule of reference.md's
validation list): `liveness_contract_rejects_null_and_blank`,
`liveness_contract_rejects_unknown_values_mappings_and_lists`,
`liveness_contract_is_source_and_fs_only`,
`nix_shell_contract_requires_the_rc_name_shape`,
`pytest_contract_requires_the_session_layout`,
`node_project_contract_requires_node_modules`,
`project_contracts_refuse_checkouts_allow`.

**Properties.** The pid parser: every value from 1 to 4194304 formatted
in decimal parses back; leading zeros, signs, values above the limit and
non-digits never parse. The session-number grammar: canonical digits
fitting 64 bits parse; leading zeros are rejected as candidates but
counted in `M`; values over 64 bits are rejected and not counted. The
rc-name split: a name built from either shape and a valid pid yields
exactly that pid. The Owner PID answer: over generated process tables
(`nspid` lists, unreadable and gone entries of any user) and host facts
(namespace, `hidepid`, filesystem type), *no process carries the pid*
holds exactly when every precondition holds, no listed process carries
the pid at any level, and no process stayed unreadable. The pytest
verdict is a pure function of the facts of safety.md's verdict table
(session names, locks with their ages, session ages, `M`, and per process
its uids, command line, environment, cwd and resolution results); it
returns the verdict and the row that decided it, so tests assert which
rule permitted. Over generated inputs: every session gets exactly one
row; no session that a process names is eligible; row 7 never permits a
session with a lock younger than the dead-lock age, nor, while a pytest
of the user runs, a session younger than it. Row 7's invariant: against a
model of pytest 9.1.1's cleanup run by a session with the default
retention created at a time `t_new` at or after the check, on the same
snapshot of `P` (`int()` suffixes and a case-insensitive prefix, `M`
counting the new session, lock age measured from the new session's
mtime), every session row 7 permits is one that model deletes. Row 11 is
excluded from the invariant by design.

**Mutations that must bite** (the implementing task records each one and
the vector that went red): dropping `liveness_contract` from a built-in;
matching only the innermost `NSpid` level; skipping the namespace,
`hidepid` or filesystem precondition; ignoring another user's unreadable
process; letting an ended lock owner permit on its own; ignoring the lock's age in
the rank rule; dropping the dead-lock margin; letting row 7 permit a young
session while a pytest runs; letting an undecidable naming test block row
7; letting an undecidable test fall through to eligible; counting other
users' processes in a contract's process set; putting the naming veto after the rank; matching names by
bytes only; resolving strings other than the variables pytest uses;
counting an empty value; keeping two sessions instead of three; dropping one temp-root variable; resolving a
relative value from the current working directory instead of failing
closed; ignoring `view`; not following a
temp-root symlink, or following it in diskgc's root; skipping the
command-line and environment check for `S`; treating an unreadable
environment as harmless; not excusing a privilege-boundary process;
ignoring atime or ctime; counting directory atimes; counting VCS metadata
as project change; measuring the candidate's parent instead of its
workspace; deciding "inside the project" by the walk's inode set instead
of by path, or by the process's link text without an identity check;
resolving link text outside the tree, or across a device the tree does
not have; retaining for a held file whose text lies outside the tree;
skipping the root-link strip (killed by
`node-project-container-chroot-outside-eligible`, which the unstripped
text would wrongly retain, and by `node-project-container-chroot-bind-kept`)
or the mount-table translation; comparing escaped `mountinfo` fields; ending a climb at a level bound instead of the fixpoint;
comparing a climb with the sought directory only, not every directory of
the tree; climbing once per candidate; treating an unopenable magic link
of an excused process as unreadable; not matching the execnet bootstrap;
measuring an old session by the session directory alone;
climbing past a bind-mounted subdirectory; matching a nested dependency directory; measuring the
candidate instead of the project.

**Container tests** (the kernel's view matters). The two lifecycle tests
have landed (`diskgc-e2e/src/lifecycle_suite.rs`, tier `full`, Nix image
only); they run the pinned producers, never diskgc, from scenario scripts
that print one fact per line, and require exactly the expected set. Nix
runs against a chroot store in the tester's home, since the image's store
is read-only and has no database: legacy `nix-shell` of a derivation
without inputs, and `nix develop` of a ready dev environment, build
nothing. The others land with the steps that use them; the three Owner
PID tests landed with step 4 and the filesystem-rule one with step 7
(`diskgc-e2e/src/owner_pid_suite.rs`, tier `full`, `--pid=host`,
Container setup), the three pytest contract tests
with step 5 (`diskgc-e2e/src/pytest_contract_suite.rs`, tier `full`,
`--pid=host`: in a PID namespace of its own diskgc's process list is not
complete and every session above the rank is `producer_unknown`, row 8).
Each asserts the deciding row by the report's detail
(`liveness_contract pytest row N (rule): ...`), and every run is judged
by the deletion oracle; the sessions are made now, so the sandboxed
binary's clock is advanced two hours past the 1h floor and below the
3-day dead-lock age. `live_pytest_without_session_lock_is_unsafe_to_reclaim`
(tier `fast`, its own PID namespace) also requires the shipped
`pytest-tmp` to keep its live session (`producer_unknown`, row 8).
`nix_shell_rc_lifecycle_is_pinned` (pinned Nix: the directory's pid equals
the shell's for legacy `nix-shell` and `nix develop`, interactive and
`-c`; what remains after a normal exit, `SIGHUP` and `SIGKILL`; whether
the shell or a `setsid` child holds or names the directory after start),
`nix_shell_rc_shell_in_private_pid_namespace_is_kept` (Nix image: a
pinned `nix develop -c` shell in a nested PID namespace sharing `/tmp`,
its namespace-local pid moved by `ns_last_pid` into a range no host
process carries, since small ones collide with other namespaces' and only
retain: `in_use`, `owner_pid` with the shell's host pid, while it lives;
reclaimed, judged by the deletion oracle, once it is killed and reaped),
`owner_pid_candidates_are_kept_when_diskgc_has_its_own_pid_namespace`
(diskgc in a nested namespace: a dead and a live owner's directory both
`producer_unknown` naming the namespace rule, nothing deleted; then from
the initial namespace the dead one is reclaimed and the live one `in_use`),
`owner_pid_is_unknown_under_hidepid` (diskgc in a nested namespace whose
`/proc` has `hidepid=invisible`; a shell of tester's uid and another
group, as after `newgrp`, owns a directory; tester cannot list it, root
can; `producer_unknown` naming `hidepid` for tester and for root with
`--allow-root`. Rootless Podman cannot mount a `hidepid` `/proc` for the
initial namespace, so the rule is observed by its detail (safety.md, Owner
PID). From the initial namespace without
`hidepid` the shell is listed and its fd table is denied to tester, so In
use keeps the directory first, evidence `unreadable`),
`owner_pid_is_unknown_on_an_unlisted_filesystem` (TASK-112 step 7; in the
initial PID namespace, `--pid=host`: an rc directory whose owner number
no process carries, on a kernel overlayfs mounted in the container with
upper and lower on the scratch tmpfs, judged by a user profile with
`liveness_contract: nix-shell` rooted at the overlay: `producer_unknown`,
detail naming the filesystem rule, nothing deleted under the oracle;
control: the same number's directory on the tmpfs `/tmp` is reclaimed by
the built-in in the same namespace, so the overlay one is kept by the
filesystem rule alone. `virtiofs`, `9p` and ZFS are not mountable in the
rootless container; their rows stay model vectors, and `virtiofs` and
`9p` are refused as filesystems that may block before any contract),
`pytest_lifecycle_is_pinned` (pinned pytest: the lock by `keep`, holding
the pid as text, removed at exit; the exit cleanup by rank; a killed
session's lock below the rank kept while young and removed after three
days; a `keep` 0 session deleting a concurrent lockless one,
`--basetemp` making no lock, and pytest's own cleanup deleting a lockless
`--basetemp` directory shaped like a session with `N ≤ M − 3`, the fact
row 7's invariant rests on),
`pytest_live_policy_none_session_in_shell_root_is_kept` (a sleeping
`tmp_path_retention_policy=none` test with `TMPDIR` in a `nix-shell.*`
root, no lock and nothing held in the session: `in_use`, `temp_root` with
the pytest's pid, row 10; killed, it leaves the session, which is then
reclaimed by row 11),
`pytest_live_basetemp_session_is_kept` (a sleeping run with `--basetemp`
shaped like a session, `pytest-7`, in a dead shell root: `in_use`,
`command_line`, row 6; a leftover lockless `pytest-1` below the rank in the
same directory is reclaimed meanwhile, row 11, since a running pytest
makes row 7 need an old session; the `--basetemp` session is reclaimed
once the run is killed),
`pytest_orphan_xdist_worker_keeps_session` (pinned pytest-xdist 3.8.0 and
execnet 2.1.2: the controller, whose pid the `.lock` holds, is killed
while its worker sleeps in a test; the worker's command line is the
execnet bootstrap and its initial environment holds no `PYTEST_XDIST_*`;
`in_use`, `temp_root` with the worker's pid, row 10, not only the
outcome. execnet bounds an orphan's life: its receiver thread, seeing the
controller's pipe close, SIGINTs the worker after 5 s and `_exit`s it 10 s
later (`WorkerGateway._terminate_execution`), so the test's worker
ignores SIGINT and the test checks that the worker still lived after
diskgc's runs; once it has ended the session is reclaimed, row 11).
Planned with the project contracts (TASK-79), not yet written:
`node_and_python_project_read_keeps_dependencies`
(TASK-79's probe on a `relatime` tmpfs: a run that only reads keeps the
dependency directory; on a `noatime` mount the residual is shown),
`npm_install_leaves_old_mtimes_and_new_ctimes` (the pinned npm: after an
install from an offline tarball, the project is `not_idle` by ctime), and
`dry_run_leaves_file_atimes_unchanged` (a `relatime` mount: diskgc's dry
run over a project changes no regular file's atime).

### nix-store backend vectors

Designed (TASK-118), implemented by TASK-36 under
`crates/diskgc-model/tests/vectors/backend/` (`nix-*.yaml`, profile
`nix-garbage`, path `/nix/store`), in the shape of the Docker backend
vectors. The host model gains a `nix:` block (the model schema is
tentative, PRD): the client's canonical directory and `--version` text,
the answers to `nix config show store` and the pinned `store info` query,
the daemon probe's result (`serving`, `eof`, `timeout`, `wrong_magic`, or
an errno), the daemon's `--query --roots` lines, and the GC's effect (store paths it removes, its exit or
signal). The tree carries `/nix/store`, `/nix/var/nix` and its socket
with their owners and modes, so the layout checks run on the model's
`statx`. `expect.commands` pins every Nix argv and the exact child
environment; a vector that expects a skip expects no GC command, and one
that skips before step 8 also no `--query --roots`.

| Vector | Model | Expected |
|---|---|---|
| `nix-gc-collects-through-the-pinned-daemon` | multi-user layout, multi-call client 2.34.8 in `/nix/store`, daemon 2.34.8, roots answer lists every generation | plan `/nix/store` eligible, `estimated_bytes: null`; commands, each through `<dir>/nix` or `<dir>/nix-store` with `--extra-experimental-features nix-command` on every `nix`: `config show store`, pinned `store info`, `--query --roots <every generation>`, `--gc --max-freed <goal − avail>`, the pinned ones with `--store unix:///proc/self/fd/N` and env exactly `HOME` and `NIX_USER_CONF_FILES=`; the dead path removed, the rooted one and the generations preserved |
| `nix-dry-run-runs-no-root-query` | as above, `--dry-run` | plan eligible; commands are queries only (no `--query --roots`, no GC) |
| `nix-client-missing` | no `nix-store` on `PATH` | `missing`, no Nix command |
| `nix-store-absent` | no `/nix/store` | `missing` |
| `nix-client-outside-store` | client resolves to `/usr/bin/nix-store` | `unsupported_backend` |
| `nix-client-not-multicall` | `bin/nix-store` a regular file, or a symlink to anything but `nix` | `unsupported_backend` |
| `nix-single-user-layout` | `/nix/store` owned by uid 1000, no sticky bit | `unsupported_backend` |
| `nix-socket-ancestor-writable` | `/nix/var/nix` mode 0777 | `unsupported_backend` |
| `nix-socket-absent` | multi-user store, no socket | `backend_unavailable` |
| `nix-daemon-refuses-connection` | probe `ECONNREFUSED` | `backend_unavailable`, no Nix command |
| `nix-daemon-closes-before-magic` | probe `eof` (socket-activated service failed, or `allowed-users`) | `backend_unavailable`, no Nix command |
| `nix-daemon-probe-timeout` | probe `timeout` | `backend_unavailable` after 10 s of virtual time |
| `nix-daemon-wrong-magic` | probe `wrong_magic` | error, exit 3 |
| `nix-socket-path-env-refused` | `NIX_DAEMON_SOCKET_PATH=/tmp/other.sock` | `unsupported_backend`, no Nix command |
| `nix-state-dir-env-refused` | `NIX_STATE_DIR=/home/u/nix-state` (and, as variants, `NIX_STORE_DIR`, `NIX_STORE`) | `unsupported_backend`, no Nix command |
| `nix-remote-store-refused` | `config show store` answers `ssh-ng://builder` | `remote_backend`; no store info, no connection to it |
| `nix-local-store-refused` | answers `local://?state=/tmp/b` | `unsupported_backend` |
| `nix-chroot-store-refused` | answers `local://?root=/home/u/.local/share/nix/root` | `unsupported_backend` |
| `nix-other-socket-refused` | answers `unix:///tmp/other.sock` | `unsupported_backend` |
| `nix-auto-store-accepted` | answers `auto`, probe `serving` | bound, as the first vector |
| `nix-config-show-fails-is-error` | `config show store` exits 1 | error, exit 3 |
| `nix-lix-client-refused` | `nix-store (Lix, like Nix) 2.93.0` | `unsupported_backend` |
| `nix-client-version-not-allowlisted` | client 2.35.0 | `unsupported_backend` |
| `nix-daemon-version-not-allowlisted` | client 2.34.8, daemon 2.31.2 | `unsupported_backend` |
| `nix-other-filesystem` | `/nix/store` on another device than the path | `other_fs` |
| `nix-root-database-mismatch` | roots answer lists `/nix/var/nix-b/profiles/system-3-link`, not the generation diskgc read | `unsupported_backend`; `--query --roots` ran, no GC |
| `nix-no-generation-to-check` | no `*-<N>-link` under the profiles directories | `unsupported_backend`, no GC |
| `nix-older-generation-not-counted` | the roots answer lists the newest generation but omits an older one | `unsupported_backend`, no GC |
| `nix-too-many-generations` | 1,025 generation links | `unsupported_backend`, no `--query --roots` |
| `nix-socket-replaced-before-gc` | event: the socket's inode replaced after preflight | `changed`, no GC |
| `nix-daemon-upgraded-before-gc` | event: the pinned daemon version changes after preflight | `changed`, no GC |
| `nix-gc-fails` | GC exits 1 | action recorded, error, exit 3 |
| `nix-gc-sigterm` | event: SIGTERM while the GC child runs | child group SIGTERM then SIGKILL after 10 s on the virtual clock, lock fd inherited, action recorded, exit 143 |
| `nix-gc-deadline` | GC outlives the tier timeout | as above, exit 124 |
| `nix-overshoot-meets-goal` | GC removes far more than need | one GC, `met`, `avail_delta_bytes` above need |
| `nix-undershoot-next-tier` | GC frees less than need, a later `fs` tier | one GC only, then the next tier runs |
| `nix-allow-root-refused` | `--allow-root` with a `nix-store` tier | exit 2, nothing queried |

Each vector names the safety step it pins and turns red when that step is
removed (the TASK-121 discipline).

## 3. Property tests

`diskgc-modelgen` generates random host models (`proptest`) and checks:

- no Cargo lock file ever deleted or moved; nothing deleted that the walk
  did not check; no symlink followed; no mount crossed (on the call trace);
- every deleted entry's owner was authorised;
- **the index is never an authority:** with a randomly stale, partial or
  corrupted index, every deletion still passes the deletion-time checks,
  and nothing is deleted that a run with a correct index would refuse;
- the exit code follows the full precedence table for the final state;
- **selection semantics:** the eligible candidates equal a simple reference
  implementation of the rules in [reference.md](reference.md#selection-grammar);
  `!` and `protect` never add a candidate; `a || b` equals `b || a`; every
  eligible candidate was found by some source profile;
- **dry-run agreement**, narrowed to what is true: for non-overlapping
  candidates, `ordered` groups, `oldest` order and the same index (or
  `--no-index --tally-budget 0`), the real run's actions are a prefix of the
  eligible plan entries. With overlapping candidates or `best-fit`, every
  difference between plan and run is explained by a recorded reason (a
  child `missing` because its parent was deleted, a `best-fit` re-pick
  after observed space).

A failing case shrinks to a minimal model, printed as YAML; developers save
it under `crates/diskgc-model/tests/regressions/`, where it is replayed.
Regression files pin the host-model schema and are migrated when it
changes.

The 0.1 property suite runs five safety properties and eight integrated
filesystem selection properties with 32 cases each in `just check` and 1,024
cases each in `just soak`, plus saved regression replay. It covers independent
checks of
the filesystem deletion trace against modelled paths, owners, symlinks and
mounts; preservation of generated Cargo lock files during reclaim; exit-code
precedence; supported filesystem selection expressions; and ordered dry-run
agreement on stable, non-overlapping candidates. The integrated selection
generator varies YAML source path shapes, source `where`, known `has` and
`has_not` evidence, expressions and nested ModelHost trees. It compares the
dry-run eligible set with an independent finite-path oracle, checks narrowing
and union order, and verifies stable reclaim actions stay within that set.
Another generated family jointly varies source `has`/`has_not`, two known
marker files, whole-tree age, positive and complemented single or mixed
marker/age filters, two union terms, term-local `!profile:`, and config-bound
`protect`. Its oracle derives source admission, raw outer overlap, filter
truth and whole-tree age from generated facts without using the product
matcher or filter evaluator. It compares the eligible plan exactly and
checks that reclaim stays inside that set; when eligible candidates do not
overlap, reclaim must act on the whole set. A fixed nested case makes the
guard source's `where` reject an outer match and verifies that the raw outer
match still vetoes its child under `!profile:` and config-bound `protect`.
Additional generated cases check that unknown marker type admits neither
filter polarity; a raw `keep_newest` guard vetoes a shared union candidate
even when its source `where` fails; and `{uid}` roots select the right owner
while config-bound protection narrows the plan and reclaim. The first
generator injects an already-bound protection; the joint evidence generator
and owner case exercise the config binder. Saved regressions replay a
rejected outer `where` match and the whole Claude container exclusion with
user YAML profiles. A further generated family independently interprets
single-segment `*` and explicit-depth patterns over variable names in two
roots, with file, directory and any entry types, `find.exclude` patterns,
one or two positive patterns per source, source marker admission, term
filters, unions and raw `!profile:` overlap. A saved case checks that an
untagged outer match does not stop the second pattern from finding its tagged
child.
Its saved boundary case combines file selection, an excluded leaf and a
negated source directory in the other root. The finite families cover three
delete-source profiles, known marker names and two ages. They do not cover
every predicate, every unknown-evidence shape, all profile kinds and actions,
or concurrent races. The index-authority
property and indexed/best-fit dry-run cases follow the index implementation.
A failing generated case prints
its shrunk host model as YAML for a regression fixture.

**Profile golden sets.** Each built-in profile has a fixture tree and a
golden set of what it matches there. Any change to that set fails CI unless
the profile was renamed ([reference.md](reference.md#built-in-profile-library)).
This also covers the profile's uses under `!profile:` and `protect`,
since those use the same matching.

## 4. Differential contract tests

The differential fixture materializer builds the representable host shape on
a scratch tmpfs inside a test container (directories, `fallocate`d files,
owners via subuid ranges,
bind and stacked mounts, symlinks, helper processes holding locks or
files), and the same scenarios run on `ModelHost` and on `LinuxHost`;
the dry-run plan, reclaim exit, actions, skips, and deletion diff must match.
The independent deletion oracle judges every traced Linux run, including
dry-run. Host call traces are excluded from this comparison because the
`Host` trait is tentative; the oracle's syscall trace remains safety evidence.
Model fields that cannot ordinarily be built for real in a rootless container
(clock except the bounded sandbox offset, kernel capabilities, exact `statvfs`
numbers, filesystem types other than tmpfs, NFS, scripted Docker/Nix,
delayed freeing) are marked *model-only*; tests that use them say so.
The checked-in profile-vector inventory in
`diskgc-e2e/src/differential_suite.rs` classifies each original YAML by
filename and reason: 18 direct replays, four browser-home projections, two
process projections, two single-age clock projections, four mixed-age
originals requiring the QEMU guest, and kernel counterparts. Every direct or projected
entry names its registered container case. A kernel counterpart is a
vector whose exact interleaving (process events timed to diskgc's own
operations, such as forks after each process listing) cannot be staged on
a real kernel: it must fail preflight on its events and names the
registered container case that pins the kernel-side invariant
(`python-pycache-fork-*.yaml`: the fork-churn host tests, that the
candidate clears once the churn ends; that no unsettled scan clears a
candidate, which the sustained-churn case requires to occur and the burst
case checks only if one occurs). The checker's retry and its refusal to reuse an unsettled scan
for the next candidate are model-only: they rest on the vectors alone. Here “direct” means no
fixture-specific transformation; every outcome replay still uses the common
typed projection of nominal clock and mount geometry. The projections keep the
original expectations and check them before the transformed replay. West and
XDG use one historical timestamp cohort: the replay shifts the model clock
and explicit mtimes together, then uses the sandbox-only clock offset to make
the real tmpfs ctime of newly created objects the same age. Both real-kernel
replays pass the independent vector expectation and deletion oracle. The four
mixed-age originals require both old and fresh `idle_for` ctime cohorts.
Ordinary container replay preflight still refuses them before writing a
fixture or CLI config: changing mtime does not change kernel ctime. The
Nix-built QEMU replay creates the old inode cohort while the guest clock is
old, advances only the guest clock, then creates the fresh cohort. `statx`
checks both ctimes and mount ids before either diskgc run. The original YAMLs
and independent expected outcomes remain unchanged. Inside each guest Podman
container, ModelHost and LinuxHost run dry and reclaim, and the existing
strace deletion oracle judges the Linux runs. The containers-image replay
has its own `/var/tmp` tmpfs and an image-baked static process probe named
`/usr/bin/buildah` which holds the modelled spool file open; this tests the
in-use guard, not Buildah's own program logic. A literal `materialize(model)`
preflight checks the container's root/scratch mount ID, parent, filesystem
type, device, and statvfs size before writing a fixture. Inner mount IDs are
refused because the kernel assigns them only when mounted. An exact model
clock cannot be held constant while constructing a fixture, so literal
materialization always refuses that field after the mount checks. The refusal
precedes every fixture write. The materializer cannot infer outcome dependence
from a host model alone.
Differential vector replay therefore uses a checked `OutcomeStagePlan` to
declare every fixture stage before any fixture tree write. Its constructor
asserts the independent vector expectation. The staged tree nodes must
partition the checked tree exactly once, and the final stage must carry the
complete checked process and lock cohort. The plan issues `OutcomeProjection`
values for the physical filesystem/process shape. Replay checks the selected
profile's time predicate, then compares the model and Linux plan, action,
skip, exit, and deletion diff. The clock and mount geometry are intentionally
projected away in this outcome-only
path; literal replay is not claimed. Adding a checked-in vector requires a
new classification and parity evidence.

The other checked-in host-model YAMLs are classified separately:
`sketch/locked-cargo-target.yaml` uses a call-time event and exact external
space accounting, `docker-bundled.yaml` supplies scripted Docker
responses, and `hostlock/*.yaml` time a fork and an exit of the lock
holder (their kernel counterpart is `host_flock_release_only_by_close`).
Each fails materializer preflight with a named model-only field.
A disposable-container bite table mutates each supported refusal family
(kernel and identity, sandbox and command responses, events, process and
lock shapes, mount capabilities and types, and nonrepresentable nodes and
paths), invokes `materialize`, and compares recursive metadata and regular
file bytes under `/tmp`, `/var/tmp`, and `/home/tester` before and after each
refusal. A nested same-inode overwrite bite proves the comparison detects
changed contents in an existing directory. Additional live preflight bites
alter the clock, mount ID, parent, filesystem type, device, or size one at a
time from live kernel facts and require a contextual refusal without any
scratch change.

The generated differential cases run 100 disjoint Python bytecode models and
100 disjoint Mypy cache models per container run. The Mypy cases vary payload
sizes, a second tagged cache, a matching path without `CACHEDIR.TAG`, and a
tagged cache containing a checkout; an independent expectation rejects a
missing tag on an expected eligible cache. Both generators run the same
ModelHost/LinuxHost outcome comparison and deletion oracle as checked-in
vectors. A uniform sandbox clock offset (task-89) makes newly created
inodes old enough for the West and XDG fixtures after the model clock and
explicit mtimes are rebased. It cannot simultaneously reproduce the old eligible and fresh
`not_idle` inodes in the Autotools, CMake, Meson, and containers-image
fixtures: all get the same fresh kernel ctime after materialization. Those
four originals use staged kernel creation times in a disposable QEMU guest
whose wall clock changes between creating the old and fresh cohorts. A host container cannot safely set the wall clock:
`CLOCK_REALTIME` is not time namespaced, and the sandbox offset changes only
diskgc's read of time, not the kernel's ctime. The reproducible
`vm-historical-ctime` flake check boots a disposable guest with a private
ext4 disk and rootful Podman using native overlay. The Nix store and test
binaries are mounted read-only in the guest containers; no writable host bind
is present. A missing image marker makes the canary exit 86 before the four
replays. The guest kernel command-line token is checked before any clock
change, the guest clock is restored after each replay, and the VM driver
reads its host clock while the guest is held at the old wall time, comparing
it against a monotonic baseline before restoring the guest clock. Destructive diskgc runs remain
inside the guest Podman containers.

## 5. End-to-end tests in containers

The real binary in **rootless Podman containers**, built from images Nix
produces. They run everywhere, need no KVM, and isolate the test's
filesystem, processes and mounts from the developer's system.

- **Cargo race tests**, each with real Cargo processes, against every
  supported Cargo version (at least the oldest supported stable and one
  dated nightly; the pinned set is in Cargo toolchains under test), on
  fixture projects with no registry dependencies, offline. Races are made
  deterministic (a helper holding `.cargo-lock` makes Cargo take
  `.cargo-build-lock` and block; a `build.rs` waits on a control file; a
  `runner` wrapper pauses between compile and run):
  1. a build holding the lock: its target is skipped;
  2. a build that starts during deletion waits, then succeeds;
  3. one build blocked on the lock plus another started right after the
     reclaim never run concurrently, and both succeed;
  4. a target whose test binary is still running is skipped;
  5. a new profile (`--release`, `--target`) started between discovery and
     locking: skipped as `changed`, its build succeeds;
  6. diskgc SIGKILLed during deletion at each point of a seeded list of
     positions in its call trace: the next `cargo build` always succeeds;
  7. a `cargo check` paused after taking `.cargo-build-lock` and before
     `.cargo-lock`, plus a concurrent `cargo build`: the target is skipped,
     no lock file is ever unlinked, both commands succeed;
  8. `cargo run` / `cargo test` on an old, up-to-date target, paused
     between compilation and execution: skipped as `in_use`, the binary
     runs;
  9. a profile directory with an unknown `.cargo-*lock*` file: skipped as
     `unsupported_layout`.
- **Mounts** a rootless container can create: tmpfs, bind mounts (including
  a bind mount of the same filesystem), stacked mounts; the path-traversal
  rules.
- **Filesystem profiles** on the scratch tmpfs mounted at the container's
  `/tmp`: entries held open, used as working directory, mapped, named in a
  process's initial environment, `flock`ed, containing a socket or FIFO,
  containing a mount, a mixed-owner tree (subuid ranges), a file created
  mid-delete, a deleted-but-open file in `held_by_deleted_files`, and a root
  with 100,000+ entries (ordering cost, aggregated reports, time to first
  deletion including the eligibility walk). The `fs` reclaim
  (`fs_reclaim_suite.rs`) runs the binary over user profiles of a config
  of the test's own: a `/tmp` laid out as on the development machine
  (pytest test dirs, also in nix-shell TMPDIRs, rc dirs, decoys of every
  refusal, a `systemd-private-*` directory a pattern matches) of about
  12,000 entries in the fast tier and 100,000 in the full tier, judged by
  the oracle; stale sockets and FIFOs; and a FUSE mount whose server
  never answers (below).
  The full-tier `fs_40k_small_candidates_plan_and_reclaim_scale` builds
  40,000 distinct small directories on the disposable container's `/tmp`
  tmpfs. Its first diskgc process dry-plans all of them; its second process
  deletes them under strace. The independent deletion oracle checks the
  entire container filesystem. The case logs both durations, deletion rate,
  and the diskgc processes' `VmHWM`. A path-heavy fixture process exercises
  the environment ancestor index; a separate read-only host probe reports a
  lower bound for the index's retained heap allocation and the scan
  process's RSS while the scan is alive. The first plan has no warmed diskgc
  process state, but fixture construction has warmed kernel dentries, so it
  does not measure a cold Linux page cache. `VmHWM` includes all diskgc
  allocations, not only the ancestor index.
  The separate `vm-cold-scale` flake check (`just e2e-vm-cold-scale`, also in
  `just e2e-full`) builds 40,000 one-file directories on a disposable NixOS
  guest's private ext4 root disk. It starts a genuine Podman container with
  that tree bind mounted read-only, verifies ext4 from both the guest and the
  container, and checks the real sandbox canary. A guest kernel-command-line
  token guards the cache drop; after container setup, the script syncs and
  writes `3` to the **guest's** `/proc/sys/vm/drop_caches`. It requires a
  measured drop of at least 40,000 dentries and 32 MiB of guest page cache
  before the first diskgc invocation. The first real binary dry-plans all
  candidates with the default 10-minute product deadline; a second dry plan
  is the warmed control. Both assert exit 0, `dry_run`, 40,000 unique eligible
  candidates, no errors, and unchanged fixture count. GNU time runs inside
  the container around each diskgc process and records elapsed time and peak
  RSS, excluding Podman startup; the guest records the cache counters. This
  measures a cold **guest** VFS/page cache on ext4; the QEMU host may still
  cache the guest disk image, and it does not measure host deletion throughput.
  On 2026-10-23 the final focused VM check passed: the guest dentry count
  fell from 103,231 to 3,863 and Cached from 502,724 to 43,544 KiB. The cold
  dry plan took 12.93 s with 53,636 KiB peak product RSS; the warm control
  took 10.57 s with 53,916 KiB. Both judged 40,000 eligible candidates
  without changing the fixture. One warm control is too little evidence to
  claim a general warm-cache speedup.
- **Fork churn during the in-use scan:**
  `host_inuse_scan_settles_after_fork_churn` runs the real Linux scanner in
  a disposable container with 0, 1, 4 and 8 bounded child-spawning workers.
  It timestamps each child's spawn and wait with `CLOCK_BOOTTIME` and
  requires one entire spawn-and-wait interval inside a measured scan's
  recorded interval. An unsettled scan or checker result must block
  clearing. It reports process-list rounds, duration, whether the 16-listing bound
  was hit, and whether the checker retried. After reaping every worker,
  it requires the unused candidate to clear. This exercises the
  current host kernel. Six Nix QEMU checks (`vm-pidfd-linux-*`,
  `vm-old-kernel-pidfd`) run the same case in disposable Podman containers
  on pinned Linux 5.8.16 (the PRD floor), 5.15.221, 6.1.189, 6.6.158,
  6.8.12 and 6.12.112 guests. The 5.8 and 5.15 guests have four vCPUs; the
  others have two. The guest asserts its online vCPU count before testing,
  so the scanner and fork workers can overlap. The 5.8, 5.15 and 6.8 VMs
  each use a separate exact Nixpkgs pin for the whole test driver and NixOS
  guest (20.09 `cd63096d6d887d689543a0b97743d28995bc9bc3`,
  `6f3b13fcf904691a274955cc80c9f2ed655ff6a9`, 24.05
  `63dacb46bf939521bdc93981b4cbb7ecb58427a0`); their image and static
  binaries still come from the primary project pin. The 20.09 guest needs
  three adjustments: Python 3.9 for the probe (3.8 has no pidfd API), the
  driver's `black` lint of the generated script switched off, and
  `--security-opt seccomp=unconfined` for its containers, because Podman
  2.1.1's built-in seccomp profile answers `pidfd_open` with `EPERM` and
  diskgc then refuses to start (docs/safety.md, In use). A direct guest
  probe holds a pidfd across child release and reap: the live child has no
  poll event and accepts signal 0; the released pidfd polls readable
  without HUP on 5.8, 5.15, 6.1, 6.6 and 6.8, with HUP on 6.12, while
  `pidfd_send_signal(pidfd, 0)` returns ESRCH on all six. In each guest,
  `host_pidfd_release_after_reap` pins a live child with
  `LinuxHost::proc_identity`, checks that it is not released, reaps it while
  retaining the pidfd, then requires `LinuxHost::proc_identity_released` to
  return true. The 5.8, 5.15, 6.1, 6.6 and 6.8 cases directly bite the no-HUP
  fallback; 6.12 is a HUP control. On 5.8 and 6.8 no measured scan reached
  the 16-listing bound, so those points check release detection and
  settling, not the bound. Podman's graph root is on each guest's private ext4
  disk; its only bind mounts are read-only Nix store and test binaries.
  `host_inuse_scan_under_sustained_fork_churn` is a full-tier, 30-second
  disposable-container measurement with eight continuous fork workers. It
  records completed children, checker samples, unsettled and blocked results,
  explicit growth-limit reasons, the highest raw PID listing count and scan
  latency, and requires the unused fixture to clear after every worker exits.
  The raw listing count includes re-reads and is diagnostic; reaching the
  growth-listing limit is checked against the scanner's reason and
  `MAX_LISTINGS`. The sustained worker probe is untraced
  to avoid filling the E2E record store with child process logs; it checks
  that its candidate fixture was not changed. A focused run on the current
  7.2.9 host kernel completed 108,221 children and 263 checker samples:
  261 scans were unsettled, all 261 blocked and reported the 16-growth-listing
  limit, the highest raw listing count was 16, and the candidate cleared
  after the storm. This is a current-kernel
  sample under synthetic load, not evidence about an actual loaded CI host.
  The VM checks cover six kernel points, not every kernel in the supported
  range; distribution kernels with backports are not covered. A loaded CI
  host has not been measured.
- **Root-mode owner resolution:** the static binary runs under container
  root with a protected config that names a numeric UID absent from
  `/etc/passwd`; an oracle-checked reclaim proves the UID owns every removed
  entry. A separate negative fixture rejects an unknown name with the musl
  NSS limit in its exit-2 message, and rejects a name plus explicit UID that
  resolve to the same owner before any deletion.
- **Buildah's live containers/image spool:** a rootless Buildah commit from a
  small offline payload is stopped while it holds a tester-owned
  `/var/tmp/container_images_storage*` file open. The cleaner must report
  that directory as `in_use`, and the deletion oracle must remain clean;
  Buildah then resumes and completes. This Nix-only test bind-mounts the
  pinned Buildah and UID mapping tool closure read-only from the host's Nix
  store into its disposable container. Its fixture uses a one-second idle
  floor to exercise the in-use check without waiting for the shipped
  profile's three-day floor.
- **Dormant temp users:** a live process sets `TMPDIR` after exec, holds no
  descriptor in the directory and then sleeps. A ModelHost case and a
  disposable-container dry-run prove that a `tmp.*` delete profile would
  mark it eligible. The container case also proves the same false negative
  for a `nix-shell.*` directory under `/tmp/claude-{uid}`. A separate
  disposable-container run reclaims an exact `node-compile-cache` entry
  inside that prefix and preserves the enclosing shell and neighboring
  file, checked by the deletion oracle. The container test requires
  `mktemp-dir`, `nix-shell-tmpdir` and `tmp-entry` to remain absent from
  the built-in library (`nix-shell-rc` ships with its Owner PID contract,
  TASK-112 step 4). The user-configured probe is only run
  with `--dry-run`. Protecting real dormant `nix-shell` and `nix develop`
  sessions remains a release blocker for those planned profiles.
- **Live pytest without a session lock:** pinned pytest 9.1.1 in the
  disposable Nix image runs a test using `tmp_path` with
  `tmp_path_retention_policy=none`. While the test sleeps, its normal
  `pytest-of-tester/pytest-0/test_active0` directory has no numbered-session
  `.lock` and no open descriptor beneath it. A user-configured path-matching
  probe marks the directory eligible in `--dry-run`; the deletion oracle
  verifies that the run touched nothing. The test requires `pytest-tmp` to
  stay absent from the built-in library. A checked-in ModelHost vector
  independently proves the same no-descriptor classification with an old
  candidate timestamp, so adding an idle filter alone cannot certify safety.
- **Native Docker Engine smoke** in its own digest-pinned dind image under
  rootless Podman (`just e2e-docker-one native_docker_engine_dind_smoke`).
  The harness starts `dockerd` on a private Unix socket, imports an image
  from an offline scratch payload, creates a container and volume, and
  verifies their identities survive a traced diskgc run and the deletion
  oracle. Backend binding and prune cases remain with task-35.
- **Nix GC** (TASK-36, its own image and `just` recipe): a pinned
  multi-user Nix whose `nix-daemon` runs as container root on the
  container's own `/nix` (tmpfs or overlay, never the host store; no host
  socket mounted), diskgc as uid 1000, untrusted. A pre-scenario check
  requires the store to contain only the image's paths. Scenarios:
  - `nix_gc_keeps_every_kind_of_root`: a direct root, an indirect out-link
    root (`nix-build -o`), two generations of `/nix/var/nix/profiles/system`
    and of a per-user profile, a temporary root held by a paused build,
    and a runtime root (a process with a store file open) all survive;
    dead paths are removed; the oracle lists the store before and after.
    The report gives the root check's duration next to the GC's.
  - `nix_gc_refuses_a_daemon_with_another_root_database`: the daemon
    restarted with `--store 'local?state=/nix/var/nix-b'` over the same
    `/nix/store`, B registering a path rooted only in A; the reclaim
    skips `unsupported_backend` at the root check, and the path survives.
  - `nix_gc_refuses_unlisted_versions`: a second pinned CppNix as client,
    and a pinned Lix client, each `unsupported_backend`, with no GC run.
  - `nix_gc_refuses_single_user_layout`: no daemon, store owned by uid
    1000: `unsupported_backend`.
  - `nix_gc_refuses_redirected_store`: `NIX_REMOTE=local?state=...`, a
    user `nix.conf` `store` setting, and `NIX_REMOTE=ssh-ng://x`: skips
    as specified, nothing deleted in either state.
  - `nix_gc_daemon_down_is_a_skip`: the daemon stopped with its socket
    kept, and a socket-activated daemon (a listening socket handed to a
    daemon that exits at once): both `backend_unavailable`, exit 1 with no
    other tier; a user excluded by `allowed-users`: the same.
  - `nix_gc_trusted_and_untrusted_user`: the keep-every-root scenario once
    with diskgc's user in `trusted-users`, once without; the same paths
    survive. Every admitted daemon version answers the probe's magic.
  - `nix_gc_sigterm_mid_gc`: 10,000 small dead paths, SIGTERM to diskgc
    after the first `deleting` line on stderr: exit 143, the host lock
    held until the client exits, every rooted path present, and a second,
    uninterrupted GC completes; also SIGKILL to diskgc (TASK-36 AC3).
  - `nix_gc_overshoot_is_bounded_by_the_last_path`: one 64 MiB dead path,
    need 1 MiB: at least 64 MiB freed, one GC, `met`.
  - `nix_gc_optimised_store_frees_at_the_end`: `auto-optimise-store =
    true`, dead paths sharing files through `.links`: the space appears
    only after the links phase; measured and reported, not asserted
    against a timing.
  - `nix_gc_pinned_socket_survives_replacement`: container root replaces
    the socket pathname with a decoy listener after preflight: `changed`;
    replaced after the recheck instead: the GC still reaches the original
    daemon through the pinned fd, and the decoy receives no connection.
- **Signals, deadlines, the CI-hook use of `ensure`, `watch` as a plain
  process.**
- **Journeys** (`journey_suite.rs`): the product driven through the files
  its users write, judged like every container test. The CI operator's:
  the example pre-job hook [ci-pre-job.sh](ci-pre-job.sh), run as written
  by `sh` over five real Cargo projects (three idle, one with a build
  paused in its build script, one whose test binary still runs) and a
  deleted file a stale process holds; the goals come from the harness's
  `statvfs` and its own count of each target's bytes, and every exit 0
  or 1 is checked against that `statvfs` and `--min-free`
  (`journey_ci_operator_frees_space_with_ensure`, once per toolchain in
  its tier). It also runs the hook's default selection
  (`filter:idle-1d`) for its main cases. The sandbox-gated clock-offset
  file first advances the binary's wall clock one second to prove fresh
  targets get `not_idle`, then two days to prove real-binary selection of
  old targets through the same filter. The oracle and statvfs checks judge
  the resulting deletion and space claim. The offset is uniform within a
  run; it does not solve the mixed historical/fresh ctime vector gap below.
  The final human-mode run asserts that the output names the two held targets
  with their distinct `in_use` and `locked` reasons, omits the three targets
  emptied by earlier runs, and includes elapsed time and the held deleted log's
  process and its size. Report unit tests cover bounded per-reason examples,
  preflight refusals separately from kept candidates, control-byte escaping,
  held bytes and top PID in normal and dry-run output, and exact byte counts
  when rounded sizes would misleadingly compare equal (including in a dry run). A ModelHost
  integration test makes the progress line compare near-equal available and
  minimum values without hiding their one-megabyte difference.
- **Agent pre-job journey** (`agent_journey.rs`, Nix image): the shipped
  [agent-pre-job.py](agent-pre-job.py) runs real `ensure --json` and decides
  from its exit code and parsed JSON alone. Separate traced runs exercise
  `met` (0), `exhausted` (1), a mistyped selection (2, `abort_config`), a
  runtime misconfigured tier reported as `error` (3), and `lock_timeout`
  (75) while another real reclaim holds the host lock. Every documented
  exit code, including 2 with `cause: provisioning`, 124, 130 and 143, is
  decided against a stand-in `diskgc` by `tools/test_agent_pre_job.py`
  (part of `just check`). The deletion oracle judges every run; the competing command
  keeps its candidate, and the test checks the agent's one-line JSON
  decision in each case.
- **Not only Nix:** every test but the Nix-only and Docker-only ones in a container from
  a Debian image with the static binary, without Nix (Container setup:
  Images; `just e2e-debian`). A separate dind image exercises Docker Engine;
  the Nix and Debian images do not carry a Docker daemon.
- **Shipped files:** `shipped_tmpfiles_and_example_work_in_container`
  applies the packaged drop-in with `systemd-tmpfiles --root=<scratch>
  --create` in disposable Nix and Debian containers, checks the resulting
  lock file, and validates the packaged example config.
  `release_candidate_files_work_in_container` repeats that proof with the
  separately packaged 0.1 candidate binary and its installed files. The Docker dind
  image runs the same proof as
  `docker_shipped_tmpfiles_and_example_work_in_container` with Alpine
  `sd-tmpfiles`. That proof names the staged rule file explicitly: Alpine
  `sd-tools` 0.99.0 crashes when asked to scan a scratch root's tmpfiles.d
  directories without a file argument, so this test does not establish
  automatic rule discovery there. Every container test provisions its
  per-test host lock from the packaged rule. The separately built
  `release-01-candidate` package installs both files and passes the Nix
  package comparison check and the Nix/Debian container application checks.
  This packaging proof does not approve the candidate for distribution;
  the 0.1 release gate and its profile scope remain open in TASK67.

**Not covered end-to-end (for now):** deletion on real block filesystems
(ext4 or xfs as separate mounts, btrfs), network filesystems, and live
systemd. A private ext4 VM disk covers the 40k dry-plan cost only. Rootless
container behavior follows the test host's kernel; the pinned VM cases above
cover the listed kernels. Other cases rest on model tests with model-only
fields.

Why not FUSE (to model hosts): it has its own `flock`, `statx` and mount-id
semantics, needs privileges, and is a large extra moving part. One
container test mounts a FUSE filesystem as a fixture, not a model: a dead
mount (the harness opens `/dev/fuse`, mounts it and never answers), to
show that mounts come first (safety.md, Before the walk). Another fixture
answers enough FUSE requests to let a process hold a directory fd and cwd,
then stops answering; it proves an ordinary stat blocks before checking
that `ensure` exits within its deadline and keeps a held candidate. Both need
`/dev/fuse` in the container: the runner passes it (`--device`) to the
tests listed in `FUSE_TESTS` (`suite.rs`) only, when the host has it, and
the test fails saying so when it is missing; it first proves the mount
dead (a `statx` of it gets no answer for 2 s).

Container tests cannot run inside the Nix build sandbox, so they run from
`just e2e`, not as `nix flake check` checks.

## Cargo toolchains under test

The race tests run against these toolchains, pinned from the locked
rust-overlay input (flake.lock) and separate from the product toolchain
(`rust-toolchain.toml`); `cargoUnderTest` in flake.nix is the single list:

| Toolchain | Tier | Why |
|---|---|---|
| `1.90.0` | full | oldest supported stable, the version the protocol was first measured on (only `.cargo-lock`) |
| `1.99.1` | fast | newest stable in the lock, the product toolchain; holds every lock file the allowlist knows, so the lock assertions are not vacuous on every commit |
| `nightly-2026-10-16` | full | newest dated nightly in the lock: lock and layout changes show before they reach stable |

What each one locks, and the layouts they produce, is measured in
[safety.md](safety.md#lock-files). The supported range is not fixed by
version: these three are the oldest stable the docs name, the newest
stable and a nightly. A Cargo older than 1.90 with a 1.90-like lock set
(Debian 13 ships 1.85, Ubuntu 24.04 ships 1.75) is acted on but untested;
refusing unknown lock names only protects against newer versions. A new
toolchain is a new entry in the list (and a new base image); its tests
appear by themselves. Adding one also means repeating the strace
measurement of what a Cargo writes before `.cargo-lock`
([safety.md](safety.md#lock-files)) with it, until race test 2 does that
itself (task-13 H13).

- **Layout.** Minimal profiles (rustc, cargo, the host's rust-std; no docs,
  no rust-src) and one C toolchain (the linker, nixpkgs' `stdenv.cc`) are
  base-image inputs: `/opt/cargo-under-test/NAME` and
  `/opt/cargo-under-test/cc` link into the image's `/nix/store`, and the
  manifest `/opt/cargo-under-test/toolchains` lists `NAME VERSION TIER` per
  toolchain. The harness bind-mounts `/nix/store` read-only over itself in
  every container of the Nix image (its descriptor's `read-only`;
  Container setup: Images), so the deletion oracle's snapshot does not walk
  it ("mounts not walked: ... /nix/store (read-only)"). The Debian image
  has one toolchain of its own (Images). Nothing is baked under
  `/home/tester` (a fresh tmpfs per test): `CARGO_HOME` defaults to
  `/home/tester/.cargo` and starts empty. The three toolchains make the
  base image about 2.8 GB (from 80 MB); loading it after a change takes
  about 30 s, once.
- **Per-toolchain tests.** Each Cargo test runs once per toolchain in the
  manifest, listed as `NAME@TOOLCHAIN` (`just e2e-one
  cargo_smoke@1.99.1`), in the tier the list gives its toolchain (the
  manifest's third column). A missing or empty manifest, or one without a
  fast toolchain, fails the listing, so the Cargo tests never silently
  vanish.
- **Smoke first.** Before every Cargo test's body the suite runs the smoke
  check of its toolchain: `cargo --version` and `rustc --version` print the
  manifest's release, and a no-dependency fixture builds offline and its
  binary runs (as `tester`, untraced, on a tmpfs of its own that is
  unmounted afterwards). If it fails, the test is reported as `TOOLCHAIN`
  (exit 3, counted apart in the runner's summary), not as `FAIL`: a broken
  toolchain is never mistaken for a race failure. The run still fails.
- **Offline.** Fixture projects (`diskgc-e2e/fixtures/cargo/`) have no
  dependencies and their own `[workspace]`; they are compiled into the
  harness and written to a scratch tmpfs per test. Cargo runs with
  `CARGO_NET_OFFLINE=true` in a container with `--network=none`.
- **Helpers** (fixtures, not code under test; `diskgc-e2e probe ...`):
  the lock holder `flock-hold FILE...` (exclusive, or `--shared`; fails
  when a lock stays busy for the release bound instead of blocking); the race fixture's `build.rs`, which waits at the gate
  named by `DISKGC_E2E_BUILD_GATE`; the runner wrapper `cargo-runner`, set
  as the host target's runner, which waits at the gate named by
  `DISKGC_E2E_RUNNER_GATE` and then execs the binary; the race fixture's
  `#[test]`, which waits at the gate named by `DISKGC_E2E_TEST_GATE` (a
  test binary still running). A gate is a pair of
  control files on the scratch mount: the helper writes its pid to
  `GATE.waiting` and waits for `GATE.go` (failing after 300 s). `cargo run`
  execs its runner in its own process (same pid, the wrapper's
  executable); `cargo test` starts it as a child.
- **Checked by `/proc/locks`**, never by what Cargo prints: while the
  holder holds `.cargo-lock`, a build waits for it (holding
  `.cargo-build-lock` where the version has one); while the build script
  waits, Cargo holds every lock file of the profile directory and waits for
  none; while the runner waits, no process holds any of them. The sampler
  takes `/proc/locks` in a single `read(2)`: seq_file restarts each read at
  an index into the host-wide lock list, which other containers change all
  the time, so a text read in pieces can repeat or skip lines (seen under
  a parallel run).
- **A traced Cargo build** (code-under-test strace, `--seccomp-bpf`) of the
  smoke fixture must cost at most 60 % of the ptrace stops (voluntary
  context switches of the whole process tree) of the same build under the
  same strace without `--seccomp-bpf`, measured in the same container, and
  at most 120 s (a hang guard). Measured: 36 % on 1.90.0, 44 % on 1.99.1
  and the nightly; the untraced build switches far less than either.
  Limit: `--seccomp-bpf` saves less on a build than on the single-process
  churn above, because strace 7.2 restarts every new process and thread
  with `PTRACE_SYSCALL` until its first traced syscall (it cannot know
  whether the new task carries the filter), and most of a build's
  processes and threads never unlink; so the bound is relative to the full
  trace rather than per traced call.

## Cargo race tests

The race tests of section 5 are container tests in `diskgc-e2e`
(`cargo_race_suite.rs`, `cargo_reclaim_suite.rs`, with shared helpers in
`cargo_harness.rs`), each run once per toolchain under test
(`NAME@TOOLCHAIN`, Cargo toolchains under test) unless marked. diskgc runs
as the test driver's `cargo-reclaim` (the reclaim protocol of
[safety.md](safety.md#reclaim-protocol) over `LinuxHost`), traced, paused
or killed at `OP:N` positions (Controlled pause and kill points); the
Cargo commands are untraced fixtures, declared to the oracle as concurrent
writers only where they write during a judged run (`target/.rustc_info.json`,
which Cargo rewrites in place when it starts; the whole target where a
`cargo clean` removes it). Each scenario has one timed event: the Cargo
command started while diskgc or another Cargo is stopped at a known
point. Waits have wall-clock hang guards (120 s to reach a state); a state
the test asserts is checked over a count of `/proc/locks` samples (one
`read` each), not over a time.

Every test judges the driver's runs with the deletion oracle (snapshot and
strace record) and checks the Cargo commands' outcome. What else each one
rests on, independent of diskgc, and what it takes from the driver's own
report (its verdict line: reason, `entries=`, `held_us=`, the detail):

| # | Test | What it checks (Reclaim protocol step) | Independent of diskgc | From the report |
|---|---|---|---|---|
| 1 | `cargo_reclaim_skips_a_target_whose_lock_is_held` | a build paused in its build script, the holder exclusive and shared: `locked` (step 1) | `/proc/locks`: the build holds `.cargo-lock`; oracle with nothing eligible | the reason `locked` |
| 2 | `cargo_race_2_a_build_started_during_deletion_waits_writing_nothing` | the driver paused at the first unlink of the second-to-last part; `cargo build`, `check`, `test --no-run` and `doc` (a round each) started then wait for a lock diskgc holds, holding only lock files they created; the profile directory is unchanged but for them; diskgc finishes its part and stops before the last one, `changed` if a lock file appeared (the re-listing), else `in_use` (the part's check); the command recompiles, a built binary runs. On this toolchain's own target, and (H2, the `.cargo-lock` invariant of step 3) on a target the oldest stable made, where 1.99.1 and the nightly create and hold `.cargo-build-lock` and wait for `.cargo-lock`. On 1.90.0 only the own-target rounds run: no newer Cargo, and no `.cargo-build-lock` is created there | `/proc/locks` of diskgc and the command, steadily; the harness's walk (inode, size, mtime, ctime) of the profile; strace: the unlinks from the pause on lie in the part in progress | the reason and its detail |
| 2 | `cargo_race_2_a_cargo_clean_during_deletion` | `cargo clean` and `cargo clean --profile dev` take no lock and remove what diskgc is deleting; diskgc's next unlink finds nothing and it stops (`changed`, `ENOENT`), unlinking nothing more; `cargo clean -p` waits for a lock diskgc holds, writing nothing, and diskgc stops `in_use` at its next part; the next build succeeds | `/proc/locks` (never waiting, or waiting steadily); strace: no unlink after the pause, or none past the part; the walk | the reason, `entries=`, `ENOENT` |
| 2 | `cargo_race_2_split_build_dir_on_a_target_first_built_whole` | a target first built whole (all three lock files) is reclaimed, then split-mode build, run and test succeed; and race 2 with a split-mode build started during the deletion: it holds the build directory's `.cargo-build-lock` (which diskgc does not hold), creates no lock file in the target's profile, waits for a lock diskgc holds there, writes nothing into it; diskgc stops `in_use`. 1.90.0 ignores the setting (logged not applicable) | as race 2, plus `/proc/locks` on the build directory's lock | as race 2 |
| 3 | `cargo_race_3_a_blocked_build_and_a_later_one_never_run_together` | a build blocked during the deletion and one started after diskgc exits: the second waits for the very lock files (by inode) the first holds in its build script, over the samples; both succeed (step 1; lock files never move) | `/proc/locks` by file id; strace: the unlinks from the pause on lie in the part in progress | the reason `in_use` naming the blocked build |
| 3 | `cargo_race_locks_held_through_the_last_part_released_per_target` | two targets in one run: at the first and at the last unlink of the first target's last part diskgc holds every lock of it and a build waits; at the second target's first lock it holds none, and the build goes on (steps 3 and 4) | `/proc/locks` at three pauses | the reason `reclaimed` of both |
| 4 | `cargo_race_4_a_target_whose_test_binary_still_runs_is_skipped` | a running test binary (no lock held on any lock file): `in_use`, nothing deleted (step 2 or the per-part check: indistinguishable from outside) | `/proc/locks`: no lock held; the test binary's executable inside the target; oracle with nothing eligible, no unlink | the reason and the evidence naming the test binary |
| 5 | `cargo_reclaim_changed_between_discovery_and_locking` | paused at `flock_try:1`; `--release`, `--target`, or a newer build meanwhile: `changed`, no unlink (step 2) | `/proc/locks`: diskgc holds no lock at the pause; strace: no unlink; lock files keep their inodes | the reason `changed` |
| 6 | `cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds`; `cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions` (full tier, one run: a debug profile 1.99.1 and then the nightly built) | killed at every seeded position: the tree shows step 3's order (every part before the kill gone, every part after it whole, the part itself short by exactly the unlinks made); lock files keep their inodes and hold no lock; every profile keeps a known layout; the recovery builder rebuilds every profile and its binary runs, compiling again once the fingerprints are gone | the harness's plan and walk, strace counts, `/proc/locks` | nothing (the driver dies) |
| 7 | `cargo_race_7_a_check_between_its_locks_and_a_build_keep_the_target` | `cargo check` holding `.cargo-build-lock` and waiting for `.cargo-lock`, a build waiting behind it: `locked` at the first lock with none taken; then a holder on `.cargo-build-lock` alone and a second target: at `flock_try:2` diskgc holds no lock of the first target (step 1, Cargo's order) | `/proc/locks` of both commands, steadily; diskgc paused at the run's second lock attempt (a later target's) holding nothing of the first | `locked` at `.cargo-build-lock`, `held_us=none` |
| 8 | `cargo_race_8_run_or_test_paused_before_its_binary_keeps_the_target` | `cargo run` and `cargo test` paused in the runner (no lock held): `in_use`, nothing deleted, the binary runs; also a `cargo run` started while the driver, having judged the target eligible, is paused before its first lock (step 2: the scan made under the locks): `in_use`, or `changed` in a round where the harness's walk shows `cargo run` wrote into the profile (1.90.0 rewrites `<bin>.d` now and then), retried up to 5 rounds | `/proc/locks`: no lock held; oracle with nothing eligible, no unlink; the walk decides which verdict is right; the pause's length is logged | the reason and the evidence naming the runner |
| 9 | `cargo_race_9_an_unknown_lock_file_is_unsupported_layout` | `.cargo-future-lock` beside the lock files: `unsupported_layout` with no lock taken, also while `.cargo-lock` is held (which would make it `locked`) (step 1, layout before locks) | diskgc paused at the run's second lock attempt (a later target's) holding nothing of the target; the holder still holds `.cargo-lock` | `unsupported_layout`, `held_us=none` |
| AC | `cargo_race_split_build_dir_is_never_reclaimed` | a separate build directory: both halves `unsupported_layout`, no lock taken, nothing deleted, idle, during a build holding both halves' locks and during `cargo test` paused before its binary (1.90.0 ignores the setting: logged not applicable) | as race 9, for both halves; `/proc/locks`: the build holds both halves' locks | `unsupported_layout`, `held_us=none` |
| AC | `cargo_race_shared_target_dir_from_cargo_config` | a `target-dir` from `.cargo/config.toml` outside the projects, used by two projects: races 1, 8, 4 and 2 on it; then a `target-dir` pointing at another project's own target: `cargo run` paused in its runner keeps it `in_use` by the runner's environment | as the races it repeats | as the races it repeats |

**Race test 6's positions** are computed from the harness's own walk, not
from absolute call counts: the parts of step 3 in their order (every
profile's fingerprints, `.fingerprint/` and each
`build/<package>/<hash>/fingerprint`, then each profile's other entries
in name order, `build/` and `deps/` last), each object one `unlinkat`. The
seeds are the first unlink of every part (every boundary, the very first
included), the middle of every part of three objects or more (inside
`.fingerprint/`, `incremental/`, `build/`, `deps/`) and the last unlink;
they are named by part, so the positions are computed again after each
rebuild, and every kill logs the part sizes. A debug and a release profile
give about 20 positions per toolchain; the test requires a kill in the
middle of `incremental/` and of the first fingerprint part among them. On
the mixed layout every position is killed once per recovery builder
(1.99.1, the nightly): before each kill the target is built by both, so it
is mixed again, and after it only that builder rebuilds, so each recovers
alone from the same interrupted state; the seeds must include kills inside
`.fingerprint/`, inside a per-unit fingerprint and inside `incremental/`.

**Limits.**

- The walk that shows race test 2's Cargo wrote nothing compares objects
  (inode, size, mtime, ctime) before and while it waits: a file created
  and removed in between is invisible where a lock file was created too
  (the directory's times move anyway). The strace measurement in
  [safety.md](safety.md#lock-files) (what a Cargo writes before
  `.cargo-lock`) covers that once per toolchain, for `build`; the test
  does not repeat it (task-13 H13).
- Race test 8's late case is conditional evidence for the scan made under
  the locks: a scan kept from before them is caught only while the
  driver's pause is shorter than the in-use scan's reuse limit (500 ms),
  and only in a round where `cargo run` wrote nothing into the profile
  (the test logs both, and warns when no round of 5 reached the check; it
  does not fail on that). The model test of that step holds it
  independently.
- The "no lock taken" checks of races 7, 9 and the split build directory
  see a lock still held at the run's second lock attempt; a lock taken
  and released before it is not seen (the strace record traces no
  `flock`). In race 7's first state both locks a Cargo-ordered or
  reversed run would try first are held by others, so no single change to
  the protocol makes that check fail; it guards against a lock of that
  target held across targets.
- No gate stops Cargo between compiling and starting its runner, so the
  window in which a Cargo of another project, whose `target-dir` points
  at an attached target, is the only evidence (the documented blind spot,
  [safety.md](safety.md#cargo-target), In use) is not reached by any test.
- The lock order is not a safety property ([safety.md](safety.md#reclaim-protocol),
  step 1); race test 7 checks it because a wrong order makes a Cargo that
  started first wait for diskgc.
- A test whose case does not apply to a toolchain (no newer Cargo, no
  `.cargo-build-lock`, no split build directory on 1.90.0) logs "not
  applicable" and is reported `PASS` by the runner (task-13).

## Controlled pause and kill points

Some tests must stop diskgc at an exact point (race test 5 pauses it
between discovery and locking; race test 6 kills it at seeded call
positions; the replace-between-walk-and-unlink test needs the replacement
at one exact moment). In the model these are events on the `Host` call
trace. In containers, a **test driver** (`diskgc-e2e-driver`, built from
`diskgc-e2e` only) runs single diskgc operations on a fixture (`walk`,
`delete`, `reclaim` with the in-use check, `held`, `cargo-targets`:
Cargo discovery and eligibility, which locks and deletes nothing, and
`cargo-reclaim`: the Cargo reclaim protocol on every target found, which
reports how long it held the locks) over
`LinuxHost` wrapped in a **test-only `Host` wrapper** that pauses or kills
it at a call position given by the harness. This is the one sanctioned
exception to "no hooks in the product": it lives outside the product
build, and the release build is checked not to contain it.

- **Positions.** The wrapper counts every port call, `Host` and `Delete`
  methods alike, with one counter from 1. The harness names a call as `K`
  (the K-th call) or `OP:N` (the N-th call of the port method named `OP`,
  e.g. `unlinkat:2`; unlike the model's `flock`, `flock_try` and
  `flock_wait` are counted apart), with `--pause-at` or `--kill-at`; the action runs before
  the call. A planned position the run never reaches fails the run.
- **Pause** is a handshake on the driver's stdio: it prints a control line
  (`diskgc-e2e-control: pause call=K op=OP nth=N pid=P`) and blocks
  reading a line from stdin; the harness checks in `/proc` that the
  process is a child of its strace, sleeping, inside `read(0)`, then
  changes the fixture and writes a line to resume it. Closing stdin while
  it is paused makes it exit with a failure instead of carrying on.
  `SIGSTOP` is not used: under strace a stopped tracee shows `t (tracing
  stop)` and `wchan` `ptrace_stop` alike in the transient
  signal-delivery-stop, in the group-stop and at every traced syscall
  (observed under strace with `--seccomp-bpf -f`), so the harness cannot tell when it is stopped for good, and
  a `SIGCONT` sent during the signal-delivery-stop is consumed before
  strace re-injects the `SIGSTOP`, which then never ends (kernel reading,
  not reproduced).
- **Kill**: the driver prints a control line and sends itself `SIGKILL`;
  strace then ends itself with the same signal. The killed run is judged
  by the oracle like any other.
- **Deleting** goes only through the wrapped host's `Delete` methods, so
  the pre-release sandbox gate applies; the wrapper only forwards and makes
  no permit. The source scan does not cover `diskgc-e2e`, so this rests on
  review.
- **Release check** (flake check `release-excludes-e2e`): the shipped
  `diskgc` binary contains no symbol of `diskgc-e2e`, none of the
  wrapper's type, control-line prefix or the driver's usage line, and its
  `cargo tree` (normal and build edges) has no `diskgc-e2e`. The same
  needles must all be found in the driver binary, and the shipped binary
  must have a symbol table; otherwise the check fails as vacuous.
- **Limits:** positions count port calls, not syscalls. `resolve` is
  several port calls and can be stopped between them, but one `openat2`'s
  multi-component path walk and a `/proc` reader's rounds cannot be
  stopped inside. The driver's `delete` is the deletion engine (its
  eligibility walk, then its delete; no in-use check yet), which sorts each
  directory's names, so positions are reproducible for a given tree; its
  `walk` is a plain listing walk holding one fd per level (a control for
  the fd-limit tests, which `--max-open-files N` runs under
  `RLIMIT_NOFILE` N).

## Container setup

Each container test runs in its own container (clean `/proc`, tests run in
parallel) with `--cap-add SYS_ADMIN` (effective only inside the user
namespace, needed for tmpfs, bind and stacked mounts and `unshare`),
`--cap-add SYS_PTRACE` (container root's strace needs it to resolve the
code under test's fds for `-y`; the code under test has no capabilities)
and `--network=none`. The harness runs as container root and mounts a fresh
tmpfs per test at `/tmp` and `/var/tmp` (scratch for fixtures), at `/run/diskgc`
(0755 root, where every image applies the packaged tmpfiles rule to create
`/run/diskgc/lock`, 0644 root) and at `/home/tester` (0755 tester); it keeps
its own records (strace output) on a further, root-only tmpfs. Objects the
harness creates thus get stable inode numbers and birth times; only what
the image itself ships stays on the overlay root.
**diskgc itself runs as an unprivileged container user (uid 1000)**, so
permission checks, `/proc` access limits and diskgc's own refusal to run as
root behave as on a real machine; other owners use mapped subuids (uid 1001
in the image). strace attaches first and the traced process then drops to
uid 1000 (groups, gid, uid) before exec; with `--seccomp-bpf` it stops only
at the traced calls, which the suite checks against a stated bound (a
20,000-file create/stat/unlink churn: one seccomp filter more than the
harness, at most 4 ptrace stops per file, at most 10 s). The Owner PID
tests listed in `HOST_PID_NAMESPACE_TESTS` (`suite.rs`) also get
`--pid=host`: Owner PID permits only in the initial PID namespace, so a
test that shows a reclaim, or contrasts a nested namespace with the
initial one, needs diskgc there. What that exposes, and how it is
confined: every host process is visible in the container (its pid,
`status` and command name; that is what Owner PID reads). The runner
refuses rootful Podman, and these containers get `--uidmap 0:1:65536`
(and the same `--gidmap`), so no container uid, root included, is the
invoking user: the user's host processes appear as an unmapped user,
whose environment, fds and `root` link the container cannot read and whose
processes it cannot signal (probed: host processes show as `nobody`,
`/proc/1/environ` is denied; each test requires its shifted `uid_map`). The filesystem is the container's scratch. The harness signals
only processes it started, nested PID namespaces' init through a pidfd.
The shifted map also makes in-container tester a different host uid from
other containers' tester, so their processes are not the user's to
diskgc; but it is the host uid of another container's uid 1001 (`OTHER`),
so a test that keeps a uid 1001 process alive must not run beside them
(none does today). Their `podman run` uses Podman's default `TMPDIR`, not the dev
shell's (below the checkout in a 0700 home, which crun, running as a
subordinate uid under the shifted map, cannot reach). The runner runs these tests one at a time (each picks a free
namespace pid range by looking at every host process). Nested PID
namespaces are made with `unshare -p -f -m` and entered with `nsenter`. A second
mount namespace for the "other namespace" tests is made with an `unshare`
helper that bind-mounts the scratch directory at a different path (nested
Podman is not used). Toolchains, tools, users, markers and the mount points
live in a base image (`dockerTools.streamLayeredImage`) tagged by
its Nix hash and reused when an image with that tag is loaded; the binaries
under test are bind-mounted read-only, so a code change does not rebuild
or reload the base; the toolchains' `/nix/store` is mounted read-only too
(Cargo toolchains under test). The container root must be Podman's
native overlayfs: the runner checks `podman info` before starting
anything and each test checks its root mount, and both refuse
fuse-overlayfs or another storage driver, whose inode numbers and birth
times the snapshot keys were not validated on.

**Images.** There are three ordinary test images, each with its own runner (`nix run
.#e2e-runner`, `.#e2e-runner-debian`, `.#e2e-runner-docker`) and the same
static musl binaries under test. All carry, baked in by the same commands (flake.nix,
`e2eImageCommon`), the sandbox marker, the users, the overlay-root fixture
file, the mount points and a **descriptor**, `/.diskgc-test-image`
(`diskgc-e2e/src/image.rs`), which says what would otherwise be assumed
about Nix: which directories setup bind-mounts read-only over themselves
(`read-only`), whether the image has Cargo toolchains under test
(`toolchains manifest`, the manifest above, or `none`: no manifest is read
and no Cargo test listed) and which tests it runs (`tests all` excludes
Docker-only tests; `tests portable` also excludes `NIX_ONLY_TESTS`;
  `tests docker` runs only `DOCKER_ONLY_TESTS` in `suite.rs`). A missing
  or malformed descriptor fails the listing and every test's setup;
  nothing falls back to "a Nix image". The runner names the image in its
  summary.

The fourth, VM-only image (`localhost/diskgc-e2e-vm`) uses the same marker,
users, mount points and descriptor. It contains BusyBox, strace and the
named `/usr/bin/buildah` process probe, and mounts the guest's Nix store and
test binaries read-only. `nix build .#checks.x86_64-linux.vm-historical-ctime`
boots its disposable QEMU/Podman host and runs the four `Tier::Vm` replays.
It also runs a separate dry run of the withdrawn `containers-image-tmp`
matching set and 3-day floor, as a user profile, with genuine
five-day kernel ctimes: a held spool is `in_use`, a free spool is eligible,
a fresh spool is `not_idle`, and a live producer's dormant spool is eligible.
The producer reads and closes its spool before going idle, has no candidate
reference in `/proc`, and reopens the same bytes when resumed. This is a
counterexample to general dormant-spool protection, not evidence that
Buildah itself reuses such a spool; it is why the built-in was withdrawn
(TASK-112). The original vector, now that user profile, independently
verifies free deletion and held preservation.
The VM check is part of `just e2e` and `just e2e-full`.

- **Nix image** (`localhost/diskgc-e2e-base`): busybox, strace, the pinned
  pip, uv, Go, npm, pnpm, Yarn Classic, ccache, PlatformIO and Jest cleaners, and the Cargo
  toolchains under test from the Nix store; descriptor `read-only
  /nix/store`, `toolchains manifest`, `tests all`. `just e2e`, `just
  e2e-full`. The Autotools source-anchor case runs pinned GNU Autoconf 2.73
  and its generated `configure` entirely in the disposable container, with
  the tool closure mounted read-only for that test. The West source-anchor
  case builds a real Zephyr 4.3.0 `native_sim/native/64` application through
  pinned West 1.5.0. Zephyr source is a flake-locked input bound read-only
  inside the container's offline West workspace; the application and both
  generated build trees live on scratch tmpfs. The shipped profile's dry run
  sees the fresh build but applies its idle floor, while a no-floor test
  profile reclaims only the build whose application source survives.
- **Debian image** (`localhost/diskgc-e2e-debian`): no Nix inside. Its
  userland is the official Rust image for the fast toolchain's release
  (`rust:1.99.1-slim-trixie`, Debian 13), pinned by digest in flake.nix:
  Debian's dash as `/bin/sh`, coreutils, gcc as the linker, and a
  rustup-installed, glibc-dynamic Cargo 1.99.1 whose real binaries (not
  rustup's proxies) the manifest names as `debian-1.99.1`. The one file
  added from Nix is strace, the static musl build of the release the Nix
  image has, copied as a plain file, so no `/nix/store` exists in the
  container. Descriptor `read-only /usr` (the toolchain lives there),
  `toolchains manifest`, `tests portable`: every test but the Nix-only
  ones, the Cargo race tests on its one toolchain (without their
  cross-version parts, below); the CI operator's journey runs its hook
  under dash. `just e2e-debian`, and the
  end of `just e2e-full`. Costs: the pinned image is about 0.9 GB in the
  Nix store and 0.93 GB in Podman's; building the static strace fetches
  about 370 MB of build tools.
  What it does not show:
  - the kernel is the host's (the development machine's NixOS kernel), as
    in every container;
  - the image's `/etc/passwd` and `/etc/group` are replaced by the test
    users', so Debian's own user database is not what is resolved;
  - rustup's proxies are not exercised (the manifest names the real
    binaries), and Debian's own rustc package (1.85 in Debian 13) is not
    tested;
  - `/usr` is bind-mounted read-only by the harness, unlike a CI runner's;
  - the ordinary fast suite uses the development build with the pre-release
    sandbox gate. The separate release runner mounts the ungated
    `release-01-candidate` package and runs catalog, action and package
    negatives plus real Cargo CLI reclaim, busy-lock and dry-run journeys
    on both Nix and Debian images. The race tests use the test driver and
    do not establish race coverage for the packaged binary. The candidate
    remains unapproved while TASK-67 release policy is unresolved;
  - one Cargo only, so the tests that need two (`NIX_ONLY_TESTS`) run on
    the Nix image only, and the cross-version parts of the others are not
    reached: race test 2's H2 rounds (a target the oldest stable built,
    then this Cargo) find the oldest stable to be the toolchain itself,
    log "not applicable (vacuous, not passed)" and the test counts as
    `PASS`. The build-directory checks are not vacuous there: they accept a
    missing build directory only from the release measured to ignore
    `CARGO_BUILD_BUILD_DIR` (1.90.0), never from "the oldest toolchain".
- **Docker image** (`localhost/diskgc-e2e-dind`): official
  `library/docker:29.8.2-dind`, pinned by manifest digest and Nix hash in
  flake.nix, with the common sandbox marker/users, static strace copied
  to `/bin/strace`, and SHA-256-pinned Alpine 3.24 `sd-tools`, `acl-libs`
  and `libcap2` APK contents; descriptor `toolchains none`, `tests docker`.
  No package manager runs inside the test image. Podman is
  only the outer runtime. The runner overrides the upstream dockerd
  entrypoint so the interlock runs first, then gives each native Docker test
  `--privileged` and separate tmpfs mounts at `/var/lib/docker` and
  `/var/lib/containerd` (512 MiB maximum each). The test starts native
  `dockerd` with a container-local Unix socket, no network bridge or
  iptables, and the containerd snapshotter enabled;
  `docker version` must report Docker Engine 29.8.2. Its
  scratch image comes from a local tar stream, with no registry access.
  The only host bind mount is the read-only Nix output holding the binaries
  under test; no host Docker socket or developer project path is mounted.
  The smoke test checks that imported image blobs exist under the bundled
  containerd root derived from `docker info`. A separate system containerd
  socket still maps to `unsupported_backend`. Native prune tests cover cache
  and image deletion, preservation, and socket replacement. Run the full
  Docker suite with `nix develop -c just e2e-docker`. If the native image
  prune test gets no JSON line, its assertion prints the exact argv, exit
  code, signal, stdout and stderr, plus bounded tails of each process's raw
  strace file and the private dockerd log. This preserves the first failure
  in the grouped runner's host log even though that container is removed.
  A passing rerun does not identify the cause of an earlier missing report.
- **GC roots.** The container recipes build each runner to an out-link
  in `.gcroots/` at the repository root (gitignored, outside `target/`,
  so `cargo clean` does not drop it) and run it from there; `just e2e`,
  `e2e-full`, `e2e-debian` and `e2e-docker` also root the dev shell's inputs
  (`.gcroots/dev`, its `inputDerivation`). A runner's closure holds its
  image's inputs (the toolchains, the pinned Debian and Docker images, the built
  layers), so a Nix garbage collection on a low disk does not delete them
  and force a re-download. Each build replaces its link, so roots do not
  accumulate. What stays on disk, measured 2026-10-19: closures of 3.0 GB
  (the Nix runner), 1.2 GB (the Debian runner) and 1.5 GB (the dev shell),
  4.4 GB together before adding the Docker runner. Podman's own copies of
  the loaded Nix and Debian images (2.84 GB and 0.93 GB) come on top.
  Loading an image needs room beyond what it keeps:
  the Debian image's load took 2.8 GB of free space at its peak, 1.9 GB
  of it only during the load (one measurement, sampled every 100 ms; the
  Nix image's load was not measured). To give it all back: `just
  gcroots-drop` (`rm -rf .gcroots`), then `nix store gc`, and `podman
  rmi` the `localhost/diskgc-e2e-*` images.

## Safety interlock for destructive tests

An environment variable is not enough (it leaks through `export`, direnv,
CI settings). Three layers:

1. **Build-time:** destructive tests live in `diskgc-e2e`, excluded from the
   workspace; no `cargo` command in the workspace compiles it. The flake
   builds it; its test binary reaches a container only through the
   read-only under-test mount. Its host-side runner starts containers and
   runs nothing destructive.
2. **Run-time:** the test binary refuses to run unless a marker file baked
   into the test image exists (`/.diskgc-test-sandbox`, holding a constant
   also compiled into the binary) and Podman's container marker
   (`/run/.containerenv`) exists. (A kernel command-line token would not
   work: a container sees the host's `/proc/cmdline`.)
3. **Per operation:** every test asserts that the paths it touches are on
   the scratch tmpfs the harness mounted for it (checked by mount id), and
   aborts otherwise.

The CI operator journey has one test-control file beside the image marker:
`/.diskgc-test-clock-offset`, containing decimal positive seconds and one
newline. The harness writes it only in the disposable container's overlay
root, never in a host bind mount. A development `LinuxHost` reads it once
at construction only after both sandbox markers pass; absent means zero,
invalid means construction fails. The file advances only the realtime value
returned by `LinuxHost::wall_clock()`. `CLOCK_BOOTTIME` deadlines and actual
kernel ctime remain unchanged. The feature
`diskgc-linux/release-without-sandbox-gate` compiles
out the offset path and reader. The `release-excludes-clock-offset` flake
check compares the 0.1 candidate binary with a positive development
control. The default package remains gated. The separate
`release-01-candidate` output combines the proposed 0.1 catalog (Cargo,
`nix-shell-rc`, `pytest-tmp`, the idle filters) and
lifted gate solely for testing in disposable containers. It is unapproved
for deployment until TASK-67 closes; operator policy decisions remain
unresolved. The `just e2e` gate runs candidate negatives
and the real Cargo reclaim, busy-lock, rebuild and dry-run CLI cases on Nix
and Debian images through their candidate under-test runners.

## Commands

### Development scratch

Run `nix develop -c just scratch-tally` before starting each backlog
implementation task. It resolves the main checkout through Git's common
directory even when invoked from a nested task worktree. The tally uses
`du -x` without following
symlinks, skips entries on another filesystem, and reports every child of
the ignored `./tmp/`, the aggregate legacy `/tmp/diskgc*`, the old root
`target/`, and available space. It records one sample in the bounded
`tmp/scratch-tallies.jsonl` to show change since the previous tally. The
history retains at most 1,024 records and reads at most its last 1 MiB;
a corrupt final record is skipped with a warning. It warns
when tracked scratch grows by at least 2 GiB or available space falls below
10% of the filesystem or 20 GiB. A warning calls for inspection; it does
not remove anything or block a task.

Create new agent worktrees under the main checkout's
`./tmp/worktrees/<task-name>/`. The dev shell resolves the worktree root
even when entered from a subdirectory, sends Cargo output to its
`./tmp/target/`, and sets `TMPDIR` to its `./tmp/system/` for host-side
test fixtures. The E2E runner and recipes write logs under
`./tmp/e2e-logs*`; Python bytecode goes under `./tmp/pycache/`.
Nix store objects, container-private `/tmp`, rootless
container storage and the small `.gcroots/` symlinks have their own required
locations. The product's host `/tmp` profiles are unaffected.

On 2026-10-22, the legacy `/tmp/diskgc*` inventory contained 503 entries:
88 registered Git worktrees, 96 other directories and 319 files. Sixty of
the registered worktrees had tracked or untracked changes; 28 were clean.
The legacy entries occupied 133.79 GiB, and the old root `target/` occupied
7.89 GiB. A process cwd/fd/map snapshot found no references to those paths,
but 313 process fd tables were unreadable. This does not prove the paths are
inactive. Before moving any legacy worktree, inspect its
Git status and live process references, and retain its branch and build
artifacts. Do not delete host data without explicit user authorization.
Run `nix develop -c python3 tools/scratch_inventory.py --json` for a fresh
path-level report: each entry's size, registered branch or detached HEAD,
dirty count, nested `target/` bytes and observed process cwd/fd/map references.
Unreadable process tables are counted; an empty observed-PID list is only a
snapshot, not proof that a path is safe to move.

| `just` recipe | What | When |
|---|---|---|
| `scratch-tally` | Project and legacy scratch sizes, changes, free space and warnings | before every backlog implementation task |
| `check` | fmt, clippy, unit, planner, model tests, property tests (few cases), profile golden sets, `systemd-analyze verify` of the shipped unit, in the dev shell | constantly; seconds |
| `e2e` | `check` plus fast Nix-image tests, candidate 0.1 artifact probes on Nix and Debian, the native Docker smoke, a profile-vector replay under Docker Engine, and the four-vector QEMU guest check | before every commit (the gate); each ordinary runner has a 5-minute budget including setup |
| `e2e-full` | every Nix-image test, then the Debian and native Docker suites, the four-vector QEMU guest check, and the cold ext4 scale VM check | at the end of every wave, in CI on every push, and before any task labelled `irreversible` is marked done; runner deadlines guard against hangs |
| `e2e-debian` | `check` plus every test but the Nix-only ones on the Debian image (Container setup: Images) | with changes to the static binary's behaviour on a non-Nix machine, the image, or the descriptor; a 15-minute deadline only guards against hangs |
| `e2e-docker` | `check` plus Docker-only tests in the pinned native dind image | with changes to Docker fixtures or Docker kinds |
| `e2e-one NAME` | one container test, keeping the container on failure | while working on one |
| `e2e-debian-one NAME` | the same on the Debian image | while working on one there |
| `e2e-docker-one NAME` | one native Docker test in a disposable dind container | while working on one there |
| `soak` | property tests with many cases | before a release, nightly CI |
| `fuzz [TARGET] [DURATION]` | `cargo-fuzz` targets (grammar, YAML loader, mountinfo, `maps`, `environ` parsers), with 1,800 seconds per target by default | hardening blocks, nightly CI |
| `mutants` | complete selection and executor mutation sweep in bounded Podman shards, with verified outcomes | hardening blocks |
| `docs-check` | offline local links and heading fragments in `reference.md` and `safety.md`, plus the core's document contract tests | after contract edits |
| `scan-timing [SCANS] [--pytest-facts]` | read-only: the in-use process scan's age when returned on this host (min, median, p95, max), with the host (gnu) build and the shipped static musl target | with changes to the `/proc` readers or the scan; against docs/safety.md's Freshness (TASK-115) |
| `ci` | `nix flake check -L` plus `just e2e-full` | CI |

`just fuzz` enters the pinned 2026-10-01 nightly shell without changing the
product toolchain. Its five targets live in `fuzz/fuzz_targets/` and call the
production parsers directly. Seeds are versioned in `fuzz/seeds/`; evolving
corpora and crash artifacts stay in the worktree's `tmp/fuzz/`. Run a single
target with `just fuzz grammar 1800`, or all five with `just fuzz`. A crash is
not done until its input is reduced, saved as a regression and fixed. The
runner also keeps full build and run logs in `tmp/fuzz/logs/`. The
`just mutants` inventories all five named sources, finds complete nonempty
partitions of at most 16 mutants per shard, then runs each shard through the
resource-capped Podman runner. It verifies every report before returning
success. Reports stay in `tmp/mutants-container-smoke/`. Host `cargo-mutants`
is used only for listing; builds and mutation reports are created in the
container's 8 GiB tmpfs. The recipe checks that `tmp/system` is Git-ignored,
and the runner tells `cargo-mutants` to honor Git ignores so its source copy
cannot include its own scratch tree. Each shard runs an unmutated baseline
and `cargo test --workspace` for its mutants, including `diskgc-modelgen`'s
independent selection oracle. `just check` tests the clean workspace
separately. Surviving mutants fail verification and need a test or an explicit
justification before the hardening task is done.

To inspect or run a single bounded shard, use
`nix develop -c tools/mutants_container_smoke.sh --check-shard parse 0/16`
to check a bounded selection and read-only mounts without compiling. Use
`nix develop -c tools/mutants_container_smoke.sh --run-shard parse 0/16`
for one batch only after its disk and RAM guards pass. The source choices are
`parse`, `check`, `evaluate`, `mod` (the four selection modules), and
`executor`; choose a shard count that selects at most 16 mutants per shard.
The full `just mutants` recipe computes a valid count for each source and
stops if the source cannot fit the 64-shard limit, a resource guard fails, a
shard fails, or report verification fails.
First run `nix develop -c python3 tools/verify_mutant_sweep.py scope` to
compare the five named sources with the complete `just mutants` selection;
it fails if a new selection module was added without a shard plan. For each
source, then run
`nix develop -c python3 tools/verify_mutant_sweep.py plan SOURCE TOTAL`.
This lists the live unsharded inventory and every shard, failing on an omitted,
duplicate, extra, empty or oversized selection. It saves the checked partition
under ignored `tmp/mutants-container-smoke/plans/`; the source/test/fixture
snapshot, mutation runner procedure, source hash, `cargo-mutants` version, and
resolved cached container image ID identify it. Every shard preflight compares
its selected names and pinned image ID with that plan and a fresh unsharded
inventory. After
running all shard indexes, run
`nix develop -c python3 tools/verify_mutant_sweep.py verify SOURCE TOTAL REPORT_DIR ...`
with exactly one successful report directory per index. Verification rejects
missing or duplicate indexes, changed source inventories, incomplete outcomes
and unsuccessful baselines or runs. It checks reported outcome counts and
rejects every missed, timed-out, or unviable mutant by name. Each report must
match the complete plan identity, including the runner command, tool version,
and image ID. Inspect those reports and add a test or record a specific
justification before considering the hardening task done. Reports from an
older workspace snapshot must be rerun. Do this for all five sources before
treating the mutation sweep as done.
The script keeps the build and target on an 8 GiB tmpfs in a disposable Podman
container. The source, Cargo registry and Nix store mounts and container root
are read-only. The runner resolves the cached image tag once and uses that
immutable image ID for both containers. The cached image and Podman graphroot
share the host filesystem; the runner requires 16 GiB of host free space plus
64 MiB to start, then stops if the remaining headroom falls below 32 MiB. It
checks host disk and RAM before preflight, then checks host disk, runroot tmpfs
and memory during a two-hour maximum mutation
run. The host run log is capped at 8 MiB. After the mutation process exits,
it copies at most 16 MiB of reports, checking the host disk reserve during
transfer. It verifies the baseline, per-mutant outcomes, and category totals,
retaining valid survivor reports for diagnosis. It checks that the plan and
runner identity did not change during the run and removes the container.
The selected names, run log, exit status and copied reports live in
`tmp/mutants-container-smoke/`; Podman metadata and container image storage
also use host disk. On interruption or resource pressure, the container is
stopped and removed, and incomplete host logs remain. A checked shard
or a single successful batch is not evidence that the complete mutation
sweep passed.

For this hardening task, record a separate 1,800-second successful run for
each target, fix every crash with a saved regression, validate both contract
documents, and run the mutation recipe. A surviving mutant must be tied to a
specific reason or a new test. The ordinary `just e2e` gate and parallel QA
and architecture reviews still apply before its commit.

`nix flake check` can report every check passed while building nothing
(its outputs are already in the store from an earlier build of the same
inputs, or were never scheduled); a gate that must prove the tests ran
builds each check explicitly with `nix build -L --no-link --rebuild
.#checks.x86_64-linux.<name>` (task-61 tracks making `just ci` do that).

Requirements for `just e2e`: Nix and rootless Podman on the host (native
overlay storage driver, see Container setup), with a
subuid range covering uids 1000 and 1001 (the dev shell does not provide
Podman; CI runners that are themselves containers need nested Podman).
Each test's tier is declared in `diskgc-e2e/src/suite.rs`. Coverage of unit and model tests
via `cargo llvm-cov`.

### Walkthrough driver

Walks (the phase3.1 acceptance signal) run in a disposable rootless Podman
container from the e2e base image with the 0.1 release package on `PATH`,
seeded data on size-limited tmpfs mounts and a host-shaped process load
(`tools/walk/`). Every reclaiming run in a walk happens there.

| `just` recipe | What |
|---|---|
| `walk-up [NAME]` | container and tmux session `walk-diskgc-NAME` (default `podman`), with the tester's shell |
| `walk-reset [NAME]` | regenerate the data from `WALK_SEED`; `WALK_TMP_BULK=N` adds N bulk `/tmp` leftovers |
| `walk-install NAME BIN TAG` | copy a diskgc binary into the container as `/usr/local/bin/diskgc-TAG` |
| `walk-down [NAME]` | remove the container and its tmux session |

`NAME` is positional (`just walk-up review`; `NAME=review` is refused).
`tools/walk/step.sh DUMP COMMAND` types one command into the session named
by `WALK_ENV` and saves the screen.

Each name is its own container with its own uid/gid slot
(`tools/walk/uid-slot.sh`): container root stays the invoking user (so the
container can read the bind-mounted checkout under a 0700 home), and
container uids 1..2047 map to a block of 2,047 subordinate ids. Slots are
2,048 ids wide, so the usual 65,536-id `/etc/subuid` range gives 32; the
script reads the effective range from `podman unshare`'s uid and gid maps
and refuses fewer than two slots. A name other than `podman` starts at a
slot derived from its checksum and takes the next one no running container
labelled `diskgc.walk.uid-slot` holds; two `walk-up`s racing for one slot
both back out. `podman` is slot 0, Podman's default host uids for those
users, which it shares with every default-mapped rootless container,
including walk containers started before slots existed (they carry no
label and are not counted).

Why slots: the in-use scan cannot read a process of the user's own uid in
another user namespace and keeps whatever it could use, so two containers
whose tester is one host uid, and that see each other's processes, reclaim
nothing (reproduced 2026-10-25: a `--pid=host` probe on a walk container's
slot kept all 8 Cargo targets `in_use` by that container's load; on a
distinct slot it reclaimed them).

By default a walk container has its own PID namespace and runs with
`--init`, so the holders `walk-reset` stops are reaped rather than left as
zombies that grow the process table, and with it the in-use scan's cost,
across a series of resets. `WALK_PID_HOST=1 just walk-up NAME` joins the
host's initial PID namespace instead, which Owner PID needs (rc dirs and
pytest locks of `/tmp` profiles are otherwise `producer_unknown`;
[safety.md](safety.md)); there Podman refuses `--init`, the container sees
the host's and other walk containers' processes (which is when slots
matter), and container root, being the invoking user, could signal the
user's host processes: `reset.sh` signals a holder only while its pid
still has the start time recorded when it was started.
