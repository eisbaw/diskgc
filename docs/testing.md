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
fixture object must keep its inode. Every other host test that runs the
binary gives `ensure` `--dry-run` and `watch` `--tally-only`, also where
the command line or the configuration is refused before anything runs;
the reclaiming forms of those refusals run in containers (for example
`invalid_config_never_falls_back_or_reclaims`,
`release_candidate_watch_refuses_user_profiles_and_unshipped_builtins`). `tools/host_reclaim_scan.py`
(`just check`) enforces this: it reads the host test sources (the Rust
tests, benches and examples of the workspace and the fuzz project, and
`tools/test_*.py`; not `diskgc-e2e`, which runs only in Nix builds and
containers) and fails on an `"ensure"` argv literal without `"--dry-run"`,
or a `"watch"` one without `"--tally-only"`, in the same list, outside the script's
allowlist, whose entries each name a function, pin a count and give a
reason. It is a text tripwire, not a proof: it does not see argv built
from variables or formatted strings, a command line written as one string
(`sh -c "diskgc ensure ..."`), a script a test runs that names `ensure`
itself (`tools/test_agent_pre_job.py` runs `docs/agent-pre-job.py`, so its
stand-in `diskgc` is all its `PATH` holds), nor a reclaim run in-process
on a `LinuxHost`.

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
   `--min-free` and exits 0, cold (no snapshot) and warm (its own
   snapshot from the previous run); the time to the first deletion is
   logged for both and is not a pass criterion (the PRD sets no latency
   target).

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
  A test that needs a socket nobody listens on binds one that never
  listens, never a closed listener: a child another test forked and has
  not yet exec'd keeps a closed listener listening, so a connect gets end
  of file instead of `ECONNREFUSED`. This was TASK-345's intermittent
  `refused: Eof` in `the_daemon_probe_classifies_what_the_socket_does`
  (5 and 15 in 3,000 probes under concurrent spawns); pinned by
  `a_closed_listener_keeps_listening_while_a_forked_child_holds_it`.
- **A test child's exit does no work that scales with the machine.** The
  wait for a child (`CHILD_EXIT_BOUND` in the `diskgc-linux` host tests)
  turns a slow exit into a failure naming the child's state and kernel
  wait channel; it is not a speed target. Unmounting a non-volatile
  overlay syncs its upper layer's whole filesystem, the host's, so a
  namespace helper's private overlay is mounted `volatile`. This was
  TASK-317's intermittent timeout of
  `a_lock_file_on_an_overlay_is_not_copied_up` on a loaded host: the
  helper had printed its result and waited in `wb_wait_for_completion`
  while the host's dirty data was written back.

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
  something in the model frees space in the model. A mount's `free_lag`
  shows what an `unlinkat` frees only that much model time later (a
  filesystem that releases blocks asynchronously; TASK-334's wait rule,
  `crates/diskgc-model/tests/space_wait.rs`).
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
  built-in source in each role below) and `release` (the 0.1 and 0.2
  catalogs: what each admits and what it refuses).

The invocation keys: `profile` (shorthand for `select: profile:NAME`) or
`select` (`--select`; neither: the configured groups run), `config` (the
configuration file's text; absent: none was found), `catalog` (the
build's built-in catalog: `development`, the default, `release-0.1` or
`release-0.2`, that release's row of the release descriptor, which a
`release-catalog` build of that version selects; refusals name `diskgc
0.1` or `diskgc 0.2`), `user_profiles`
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
argv}`), `report` / `dry_run_report` (optional subsets of the JSON
report, e.g. `{status: met, seed: 42}`), and `check_warnings` (optional,
with a `config`: `check`'s root warnings in order, each a text it must
contain, and no others; `invocation::check_roots`, the decision `ensure`
refuses missing roots with).

Every vector is also held to invariants it does not state: the dry run
never calls `unlinkat` or `fchmod` and runs no command; nor does `check`'s
root check, run on a fresh host for every vector whose configuration loads
(a lookup that reaches a `dead` mount fails it there too); every `unlinkat`
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
its producer (none now: `backend_unavailable` got its producer in TASK-36,
the `nix-daemon-*` and `nix-socket-absent` vectors; `stale` got its
producer and vector in TASK-31, `races/unit-gone-before-rematch-is-stale`);
the test fails once one enters `Reason::ALL` or is shown by a vector, so a
reservation cannot outlive its producer;
`every_builtin_source_has_match_in_use_guarded_and_nested_vectors` needs,
for each built-in source run unchanged, an action of it, an `in_use` or
`locked` skip, a `protected` or `excluded` skip, and a nested-guard skip
(`keep_marker`, `contains_checkout` or `contains_special`); Docker prunes need
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
  not answer; `dead: true` is a mount whose server never answers, and any
  call that reaches it (`openat2` to or below it, `statx`, `getdents`, a
  read, `statfs`, `statvfs`) fails the test, as it would hang on Linux. A repeated device requires `bind_of: <earlier mount id>` and
  exposes that filesystem's root through a distinct mount ID, or with
  `bind_root: /dir` that directory of it (a path from the filesystem's
  root, mountinfo's root field; missing directories are created).
  Filesystem metadata must match. A bind never repeats the mounts below
  its source (it is non-recursive, or private), so it shows a mount point
  there as the uncovered directory; removing or renaming that directory is
  still `EBUSY` through every mount of its filesystem, as Linux marks the
  directory, not a mount showing it (TASK-290). An overlay's directories
  count as the overlay's own there: a mount on an upper directory does not
  make the merged entry busy in the model, where Linux's rmdir through the
  overlay reaches the upper directory and is `EBUSY` (a divergence that
  lets the model delete where Linux refuses, never the reverse). `fs: overlay` with `overlay_upper:
  /path` (a directory on an earlier mount) shows that directory's tree
  under the overlay's own `dev` and root field `/`, so an unlink on either
  side removes the entry from both; lower layers, whiteouts and copy-up
  are not modelled. On a device of major 7, `loop_backing: /path` is what
  `/sys/dev/block/7:N/loop/backing_file` reads (absent: `EACCES`); the
  image's bytes are not tied to that file's. `members: ["8:1", "7:5"]` on a
  btrfs of major 0 lists the block devices it is stored on, which
  `loop_devices` reports through a descriptor of it (an error without
  one, as on Linux); partitions of a loop device are not modelled.
  `idmap: [{stored: 0, shown: 1000}]` makes a mount idmapped (mountinfo's
  `idmapped`): `statx` and the listing permission show each stored owner
  and group mapped, an unlisted one as 65534; an overlay's idmapped
  layers are not modelled.
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
  answers to Owner PID's preconditions and the in-use check's visibility
  rule (default: the initial namespace, no `hidepid`);
  `pid_namespace: test_container` is a nested namespace inside a marked
  test container of the e2e harness's build, which the visibility rule
  excepts and Owner PID does not (refused with `sandbox_gate: closed`).
  The top-level `loops:` (`{dev: "7:N", backing: TEXT, file: PATH,
  readable: true}`) are configured loop devices no mount shows, listed
  with the mounts' `loop_backing` by `loop_backing_files`. `file` is the
  file the device holds by the name it opened (default: the text; given
  apart for another namespace's rendering or a decoy mounted over the
  name); the text follows that opened entry as `d_path` follows its
  dentry (a rename of it: the new path; unlinking it: the text with
  ` (deleted)`, other links or not). `readable` (or `own_uid: 0`) lets
  diskgc open the device, so `loop_backing_identity` reports the held
  file's identity; otherwise it is unavailable. A mount's `fs:` name gives `fstatfs` its magic (unknown
  names: 0, never on the local list). The top-level `own_uid_map` is diskgc's
  own (same default, `none` too), `own_uid` its user id (default 1000; 0
  models running as root), `own_cap_effective` its effective capability
  set as its own `proc_status` shows it (default: every capability for
  uid 0, none otherwise), and `sandbox_gate` the pre-release gate as
  `LinuxHost` has it (`open`, the default; `closed`: `may_delete` and the
  deleting calls are refused; `refuses_deletes`: only the deleting calls
  are). `command_exit` is the exit code of a modelled command action
  (default 0); `command_error_after_start: true` models a read or wait
  error after spawn, so the executor must still report an action and
  observed space change; `command_lines_not_forwarded` (default 0) is the
  count of output lines stderr did not take that every started command
  (an action, a prune, the Nix GC) reports, which the executor logs on
  the action's line and never turns into a failure (TASK-342). The model records argv, environment and checked
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
  `listable`, `read_head`, `unlinkat`, `fchmod`, `bound_unix_sockets`, `statvfs`,
  `proc_identity`, `proc_identity_released`, the `/proc` readers
  `proc_fds`, `proc_links`, `proc_maps`, `proc_environ`, `proc_status`,
  `proc_uids`, `proc_cmdline`, `proc_root_lookup`, `docker_query` or
  `run_command`), `mount` (`{id, at, fs, dev,
  size}`: a fresh filesystem mounted on an existing directory, e.g. a FUSE
  mount appearing in a candidate; with `bind_of: <mount id>` and optional
  `bind_root`, a bind of that mount's filesystem, an overlay's included,
  whose `fs` and `dev` must be the ones it shows), `unmount` (a mount id;
  no mount may be on it), `move_mount` (`{id, to}`: `mount --move` onto an
  existing directory; id, device and root field stay). `remove` is `rm -r`: every entry below is unlinked, and an
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
    not modelled; permissions are not checked, except a directory's read
    permission on a listing (`read_dir`, and `listable`, its open alone):
    its owner's `r` bit for its owner, the others' for anyone else (the
    group class is not modelled), none for `own_uid: 0`, so a directory
    of mode `0333` is searchable but cannot be listed;
  - inode numbers are never reused, so a replacement that reuses the freed
    inode number cannot be modelled;
  - `/proc` link text is the path at open time (Linux follows renames);
  - mtime, ctime and birth time are equal at load, so "ctime newer than
    mtime" cannot be expressed;
  - one task per process; diskgc's own process has only its fds;
  - signals only as an event that sets the port's interruption flag
    (`Host::interrupted`), with no delivery semantics; `read_dir` lists names in name
    order, while the kernel's order is the filesystem's, so callers must
    not rely on it;
  - processes come from the model or a `fork` event (no exec); memory
    maps show the mapped file's own device (a layer's device, as overlayfs
    can show, is modelled with a second filesystem);
  - link counts and reachability are derived once per filesystem and
    cached until it next changes (TASK-28's 98,000-entry index test runs
    in seconds); a path from an inode and available space are still
    linear per call, so 100,000-entry models that use them need indexes
    first.

Model scenarios required by [safety.md](safety.md) and
[architecture.md](architecture.md), among others:

- an entry renamed out of a candidate, or replaced by another inode,
  between the walk and its unlink: the replacement is never deleted, the
  candidate is `changed`;
- a candidate directory renamed elsewhere mid-delete: deletion continues on
  the checked object only;
- a user-owned directory containing another user's file: `not_owner`;
- an index block (or consecutive candidates) whose later member is
  opened by a process after an earlier member was deleted: the later member is skipped (`in_use`) when the scan
  used for the earlier member is older than the reuse limit or the run
  spans a suspend ([safety.md](safety.md#in-use), Freshness); an open within
  the reuse window is the accepted limit 5 and is pinned as such
  (`a_block_member_opened_after_an_earlier_member_was_deleted_is_in_use`;
  candidates of a liveness tier found ahead share one scan,
  `a_liveness_tier_from_the_index_shares_one_scan`; a liveness tier's
  found-ahead batches grow from 64 and a plain tier's do not,
  `found_ahead_batches_grow_only_on_a_liveness_tier`);
- one scan serving consecutive `fs` candidates of a `delete` (TASK-203;
  property test `no_fs_delete_starts_on_a_scan_past_the_bound`, with
  random scan, unlink and final-check durations): no deletion starts on a
  scan one second old or older, a scan is replaced only once it could not
  serve the next deletion, and with scans under a second no attempt is
  spent; a command action keeps the 500 ms window;
- a host too busy for a fresh in-use scan (every scan finishing older
  than the freshness bound): nothing is deleted on a stale scan, the
  candidate is skipped `in_use_unknown` after its three attempts and the
  next one is still tried, any other outcome resetting the count
  (`vectors/inuse/busy-host-*`, TASK-144); after three in a row the tier
  backs off before each further check, the pause growing, and checks
  every candidate it reaches in full (TASK-340; diskgc-model
  `tests/busy_host.rs` through the executor): a storm that settles is
  waited out and the rest reclaimed, in a plain, a union and a Cargo
  tier; a storm of exactly three costs one pause and two in a row none; a
  storm until the deadline deletes nothing, uses the deadline and no more,
  ends `deadline` (exit 124, not `exhausted`) with the unreached
  candidates in no row and `kept` counting only the checked ones; a pause
  that oversleeps the deadline ends at once and is reported in
  `overruns`; a tier's own `timeout` reached during a storm excludes the
  tier with an error naming the busy host, and the next tier runs; space
  freed by someone else during a pause ends the tier at the goal without
  another deletion; a signal ends a pause within one slice. A `watch`
  cycle in such a storm holds the lock to the cycle's deadline and no
  longer, records `deadline` (class `retry`), and the next cycle reclaims
  (`tests/watch.rs`,
  `a_busy_host_holds_a_cycle_to_its_deadline_and_the_next_cycle_reclaims`).
  A process that cannot be inspected counts toward the three but skips
  the tier's rest without a back-off, each skip naming it
  (`unreadable_process`, its pid; diskgc-model
  `an_unreadable_process_counts_toward_the_in_use_unknown_breaker`, and a
  `newgrp`-like refused shell in `busy_host.rs`), also when a process
  whose reads do not settle is in the same scan. A process whose reads do
  not settle (TASK-348, the model's `proc: unsettled`: a linker exiting
  during a busy build) is a busy host's: the candidate is waited out and
  checked again, and once it has exited every candidate is reclaimed; one
  that never settles is taken to be stuck after four back-offs (30 s) and
  skips the rest naming it, also when its status would excuse a refusal
  (`busy_host.rs`). A scan that does not settle is waited out for the
  candidate it was made for whatever the size of the tier (TASK-364,
  `busy_host.rs`, a storm modelled as a fork after each listing): a tier
  of one candidate and one of two reclaim after one 2 s back-off; a dry
  run waits the same way and plans the candidate eligible; a storm until
  the deadline ends a one-candidate run and its dry run `deadline` (124)
  with the deadline used and no more, the candidate kept `busy` (a row in
  the plan too); a signal during the waits stops the reclaim and the dry
  run within a slice; space someone else frees during them ends the tier
  at the next back-off, also in a storm that outlasts the deadline; an
  oversleep in them is one `busy back-off` overrun, and a slow space
  sample after one is one `statvfs` overrun, not both; a dry run busy
  until its deadline counts the candidate it waited for once (kept, not
  also not judged), in the Cargo, `fs` and `fs` union plans; a tier's own
  `timeout` during them excludes it naming the busy host; no back-off is
  taken holding a candidate's `flock` or a Cargo target's locks (a host
  that checks at every sleep, `busy_host_locks.rs`; the protocol's own
  short pauses, at most 1 s, stay under them); a lasting
  cause (a `newgrp` shell) keeps a one-candidate tier and its plan at once
  without a back-off; a Cargo target the host turns busy on after its
  first part was deleted is recorded part-way, not run again (a run again
  lost the first part's action from the report). The project contracts'
  command line (TASK-366, the
  model's `cmdline: unsettled`: TASK-348's e2e run 1, a container process
  exiting while it is read): a dry run and a reclaim of two `node-modules`
  projects wait one back-off and take both, where the base kept both
  `in_use`; one that never holds still, after four back-offs, and one that
  cannot be read (`cmdline: unreadable`), at once, keep both `in_use`
  naming it, in the reclaim and in a dry run (`busy_host.rs`). Those of
  the waits failed on the stack's base (TASK-317 + TASK-348), and each
  mechanism has a mutation that fails one (no busy note for the command
  line, a wait for any cause, no stuck bound, no retry in the reclaim
  walk, no wait in a plan, the goal not sampled between waits, the busy
  flag at a tier timeout, the waits' overrun). Not pinned: that a scan in
  which a process held still restarts its 30 s (never restarting only
  takes it to be stuck sooner, which keeps candidates). TASK-337's union
  tests (`fs_union_in_use.rs`) forced the first term's `in_use_unknown`
  with a storm; since a storm is waited out they force it with a refused
  shell, one that exits between the two terms' checks for the bite, and a
  storm variant pins that plan and reclaim both wait under the first
  term. A refusal seen
  earlier in the scan still decides when its later rereads only do not
  settle, and no status (another user's, setuid, more capable,
  non-dumpable) ever excuses a read that did not settle (`busy_host.rs`;
  diskgc-core `inuse::scan` unit tests). Which read
  failures count as not settling is pinned on the readers' scripted
  rounds (diskgc-linux `tasks.rs`,
  `a_live_task_that_showed_nothing_once_is_failed_not_unsettled` and the
  churn cases). Not in a container: a fork storm cannot be timed to
  diskgc's listings there (the kernel cases pin only that no unsettled
  scan clears), nor can a process held mid-exit or one exiting while its
  command line is read, and a process diskgc cannot inspect, which a
  container can make, does not back off. On a real kernel the waits run
  only when a run meets real churn (TASK-317's gate met it in the serial
  host-PID tail of `release_candidate_pip_cache_*`); no container test
  stages it on purpose;
- a bind mount of a subdirectory of a protected tree (TASK-260;
  `/mnt/x` binds `/gen/x` of `/tmp`'s filesystem): a candidate under the
  bind is `protected` while the match is the bind source's ancestor
  `/tmp/gen`, and a candidate holding the source is `protected` while the
  match lies under the bind; a reclaim through the bind deletes nothing
  protected; the same for `!profile:` (`excluded`) and for kind
  `cargo-target` on either side; a filesystem mounted deep inside a
  protected Cargo target and mounted again elsewhere is `protected` there,
  and one mounted inside an `fs` match stops the run at enumeration
  (diskgc-model `tests/fs_dry_run.rs`
  `protect_covers_a_bind_mount_of_a_protected_subdirectory` and its
  neighbours, `tests/fs_reclaim.rs`
  `a_reclaim_through_a_bind_of_a_protected_subdirectory_deletes_nothing_protected`,
  `tests/ensure.rs` `a_cargo_target_under_a_bind_of_a_protected_subdirectory_is_protected`,
  `a_protected_cargo_target_seen_through_a_bind_protects_its_source_tree`,
  `a_filesystem_mounted_inside_a_protected_cargo_target_is_protected_elsewhere`);
  a filesystem mounted on a private bind of a protected subdirectory and
  again elsewhere stops the run at enumeration, the match holding that
  mount by place (TASK-290:
  `protect_stops_the_run_on_a_filesystem_mounted_on_a_private_bind_of_its_subdirectory`);
  a rename above the source of a bind a candidate is reached through, or
  a `CACHEDIR.TAG` made above it under a Cargo `protect` root, after the
  enumeration, keeps the candidate `protected` (TASK-285:
  `vectors/config/protect-rename-above-a-bind-source`,
  `vectors/config/protect-cargo-tag-above-a-bind-source`);
  which mount table differences make a check enumerate again
  (TASK-300; safety.md, Which table differences count): mounts that
  come, go or move far from every root (a container's overlay, its
  network namespace) enumerate nothing again and change no verdict,
  one made directly in a root's parent does
  (`tests/fs_reclaim.rs`
  `container_mount_churn_far_from_the_protect_roots_enumerates_nothing_again`);
  a bind mounted above a root, a mount moved into one, and a rename
  above a root seen only through an unrelated mount's moved point keep
  the candidate or stop the run
  (`vectors/config/protect-bind-mounted-above-a-protect-root`,
  `protect-mount-moved-into-a-protect-root`,
  `protect-rename-above-a-root-seen-by-an-unrelated-mount`); and, as a
  property over random tables, binds, renames, moves and remounts,
  whenever a difference is judged not to count, every table question
  the enumeration asks answers alike (`places::relevance` unit tests);
  with any match, a candidate or match on an `overlay` mount is an error
  and a reclaim through it deletes nothing
  (`an_overlay_on_either_side_cannot_prove_no_overlap`,
  `a_reclaim_through_an_overlay_with_a_protect_match_deletes_nothing`);
  an overlay (the model's `overlay_upper`: its upper directory shown
  under the overlay's own device) or a FUSE filesystem mounted inside a
  match, by path or through a bind, is an error for every candidate, and
  one inside no match changes nothing
  (`a_mount_storing_elsewhere_inside_a_match_cannot_prove_no_overlap`);
  through `ensure`, an overlay inside a protected or negated Cargo target
  whose upper directory holds the candidate deletes nothing, where it
  deleted the candidate inside the target before TASK-268
  (`vectors/config/overlay-mounted-inside-a-{protected,negated}-match`;
  on a real kernel overlay, the container test
  `protect_refuses_an_overlay_mounted_inside_a_protected_cargo_target`,
  whose oracle saw the deletion with the rule disabled); a
  negated profile's root on another filesystem is searched
  (`a_negated_match_on_another_filesystem_is_searched`); a covered mount
  still listed in the table cannot prove no overlap
  (`a_covered_mount_in_the_table_cannot_prove_no_overlap`);
- binds compared by place, not path text (TASK-267, TASK-285, TASK-290
  and their review): a root bound from inside container storage is
  refused (`vectors/walk/container-storage-behind-a-subdirectory-bind`,
  `container-storage-cargo-root-behind-a-subdirectory-bind`); a snapper
  layout (`/` showing a snapshot, `/.snapshots` unreadable) does not make
  roots look above the namespace's root or where a mount is attached
  (`tests/fs_dry_run.rs`
  `a_snapper_root_does_not_make_roots_climb_into_the_snapshots`), and a
  directory above a bound root that cannot be asked is named
  (`an_unaskable_directory_above_a_bound_roots_place_is_named`); a
  candidate, or a Cargo profile directory, holding a mount point only by
  place is `contains_mount`
  (`vectors/walk/{,cargo-}mount-point-inside-through-a-non-recursive-bind`,
  `cargo-profile-dir-is-a-mount-point-through-a-non-recursive-bind`,
  `cargo-triple-dir-is-a-mount-point-through-a-non-recursive-bind`, and
  under the locks
  `vectors/races/cargo-profile-dir-mounted-over-during-the-locked-walk`); a
  rename above a bind's source, a `CACHEDIR.TAG` above it, or a mount made
  after the enumeration (a bind of an overlay at a protected root) makes
  the check enumerate again
  (`vectors/config/protect-rename-above-a-bind-source`,
  `protect-cargo-tag-above-a-bind-source`,
  `negated-rename-above-a-bind-source-after-a-protect-refresh`,
  `protect-overlay-bound-at-a-protect-root-after-enumeration`), and one
  mount event costs one enumeration more, not one per candidate
  (`tests/fs_reclaim.rs`
  `a_mount_after_the_enumeration_costs_one_enumeration_more`). On a real
  kernel (rootless container, a non-recursive `MS_BIND` of a scratch
  directory; each fails with its check disabled):
  `a_non_recursive_bind_keeps_a_candidate_holding_a_mount_point`,
  `a_non_recursive_bind_keeps_a_cargo_profile_directory_that_is_a_mount_point`,
  `a_root_bound_from_inside_container_storage_is_refused`. Not on a real
  kernel: the snapper layout, btrfs subvolumes, binds of overlays.
- a `protect` match removed after the tier enumerated it, before a
  candidate's first overlap screen, does not stop the run: the screen
  enumerates again (TASK-269), from discovery, the union walk, the
  index's queue and for a Cargo target (`tests/fs_reclaim.rs`
  `a_protected_match_removed_before_the_first_screen_does_not_stop_the_run`,
  `tests/index_reclaim.rs`
  `a_protected_match_removed_before_the_first_screen_from_the_index_does_not_stop_the_run`,
  `tests/ensure.rs`
  `a_protected_match_removed_before_a_cargo_targets_first_screen_does_not_stop_the_run`);
  the same for a `!profile:` match, in `ensure` and the dry run, on the
  union walk and for a Cargo target
  (`a_negated_match_removed_before_the_first_screen_does_not_stop_the_{run,dry_run}`,
  `a_negated_match_removed_before_a_cargo_targets_first_screen_does_not_stop_the_{run,dry_run}`);
  a protected or negated Cargo target one of whose entries vanishes
  during its inspection is inspected again and still matches, and one that
  never settles stops the run
  (`vectors/config/{protect,negated}-cargo-target-child-vanishes-during-enumeration`,
  `vectors/config/protect-cargo-target-inspection-never-settles`);
  a valid `CACHEDIR.TAG` renamed under the candidate's lock into an
  ancestor that already held a profile directory makes it a protected or
  negated Cargo target, and the candidate inside is kept
  (`vectors/config/{protect,negated}-cargo-tag-made-after-enumeration`);
  a candidate on a loop-mounted image whose file a match holds is
  `protected` or `excluded`, and the reverse, and a loop mount inside a
  match protects its file; an unreadable or deleted backing file is an
  error, and a loop device in no match is never read (model:
  `loop_backing` on a mount of major 7; `tests/fs_dry_run.rs`
  `a_loop_image_overlaps_the_file_behind_it`, and for every loop member
  of a btrfs, an unreadable membership and a covered mount point
  `every_loop_member_of_a_btrfs_is_judged`,
  `vectors/config/{protect,negated}-loop-image-file`,
  `vectors/config/protect-loop-image-unreadable`; no container test: a
  rootless container cannot attach a loop device);
  and one removal costs one enumeration again, not one per later
  candidate (`tests/index_reclaim.rs`
  `one_removed_protect_match_costs_one_enumeration_not_one_per_candidate`);
- a stale index (deep modification, a new `.diskgc-keep`, a new Cargo
  target inside a `tmp-entry` candidate, a new protected match after the
  tally): the deletion-time checks refuse what they must, each candidate
  taken from the index's queue (diskgc-model `tests/index_reclaim.rs`:
  `stale_index_changes_after_the_tally_are_refused_at_deletion_time`,
  `a_seeded_index_that_missed_the_changes_is_refused_at_deletion_time`);
- new candidates of a stale tier created after the snapshot: they are
  deleted before any pressure tier runs (per-tier discovery;
  `new_cruft_after_the_snapshot_is_deleted_before_the_pressure_tier`);
- **index and collapse** ([architecture.md](architecture.md#the-index)),
  with the memory budget made small in the model:
  - `collapse_keeps_every_member_checked` (TASK-31: the index-authority
    property below runs every corrupted index, many collapsed by a small
    budget or to lumps, against a correct run and asserts equal verdicts;
    a kept member of a lump is refused in
    `a_lump_above_the_bound_root_is_expanded_from_the_root_only`): with a goal that needs every
    candidate, a tier whose candidates are all collapsed into blocks
    gives every candidate the same verdict as the same run with a large
    budget (oracle on the call trace), and every member
    deleted had its own identity check, eligibility walk and in-use
    check; a block member planted with a `.diskgc-keep`, an open fd or
    another owner is skipped with that reason;
  - `collapse_revalidates_members`: members removed or replaced after
    their tally are evicted by the collapse and contribute nothing to
    the block's `members` or `est_bytes`; a site directory replaced by
    another inode under the same name is not collapsed and its units
    are evicted;
  - `index_never_exceeds_its_budget`: after every insertion and
    collapse the accounted size is at most the budget (asserted in the
    engine, and checked here over 98,000 rc dirs with a budget that
    fits a tenth of them), and repeated collapse ends;
  - `no_insertion_is_refused_at_or_above_the_floor`: with a budget at or
    above one lump per mount with its finders plus the tables (two
    mounts, three profiles), no insertion is refused, the budget holds
    after every insertion and every collapse ends;
  - `a_crowded_plan_never_refuses_an_insertion_above_the_floor`
    (TASK-230): more name families beneath one site than a pass plans
    steps, each family step growing the index (long names over singles
    whose nodes stay): insertions at the ceiling still collapse, none is
    refused (the previous planner refused one: `NoRoom`);
  - `a_block_is_recounted_on_a_complete_listing_only`,
    `sync_tally_recounts_blocks_on_complete_listings_only`,
    `background_tally_recounts_blocks_on_complete_listings_only`: a
    listing stopped by its deadline or with an unreadable directory
    leaves a block's count; a complete one recounts it (also counting
    the candidates a fresh block covered without counting);
  - `discovery_dissolves_a_block_only_below_the_low_water_mark`: a
    block is replaced by its listed members only when they fit under
    the low-water mark;
  - `new_candidate_after_a_recent_snapshot_is_found_before_pressure`: a
    tier candidate created one minute after the snapshot (whose
    `discovered_at` lets pass 1 skip the root) is deleted before the
    pressure tier runs;
  - `stored_cargo_refusal_is_retallied_without_locks`: a kept Cargo
    target refused by an earlier tally is tallied again at reclaim and
    none of its lock files is opened for locking;
  - `ensure_skips_the_snapshot_when_it_would_cost_the_goal`: state
    directory on the reclaimed filesystem, available space just above
    `--min-free` after the walk: no write, one log line, exit code
    unchanged; `ENOSPC` on the write likewise;
  - `block_expansion_finds_only_what_discovery_would`: a block's site
    holding a non-matching name, a symlink, a mount and a name outside
    its family yields none of them as members; a lump sited above the
    tier's bound root is expanded from the root, never from above it
    (`a_lump_above_the_bound_root_is_expanded_from_the_root_only`);
  - `index_unit_not_matching_its_root_is_stale`: a unit whose path the
    tier's discovery would not find under its bound root (no longer
    matching the pattern, below another candidate, gone, or another
    object under its name) is skipped `stale` and never walked or locked;
    one whose finder names another profile, root or definition never
    enters the tier's queue (diskgc-model `tests/index_reclaim.rs`
    `a_unit_discovery_would_not_find_is_stale_and_never_walked`);
  - `stored_refusal_never_fakes_exhaustion`: a unit refused by an earlier
    tally (`kept`) whose marker is gone is deleted in the same run;
    a refusal found by the run's own synchronous tally skips it for the
    run (`a_stored_refusal_hint_only_ranks_the_unit_last`,
    `a_refusal_of_this_runs_tally_skips_the_candidate_for_the_run`);
- **synchronous tally** (virtual clock): `sync_tally_discovers_every_tier_before_sizing`
  (every selected tier has a queue before any tree is walked);
  `sync_tally_sizes_only_the_first_tiers_stale_units` (with an old seed
  and an unlimited budget, pass 2 walks only the first tier's units
  tallied more than 60 minutes ago or never, in its order; later
  tiers' stored sizes are kept and none of their trees is walked); `sync_tally_budget_ending_in_discovery_still_finds_the_rest`
  (the walk's per-tier discovery deletes what the tally did not list);
  the tally takes no host lock and makes no deleting call (diskgc-model
  `tests/tally.rs`: `sync_tally_discovers_every_tier_then_sizes_only_the_first`,
  `sync_tally_skips_a_recent_listing_and_sizes_only_stale_units`,
  `sync_tally_sizes_the_first_tier_that_found_something`,
  `sync_tally_budget_ends_in_either_pass`, all on `best-fit` tiers);
  under `oldest` pass 2 walks no tree and the reclaim walk sizes the
  first tier's stale units as its queue reaches them (TASK-251;
  `sync_tally_leaves_an_oldest_first_tiers_sizing_to_the_walk`, and in
  `tests/index_reclaim.rs`: the reclaim takes the dry run's order where
  deep changes reorder the units, also beside an expanded block's
  members, sizes only the units its queue reaches, stops sizing at
  `--tally-budget` with `tally_cut_short` (the mount table read
  charged), reads a root again where an earlier run stored the key
  (a snapshot's tally or listing: Codex review), keeps the deferral
  when preflight drops a root and says when no tier took it, and a
  failure of the host while sizing is an error of the run, also after
  a first batch was deleted: `deferred_sizing_*`,
  `the_deferred_sizings_mount_table_read_is_charged_to_the_tally_budget`,
  `an_unused_deferral_is_logged`,
  `a_host_error_in_the_walks_deferred_sizing_is_an_error_of_the_run`,
  `a_host_error_in_deferred_sizing_after_the_first_batch_stops_the_run`;
  and the queue's merge itself, a property in `diskgc-core`
  `executor::indexed::tier_queue`: with keys at most the sized keys,
  ties and members, it gives the order of full sizing);
- **background tally** (virtual clock, every port call costing 1 ms):
  the worker's active time is at
  most 5 % (plus one batch) of any window of 100 batches, no batch
  exceeds 50 ms or 2,000 operations (plus the reach of one unit or root
  and, after a stalled step, 32 operations; listings included: they are
  resumable rounds), and it makes no filesystem call
  while the host lock is held by another process; overdue discovery,
  re-check and re-tally jobs are all served within twice their period
  under constant churn (no starvation; the model test is scaled down to
  about 200 units and 600 entries over 150 minutes; the 100,000-entry
  run is open); a sizing cursor holds
  no fd between batches and survives a directory vanishing under it;
  a listing round cut into steps of any size finds exactly what one
  uninterrupted discovery finds (both kinds, property test), a 3,000-entry
  root is listed across many batches within their bounds, entries
  changing under a round keep it sound and a replaced directory makes it
  partial, and a round is complete only if every step was clean
  (diskgc-model `tests/tally.rs`);
- **pressure brake** (virtual clock, scripted PSI readings through the
  `Host` query): `pressure_brake_doubles_the_sleep_and_skips_batches`
  (with `io` at 30 %, no batch runs and the sleeps are 1, 2, 4 ... s up
  to 60 s; one reading below every threshold runs a batch and returns
  to 19 × *t*); each of `io`, `memory` (10 %) and `cpu` (50 %) alone
  triggers it, and readings at the thresholds do not;
  `no_psi_means_duty_cycle_only` (PSI unreadable: one log line, the
  plain duty cycle); the synchronous tally never reads PSI (ModelHost's
  `pressure` script);
- **snapshot** (ModelHost state operations; diskgc-model
  `tests/snapshot.rs`): `ensure_writes_the_snapshot_and_the_next_run_starts_from_it`
  (mode 0600 under a 0700 state directory; the second run starts from
  the snapshot, its tally lists no root discovered less than 5 minutes
  earlier and both tiers take queues from it);
  `a_second_writer_does_not_write` (another process holds the
  single-writer lock: one log line, no write; the watch side,
  `watch_waits_for_an_ensure_writing_then_starts` and
  `second_watch_exits_2`, is TASK-32);
  `a_corrupt_mismatched_or_foreign_snapshot_is_ignored_and_tallied_from_nothing`
  (a flipped byte, truncated, another format version, another user's,
  readable by others, a symlink, oversized, another boot for a
  filesystem without UUID, another `<fs>` key: each ignored with a log
  line naming why, the tally lists every root, a valid snapshot replaces
  it); `a_snapshot_naming_a_relative_or_outside_path_is_ignored_not_trusted`
  (valid checksum, a path the index would assert on: ignored, never
  reaching the index); `an_edited_profile_drops_only_its_own_units`;
  `a_run_of_other_tiers_keeps_the_units_it_does_not_use`;
  `no_index_neither_reads_nor_writes_the_snapshot` (no state call);
  `an_ensure_whose_goal_is_met_makes_no_state_call` (no tally, no state
  directory, no lock file);
  `the_write_never_takes_the_reclaimed_filesystem_below_min_free`;
  `a_state_write_without_the_exclusive_lock_fails_the_test` (the port's
  shape); `a_snapshot_with_1_kib_of_headroom_is_not_written_and_exit_0_holds`
  (the write's allocation is guarded, and it comes before the final
  sample); `a_signal_during_the_snapshot_write_is_the_runs_exit_code`
  (143); and the property
  `no_snapshot_content_panics_the_seed_or_a_later_collapse`
  (`tests/snapshot_props.rs`: counters near `u64::MAX` and paths up to
  300 components through decode, validity, restore, statistics and a
  collapse to one lump, then the writer: it writes what the reader
  loads or skips the write; its found seeds are kept in
  `tests/snapshot_props.proptest-regressions` and rerun first;
  `a_path_far_deeper_than_the_bound_is_refused_before_the_index`
  for 60,000 components; `a_collapse_past_the_reader_cap_skips_the_write`
  and diskgc-core's
  `the_writer_refuses_exactly_what_the_reader_refuses_for_a_limit`,
  TASK-279). Still open: `source: snapshot` / `source: sync` in the report
  (TASK-244), and comparing an ignored snapshot's run with a
  `--no-index` run's deletions (the scenarios delete nothing in either
  run);
- **watch** ([reference.md](reference.md#watch); virtual clock, a
  scripted `other_used` curve for available space, scripted PSI, signal
  events; each scenario asserts the deletion oracle, the JSON lines
  parsed line by line, and the `Delete` and host-lock calls on the
  trace). In diskgc-model `tests/watch.rs` (TASK-32), with the
  background tally inline on the model's one thread. Each entry names the
  test that covers it, or says *open*:
  - `watch_steady_between_thresholds_deletes_nothing_and_keeps_its_duty_cycle`
    (the PRD's acceptance test): available space held between
    `--min-free` and `--target-free` for 24 virtual hours, with
    candidates created and aged all along so that many become eligible:
    no `Delete` call, no exclusive host-lock `flock` and no lock held
    across a batch (the pause probe is `LOCK_SH | LOCK_NB`, closed at
    once, pinned on the trace), no stdout byte, one sampling `statvfs`
    per interval (within one; the snapshot guard's samples are counted
    apart), the tally's active time at most 5 % (plus
    one batch) of every 100-batch window, and a snapshot write every
    5 minutes (within one). As written; the duty cycle is bounded by all
    port calls staying under 5 % of the 24 hours, the per-window bound is
    the background tally's own test;
  - `watch_reclaims_below_min_free_to_target_free`: one drop below
    `--min-free` gives exactly one cycle and one record (`met`,
    `target_met: true`, `exit_code: null`, `watch.cycle: 1`), and
    deletion stops at the goal (the record's `index` block is `null`
    until TASK-244). As written;
  - `watch_hold_back_off_waits_for_a_drop_or_15_minutes` (as
    `watch_hold_back_off_waits_for_15_minutes` and
    `watch_hold_back_off_ends_on_a_drop_by_the_step`; a full filesystem,
    whose drop by the step cannot happen:
    `a_full_filesystem_backs_off_15_minutes_not_every_interval`): an `exhausted`
    cycle; samples continue every interval; no cycle before space falls
    by the step or 15 minutes pass, whichever is first (both orders);
    one back-off log line; `resume_below_bytes` and `wait_seconds` in the
    record match when the next cycle starts;
  - `watch_retry_back_off_doubles_and_caps` (*open* with a Docker tier,
    which does not report `backend_unavailable` yet (TASK-113; the Nix
    kind does); the doubling and cap as
    `watch_path_on_another_device_backs_off_retry_doubling_and_deletes_nothing`): a Docker tier
    `backend_unavailable` at every cycle: cycles 1, 2, 4, 8, 15, 15
    minutes apart; the daemon back: the next cycle reclaims it and the
    doubling resets;
  - `watch_mixed_outcome_takes_the_shortest_wait` (the class: a unit test
    in `watch::backoff`; *open* as a scenario): `exhausted` with one
    `backend_unavailable` skip is class `retry`; a drop by the step
    before its timer still starts a cycle at once;
  - `watch_recovery_ends_back_off` (as written): space freed by someone else during a
    back-off: no cycle, one log line; a later drop below `--min-free`
    starts a cycle at that sample;
  - `watch_cycle_beside_ensure` (the timeout as
    `watch_cycle_beside_another_reclaim_times_out_and_retries`; the
    holder freeing enough: *open*): another process holds the host lock past
    `--timeout`: a `lock_timeout` record, nothing deleted, class `retry`;
    the holder frees enough before the timeout: `met` with no deletion;
  - `watch_failed_samples_back_off` (as written; a failing first sample:
    `a_failing_first_sample_is_the_first_cycle_and_the_goal_resolves_later`): `statvfs` failing for an hour gives
    `error` records (`cause: host`) at 0, 1, 3, 7, 15, 30, 45, 60
    minutes, not one per interval, and the process keeps running;
  - `watch_cycle_deadline` (as
    `watch_cycle_deadline_starts_no_deletion_after_it`, TASK-258; the
    model runs no child, so a running child's SIGTERM at the deadline is
    the container test `deadline_mid_reclaim_reports_one_json_object`,
    `ensure`'s run of the same executor; `watch`'s own *open*): a tier
    slower than `--timeout`: a `deadline` record, no deletion starts
    after the deadline, a running child gets SIGTERM;
  - `watch_path_on_another_device_deletes_nothing` (as
    `watch_path_on_another_device_backs_off_retry_doubling_and_deletes_nothing`):
    a mount stacked on `PATH` after start: the sample still reads the
    original filesystem; the cycle's own resolution of `PATH` (the one it
    measures and reclaims on) finds another device and records a `host`
    error;
  - `watch_start_refusals` (the gate, the host lock file, a target below
    the minimum: `watch_start_refusals`; the usage errors: the binary's
    `watch_usage_errors_say_why`; an invalid configuration exit 2 before
    the host starts, which would fail with exit 3: the binary's
    `watch_checks_the_configuration_before_the_host_starts`, and a valid
    one the host fails to read (`/proc` unavailable) exit 3 `host`:
    `watch_with_a_valid_config_and_no_proc_is_exit_3_host`,
    `ensure_with_a_valid_config_and_no_proc_is_exit_3_host` and
    diskgc-linux's `a_config_read_that_fails_on_proc_is_the_hosts`
    (`ensure_checks_the_configuration_before_the_host_starts` for
    `ensure`'s invalid configuration); the refusal after start and its JSON object: the binary's
    `a_late_refusal_prints_the_usage_error_object_and_exits_2`;
    `second_watch_exits_2` and
    `a_signal_during_the_start_wait_exits_0_writing_nothing`; the rest
    *open*; one case per row of the start sequence):
    usage (no `--target-free`; `--interval 0`; `--tally-only` with
    `--target-free`), invalid configuration, root without `--allow-root`,
    `--allow-root` without `CAP_SYS_PTRACE` in either mode
    (`watch_allow_root_without_cap_sys_ptrace_is_refused_at_start_in_both_modes`;
    `--allow-root` not as root, an unreadable capability set (exit 3);
    `ensure`'s are the `allow-root-*` vectors and
    `allow_root_needs_root_and_cap_sys_ptrace_in_the_effective_set`; in a container
    `allow_root_without_cap_sys_ptrace_is_refused_in_every_mode`, root with
    every container capability but that one), a missing supplied root, a lock its cycles cannot take
    (a refused per-user lock: `watch_start_refusals`; root without the
    host lock: `watch_as_root_without_the_host_lock_exits_2_at_start`), a second `watch`
    with the same state directory after its 10 s wait (`second_watch_exits_2`,
    above), `--timeout 0`: the documented exit code, one
    JSON object with that `exit_code` under `--json`, no `Delete` call;
    the same lock problem under `--tally-only` is one warning and a
    running tally; a user's watch without the host lock is no refusal: it
    reclaims under its per-user lock
    (`a_user_watch_without_the_host_lock_reclaims_under_its_per_user_lock`;
    TASK-277, the rest in `crates/diskgc-model/tests/user_lock.rs`, the
    `per-user-lock-*` exit vectors and the container tests
    `ensure_as_a_user_without_the_host_lock_reclaims_under_a_per_user_lock`,
    `ensure_as_a_user_refuses_another_users_per_user_lock` (another
    user's directory and file, which only a container can make) and
    `ensure_as_a_user_reclaims_unlocked_when_the_per_user_lock_cannot_be_made`); an unwritable state directory and a failing first
    sample are not refusals: one log line and no snapshot, and a first
    cycle record (`error`, `host`) respectively, the process running;
  - `watch_root_vanished_after_start_still_reclaims_other_tiers` (as
    written, TASK-258): a
    tier's supplied root removed after start: that tier is one `missing`
    skip row, the other tiers delete to the goal, status `met`;
  - `watch_signals` (SIGTERM in a cycle:
    `watch_sigterm_in_a_cycle_writes_an_interrupted_record_and_stops`;
    between samples: every scenario's end; a second signal during the
    stop's snapshot write and before it:
    `a_second_signal_during_the_stops_snapshot_write_leaves_the_old_one`,
    `a_second_signal_before_the_stops_write_skips_it` (the port's
    cancellation at each step on the real kernel: diskgc-linux's
    `a_cancelled_state_write_leaves_the_old_snapshot_and_names_nothing`);
    during a child's
    grace, which the model does not run, the container test
    `watch_second_signal_leaves_a_stubborn_child_holding_the_host_lock`;
    TASK-258: SIGINT in a cycle as
    `watch_sigint_in_a_cycle_writes_an_interrupted_record_and_stops`, a
    signal during the host-lock wait as
    `watch_sigterm_during_the_host_lock_wait_stops_at_once`, during a
    back-off: the end of every back-off scenario; SIGHUP is not modelled
    (the model delivers SIGINT and SIGTERM only; SIGHUP differs from them
    only in `LinuxHost`'s handler and the stop's log line), so it is
    *open* until the container test `watch_process_stops_on_each_signal`;
    the rest *open*): SIGTERM between samples, during a back-off, during
    the host-lock wait and in a cycle between two candidates: exit 0
    within one batch (50 ms) or at the next candidate boundary, a record
    with status `interrupted` for the cycle and the host-lock wait (part
    of a cycle) only, no candidate
    started after the signal, the snapshot written once; the same for
    SIGINT and SIGHUP; a second signal during the snapshot write: exit 0
    and the old snapshot unchanged; a signal during the start wait: exit
    0, nothing written;
  - `watch_tally_only_never_reclaims` (as
    `watch_tally_only_never_reclaims_and_keeps_the_snapshot_fresh` and
    `watch_tally_only_min_free_guards_the_write_on_the_watched_filesystem`): available space far below any
    threshold for an hour: no `Delete` call, no exclusive host-lock
    `flock` (only the pause probe), no
    stdout byte, snapshots every 5 minutes; with a reclaim of another
    process holding the host lock, no tally batch runs until it is
    released; `--min-free` given and the state directory on the watched
    filesystem: the write is skipped when it would take space below it;
  - from the TASK-32 review: `a_failed_background_listing_keeps_the_next_cycle_off_the_index`
    (a background listing that failed with a failure of the host keeps
    every tier off the live index until a complete listing),
    `a_random_group_is_shuffled_again_for_each_cycle`,
    `an_owner_name_gone_since_start_refuses_the_cycle`,
    `a_failed_record_write_ends_watch_at_once`; on the real kernel
    (diskgc-linux `tests/tally_thread.rs`): the tally thread at nice 19 and
    the caller not, a panic in the watcher's loop stopping the tally
    thread, and a tally turn that does not end never holding the watcher
    past its 1 s bound;
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
- `command_action_stalled_stderr_neither_fails_nor_hangs` (TASK-342): an
  unread full blocking parent stderr pipe while a child writes 5,000
  lines: the child runs to its end (its done file written), the result is
  its exit 0, the port returns within 5 s, every line is counted as not
  forwarded, and the pipe's flags stay blocking.
- `command_action_on_a_slow_stderr_completes_and_counts_unforwarded_output`
  (TASK-342): the CLI with its stderr a pipe read 4 KiB every 250 ms
  while a command action writes 20,000 lines: the action completes
  error-free (candidate gone, done file written, exit 0 or 1 by the
  space, never 3), and the forwarded lines plus the count on the action's line
  add up to the lines written (at most one more).
- The forwarder itself (diskgc-linux `command_output` unit tests, with a
  sink the test blocks or fails): queueing never waits for a stalled
  sink, the queue stays within its bound and drops its oldest segments,
  a failed write counts against its own action only, the end of an
  action withdraws and counts what is left, and nothing is lost when the
  sink keeps up.
- `command_action_leader_exit_drain_obeys_tier_deadline`: a descendant holds
  output open after leader exit, but draining ends at the tier deadline.
- `command_action_closed_pipe_descendant_is_killed`: a descendant closes both
  output pipes and still gets killed before the successful leader is reaped.

**Survey** (`crates/diskgc-model/tests/survey.rs`; TASK-38): the
composition the binary runs (`diskgc_core::survey::load` and `run`) on a
laptop-shaped `/tmp` model (with `--in-use` unless a test says
otherwise), each test also holding the run to its
call trace (no `unlinkat`, `fchmod`, command, `flock`, lock inheritance,
per-user lock, Nix root or GC call, no state read or written):

- `the_laptop_tmp_is_categorised_by_profile_and_name_family`: every
  built-in applied; the pytest sessions inside a `nix-shell.*` dir and
  the rc dir are their profiles' candidates (one `in_use`), a profile
  that found nothing keeps its block, the unmatched entries are exactly
  the `tmp.*` and `claude-#` families with their sizes, ages and
  largest-first examples, the `nix-shell.*` dir holding candidates is no
  family, and `systemd-private-*` is never statted; the human text has no
  `would reclaim`.
- `the_candidates_are_listed_up_to_1000_per_profile_and_the_rest_counted`:
  a `--select` union of two user profiles is one tier with a block each;
  1,003 candidates list 1,000 (largest first) and count 3 as `left_out`,
  in the JSON and in `--list`.
- `each_path_reports_only_what_lies_below_it`: two `PATH`s; `/tmp`'s
  fixed-root profiles do not run under `/home/tester`, a `roots:
  required` one does, and each `PATH` has its own unmatched entries.
- `the_json_has_exactly_the_documented_fields`: the JSON's keys equal the
  reference's `Survey JSON` example, recursively.
- `a_missing_supplied_root_refuses_the_survey`: exit 2 before anything is
  planned, with `paths: []`.
- `one_profiles_error_stops_no_other_profile`: a user profile whose root
  is a symlink is an error of its own (exit 3); the built-ins after it
  are still planned.
- `a_path_that_is_a_candidate_has_no_unmatched_entries`: `survey` of a
  Cargo target itself lists no entry below it as unmatched.
- `the_deadline_stops_the_survey_with_what_it_has`: a clock event past
  `--timeout` during the first listing: exit 124, each profile not
  reached an error, every unmatched entry unsized.
- `a_kept_candidate_has_no_idle_age`: a `protected` row has no age, and
  the profile's age range is its eligible rows' alone.
- `a_selected_profile_rooted_elsewhere_is_not_surveyed`: `--select` of a
  `/tmp` profile under another `PATH` is `not_surveyed`
  (`roots_elsewhere`), not "found nothing".
- `a_mount_point_below_path_is_not_an_unmatched_entry`: `survey /` does
  not list the `/tmp` mount point.
- `an_error_before_the_deadline_stays_the_status`: a profile's error then
  the deadline: `error` (exit 3), not `deadline`.
- `a_live_session_is_in_use_with_the_flag_and_not_checked_without`: the
  pytest session a process holds open is kept `in_use` with `--in-use`;
  without it it is not checked (`eligible: null`, the block's
  `not_checked`, `eligible: null`), the human text never says
  "eligible", and no process is read (none of the `Host` calls that read
  another process: `/proc`, sockets, loop devices).
- `a_cargo_target_in_use_is_not_checked_without_the_flag`: the Cargo
  process heuristics are gated the same way.
- `static_refusals_stand_without_the_flag`: `protected` and `future_time`
  still refuse without `--in-use`.
- `a_project_contracts_file_rules_stand_without_the_flag`: python-venv's
  project contract still keeps a venv whose project changed yesterday
  `not_idle` without `--in-use` (its file-based rule), and leaves the
  idle project's not checked.
- `a_unions_contract_file_rules_veto_without_the_flag`: the same busy
  venv in a union of python-venv and a venv profile without a contract,
  in both orders, is kept `not_idle` under python-venv with and without
  `--in-use`: the union's contract veto judges the rules on files without
  a scan. The idle project's venv is not checked. Fails when the veto
  returns early without a scan (TASK-343).
- `pytests_file_rules_stand_without_the_flag`: old pytest sessions whose
  `.lock` is no pid marker (row 3), and one in a `pytest-of` directory of
  another uid (a check before the table), are `producer_unknown` with and
  without `--in-use`, alone and vetoed in a union whose other term was
  selected; a session whose `.lock` holds a pid (Owner PID, processes) is
  `in_use` with `--in-use` and not checked without. Fails when pytest's
  contract is judged only with a scan (TASK-343).
- `a_unions_toolchain_place_rule_vetoes_without_the_flag`: a custom
  toolchain (no name rustup installs again) in a union of a
  `protect_toolchains` term and a guard-free term, in both orders, is
  kept `pinned_toolchain` with and without `--in-use` (the guard's static
  half is a retention guard the union applies without a scan); a
  reinstallable one nothing pins is not checked without `--in-use`. It
  passed before TASK-343 too: it pins the documented split of the guard.
- `ensure_dry_run_always_checks_in_use`: on the same fixture `ensure
  --dry-run` scans, keeps the live session `in_use`, and its JSON has no
  survey field. That no `ensure`, dry run or reclaim can plan without the
  check is structural: the option holds a value only `survey` can make,
  and a `compile_fail` doc test on `diskgc_core::survey::InUseNotChecked`
  shows nothing outside it can.

The first three fail when the plan's checker scans regardless
(`PlanScan::NotChecked` ignored), the first two also when unchecked rows
are labelled eligible; the doc test fails when the token's field is
public.
- `a_profile_whose_listing_fails_is_not_reported_as_finding_nothing`: EIO
  on the first listing of `/tmp` fails that profile: no summary block, not
  in "found nothing", its host error present, the human text says it
  could not be surveyed, and every other profile's block equals the
  run without the failure.

The first test also checks that no backend is queried without
`--select` (they are listed in `not_surveyed`).

`just docs-vs-binary BIN` runs `survey` on the host over its scratch Cargo
targets and compares the JSON with the same example.

### Producer liveness vectors

Status: the Owner PID (`nix-shell`) and `pytest` vectors and container
tests below are checked in and run (TASK-112); the project-contract
ModelHost vectors (`node-project`, `python-project`) are checked in and
run (TASK-79, 2026-11-02), except the rows marked *planned* in their
table, and their container tests are planned (below).
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
The toolchain profiles (`rustup-toolchains`, `elan-toolchains`, TASK-94;
pending Mark's admission) follow the same design: one vector per rule of
safety.md (Toolchain pins) in `builtins/` and `liveness/`, each holding
exactly one toolchain that rule alone keeps beside one it does not, so
that disabling the rule turns that vector red; the fail-closed rules each
have a vector of their own. The name grammars are pure and tested beside
the guard with properties. Container tests with real rustup and elan wait
for Mark's admission.

The TASK-40 language and package caches (`huggingface-hub-repos`,
`cargo-registry-src`, `node-gyp-headers`, `electron-download-cache`;
pending Mark's admission) have their builtins vectors (match, in use,
negated, keep marker, the one-hour floor, and for the two with a
completion marker a directory without it), their residual vectors in
`liveness/` (among them a delete stopped by the deadline, which leaves
`.cargo-ok` or `installVersion` behind), the frozen definitions in `profile/golden.rs`
(`language_cache_sources_have_frozen_definitions_and_match_sets`) and a
ModelHost match set with the tools' sibling entries as negatives
(`language_cache_builtins_have_golden_match_sets` in `fs_dry_run.rs`).
Container tests with the real tools wait for Mark's admission.

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
shapes reclaimed, `in_use` by an fd, `protected`, `keep_marker` by
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
allow`, `in_use` by an fd, `protected`, `keep_marker`, `pytest-05` `filtered`, a
test directory and sessions outside the temp-prefix preset not matched).
The project vectors are in `vectors/liveness/` (`node-project-*`,
`python-project-*`), one file per row of the project table below, plus
`vectors/builtins/node-modules.yaml` and `python-venv.yaml` for the
built-ins' roles (an idle project's dependencies removed, `in_use` by an
fd, `excluded` by a negated user profile, `keep_marker`). Their mutation
record (TASK-79, each vector-corpus run red on the named vectors): rule 2
off (the three `read-only-use-kept` and `source-read-kept`); the
`noatime` rule ignored (both `noatime-residual` vectors); held-directory
climbs off (`cwd-in-vcs-metadata-kept`, `chrooted-process-kept`); the
held-file path test off (`fd-in-sibling-dependency-dir-kept`, before the
review round walked dependency directories); the
project walk's changes ignored (`recent-edit-kept`, the workspace,
version-control and vendored vectors); the project climb off (the
workspace, pnpm-workspace, version-control and vendored vectors); the
version-control marker off; the source's own instance check off (the
reclaim then deleted the `filtered` candidates of `no-manifest`,
`manifest-variants`, `no-pyvenv-cfg`, `inside-venv` and the sibling
`node_modules`). The review round (TASK-79, 2026-11-02) wrote its vectors
red first (12 of 415 failed before the fixes; `npm-link`,
`editable-install` and `new-directory` guard the new rules and passed
before them) and bit: the outer-`node_modules` check off
(`shadowing-root`, `outer-node-modules-above-root`,
`member-read-keeps-root`); the other-filesystem retention off
(`bind-from-other-filesystem`); directory mtimes counted again
(`after-root-reclaimed`); the command-line test off (the three `argv`
vectors); the narrowing off (`ignore-sweep`, `source-read-only`,
`interpreter-symlink`); the link-out fallback off (`npm-link`,
`editable-install`); the interpreter-link exemption off
(`interpreter-symlink`); the `.pth` check off (`pth-path-outside`); the
model's read_head moving every file's atime (`pth-read-leaves-atime`);
the above-root answer off
(`marker-above-root`); the held-file path test off (`fd-in-vcs-metadata`,
`bind-from-other-filesystem`); climbs off (`cwd-in-vcs-metadata`).
Mutation record (step 3, each vector-corpus run red):
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
`hidepid` vectors; since TASK-263 refused runs, so the core's unit
tests carry that bite now), skipping the namespace (the nested vector), naming the
namespace before `hidepid` (`a_failed_precondition_names_its_rule_hidepid_first`),
tabling only the first level and ignoring a listed carrier
(`nix-shell-rc-namespace-local-pid-refused`), skipping the filesystem,
letting a live probe permit and ignoring unreadable `NSpid` (their vectors
and the Owner PID property) all bite. Through the real binary: dropping the contract
fails `nix_shell_rc_shell_in_private_pid_namespace_is_kept` (deletion
oracle), skipping the namespace fails
`owner_pid_candidates_are_kept_when_diskgc_has_its_own_pid_namespace`
(deletion oracle), naming the namespace first fails
`owner_pid_is_unknown_under_hidepid` (its detail; since TASK-263
`ensure_is_refused_under_hidepid`, whose runs are refused);
skipping the
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

- top level `pid_namespace: initial | nested | test_container` (default
  `initial`): the answer of diskgc's held `/proc` to the
  initial-namespace check, and for `test_container` the in-use check's
  test-only exception (TASK-263);
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
- a bind of a subdirectory: landed as a mount's `bind_root` (TASK-260,
  read by the `protect` and `!profile:` verdict), e.g. `bind_of: 2,
  bind_root: /w/packages/a`;
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
| `nix-shell-rc-nested-namespace-unknown` | `pid_namespace: test_container` (anywhere else a reclaim below the goal is refused); no process 4242 | `producer_unknown` |
| `nix-shell-rc-hidepid-refused` | `proc_hidepid: true`; no process 4242 | refused below the goal, dry run too (exit 2, `safety`, TASK-263): a hidden `newgrp` shell would be unlisted |
| `nix-shell-rc-hidepid-root-refused` | as above with `own_uid: 0` | the same (root does not lift the rule) |
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
| `pytest-tmp-nested-namespace` | `pid_namespace: test_container`; B `{0..4}`, `pytest-0/.lock` holds `42` | 0 `producer_unknown` (row 5); 1 eligible (row 7: a pytest counts as running, the session is old); 2, 3, 4 `producer_unknown` (row 8) |
| `pytest-tmp-hidepid-refused` | `proc_hidepid: true`; otherwise as above | refused below the goal, dry run too (exit 2, `safety`, TASK-263) |
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
project is the candidate's parent unless a row says otherwise; idle bound
30 days; TASK-79's container probe, where reading the dependencies changed
neither mtime nor ctime, is the counterexample these turn into cases; in
the model an edited file keeps its old atime, so rule 1 is pinned without
rule 2). Rows marked *planned* need host-model features not built yet
(spoofed link text, a process's own mount table and namespace, detached
mounts, `map_files`, overlay lower layers); the translation through a
process's own mount table is pinned by unit tests instead
(`fs_kind::project::tests`: a container's executable outside the tree, a
bind at `/work`, a `chroot` inside it, a path with a space, and the
untranslatable cases), and a mapping is matched by identity only
(safety.md, A process's paths):

| Vector | Setup | Expected |
|---|---|---|
| `node-project-idle-eligible` | `/home/tester/p/{package.json,src/a.js,node_modules/x/i.js}`, all changed and read 60 days before the clock; a uid 1000 shell holding `/home/tester/.bash_history` (same filesystem, outside the project) and nothing in `p` | eligible, removed; `package.json` and `src` preserved |
| `node-project-recent-edit-kept` | as above, `src/a.js` changed 2 days ago | `not_idle` |
| `node-project-recent-install-kept` | only `node_modules/x/i.js` changed 2 days ago | `not_idle` (the candidate's own walk counts) |
| `node-project-install-old-mtimes-new-ctime-kept` | every mtime 60 days old (the normalised dates an install extracts); `node_modules/x/i.js` with `ctime` 2 days ago | `not_idle` |
| `node-project-dir-atime-only-eligible` | all old; only directory atimes in the project recent (a lister) | eligible: directory atimes do not count |
| `node-project-nested-node-modules-filtered` | `p/node_modules/a/{package.json,node_modules/b/i.js}` as well as `p/node_modules`, roots `p` and `p/node_modules/a` (discovery never descends into a candidate, so only a root inside one finds a nested one); the project active | the nested one `filtered`, never deleted, also when the outer one is kept |
| `node-project-workspace-member-root-active-kept` | `w/{package.json (with workspaces),src/x.js changed yesterday,packages/a/{package.json,node_modules}}`, `packages/a` itself 60 days old | `w/packages/a/node_modules` `not_idle`: judged by the workspace |
| `node-project-vcs-top-level-is-the-project` | as above, `w` without `workspaces` but holding `.git`; `w/src` changed yesterday | `not_idle` |
| `node-project-read-only-use-kept` | TASK-79's case: nothing changed for 60 days, `node_modules/x/i.js` read (`atime`) yesterday | `not_idle` |
| `node-project-read-only-use-noatime-residual-eligible` | the same on a `noatime` mount | eligible: the stated residual, pinned |
| `node-project-cwd-in-project-kept` | all old; a process with cwd `/home/tester/p/src` | `in_use`, `cwd` |
| `node-project-fd-in-project-kept` | all old; a process with `src/a.js` open | `in_use`, `fd` |
| `node-project-environ-names-project-kept` | all old; a process with `NODE_PATH=/home/tester/p/lib` (in the project, outside the candidate) | `in_use`, `environment` |
| `node-project-vcs-only-change-eligible` | all old except `.git/FETCH_HEAD` changed yesterday | eligible |
| `node-project-cwd-in-vcs-metadata-kept` | all old; a process with cwd `/home/tester/p/.git` | `in_use`, `cwd`: inside by path, though the walk leaves `.git` out |
| `node-project-fd-in-sibling-dependency-dir-kept` | all old; `p/other/node_modules/y.js` open in a process | `in_use`, `fd` (by identity: the project walk walks every dependency directory of the candidate's name); `p/other/node_modules` itself `filtered` (no `package.json` beside it) |
| `node-project-keep-marker-kept` | all old; `p/node_modules/.diskgc-keep` | `keep_marker`: the operator's remedy for hand-edited dependencies |
| `node-project-symlinked-node-modules-not-candidate` | `p/node_modules` a symlink to `/home/tester/shared/node_modules` | no candidate; the target preserved |
| `node-project-pnpm-layout-eligible` | all old; `p/node_modules/{.pnpm/x@1/node_modules/x/i.js (link: to /home/tester/.local/share/pnpm/store/v3/files/ab/cd),x (symlink to .pnpm/x@1/node_modules/x)}` | eligible; the store file preserved with one link fewer |
| `node-project-bind-mounted-member-unknown` | the workspace member `w/packages/a` also bind-mounted at `/home/tester/a` (`bind_root: /tester/w/packages/a`, the `mountinfo` root) and supplied as the root; `w/src` changed yesterday | `/home/tester/a/node_modules` `producer_unknown`: the climb reached a bind-mounted subdirectory |
| `node-project-held-file-other-device-not-inside` (*planned*) | all old; a process holds `/home/tester/elsewhere/z` on another filesystem; its link text spoofed in the model to `/home/tester/p/z` | eligible: a device not in the tree is never inside, and nothing outside the tree is resolved |
| `node-project-held-file-same-device-outside-eligible` (in `node-project-idle-eligible`: its shell holds `.bash_history`) | all old; a process holds `/home/tester/notes.txt` on the project's device | eligible: text outside the tree's path is not inside |
| `node-project-container-bind-work-kept` (*planned*) | all old; a process with `mnt_ns: c1`, `root: /`, its mount table binding `/home/tester/p` at `/work` (mount id 968, root field `/home/tester/p`), holding `/work/src/a.js` (link text `/work/src/a.js`) | `in_use`, `fd`: translated through its mount table |
| `node-project-container-chroot-bind-kept` (*planned*) | as above plus a `chroot` to `/jail` (root link text `/jail`, link text `/jail/work/src/a.js`, mount point `/work`) | `in_use`, `fd` |
| `node-project-container-chroot-outside-eligible` (*planned*) | all old; the chroot process of `node-project-container-chroot-bind-kept` instead holds `/jail/notes/n.txt` (link text `/jail/notes/n.txt`), its mount table binding `/home/tester/notes` at `/notes` on the project's device | eligible: translated and outside the project |
| `node-project-space-in-path-container-kept` (*planned*) | the project at `/home/tester/my proj`, all old; a container process binding it at `/work dir` (its `mountinfo` holds `/home/tester/my\040proj` and `/work\040dir`), holding `/work dir/src/a.js` | `in_use`, `fd`: mount points and root fields are unescaped first |
| `node-project-detached-mount-retained` (*planned*) | all old; a process in diskgc's namespace holds a file on the project's device through a lazily detached mount whose id is absent from `mountinfo` | `in_use` (untranslatable in the same namespace) |
| `node-project-map-files-unreadable-retained` (*planned*) | all old; a mapping of a project file on the project's device whose `map_files` link cannot be read and whose `maps` pair the walk did not see | `in_use` (untranslatable) |
| `node-project-untranslatable-retained` (*planned*) | all old; a process in another mount namespace holding a file on the project's device whose mount id is missing from its `mountinfo` | `in_use` (cannot be translated) |
| `node-project-held-file-suffix-mismatch-retained` (*planned*) | all old; a held file on the project's device whose link text has the project prefix but whose suffix resolves to another inode (renamed after open) | `in_use` (unresolvable on a tree device retains) |
| `node-project-subdir-bind-held-cwd-kept` | all old; `p/src` also bind-mounted at `/mnt/src` (`bind_root`); a process with cwd `/mnt/src` | `in_use`, `cwd`: the climb meets a directory of the tree |
| `node-project-pivot-root-process-not-under` | all old; a process whose `root` is `/srv/c` and cwd `/srv/c/work`, outside the project | eligible: its climb reaches the fixpoint without meeting the tree |
| `node-project-ssh-agent-excused` | all old; the user's `non_dumpable` `ssh-agent` whose magic links cannot be opened | eligible: excused as In use excuses it |
| `node-project-overlay-mapping-retained` (*planned*) | all old; a mapping whose `maps` device and inode are a lower layer's, of a file in the project | `in_use` (the stated availability cost) |
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
| `node-project-source-read-only-eligible` | all old; `src/a.js` read (`atime`) yesterday, nothing in `node_modules` | eligible: rule 2 counts reads only in the dependency directories (narrowing PENDING Mark) |
| `node-project-undeclared-workspace-residual-eligible` | as `node-project-workspace-member-root-active-kept`, `w/package.json` without `workspaces` and no `.git` | eligible: the stated residual (an undeclared workspace) |
| `node-project-workspace-manifest-unparsable-unknown` | `w/package.json` not JSON above a member | `producer_unknown` |
| `node-project-nearest-project-stops-the-climb` | `w/package.json` not JSON; the member `w/packages/a` holds its own `.git`; all old but `w/src` changed yesterday | eligible: the nearest project is the member, and nothing above it is read |
| `node-project-pnpm-workspace-root` | `w/pnpm-workspace.yaml` instead of `workspaces`; `w/src` changed yesterday | `not_idle`: judged by the workspace |
| `node-project-inside-venv-filtered` | `q/venv/share/app/{package.json,node_modules}`, `q/venv/pyvenv.cfg` (a venv named without a dot, which the hidden-directory exclusion does not cover) | `filtered`: inside another dependency directory |
| `node-project-hidden-directories-not-candidates` | root `/home/tester`; `.npm/_npx/f00d/{package.json,node_modules}` and `.cursor/extensions/ext-1.0/{package.json,node_modules}` beside an idle project `p` | only `p/node_modules` found (eligible): nothing below a hidden directory under the root is a candidate |
| `node-project-pivot-root-process-not-under` | all old; a process with `root: /srv/c`, cwd `/srv/c/work` and a file there open | eligible: its climbs reach the fixpoint without meeting the project |
| `node-project-environ-other-view-residual` | all old; a process with `view: {/src: /home/tester/p}` and `NODE_PATH=/src/node_modules` | eligible: the stated residual (strings are bytes in the process's view) |
| `python-project-read-only-use-noatime-residual-eligible` | `python-project-read-only-use-kept` on a `noatime` mount | eligible: the stated residual, pinned |
| `python-project-cwd-in-project-kept` | all old; a `python3` with cwd `q/pkg` | `in_use`, `cwd` |
| `python-project-manifest-variants` | `.venv`s beside `pyproject.toml`, `setup.py`, `setup.cfg`, `Pipfile`, `requirements.txt`; beside nothing, a `requirements.in`, only a `package.json` | the first five eligible; the last three `filtered` (a venv outside a Python project) |
| `python-project-symlinked-venv-not-candidate` | `q/.venv` a symlink to `/home/tester/envs/shared` | no candidate; the target preserved |
| `python-project-vendored-tree-judged-by-repository` | `r/.git`, `r/src/main.py` changed yesterday; `r/third_party/tool/{setup.py,.venv}` all old | `not_idle`: the vendored tool's project is the repository |
| `node-project-fd-in-vcs-metadata-kept` | all old; a process holds `p/.git/HEAD` | `in_use`, `fd`: the held-file path test (the walk leaves `.git` out) |
| `node-project-workspace-member-shadowing-root-filtered` | `w/{.git,package.json (workspaces),node_modules}` and member `packages/a/node_modules` (older), all idle | `w/node_modules` removed; the member's `filtered` (an outer `node_modules` exists: Node would switch to it) |
| `node-project-workspace-member-after-root-reclaimed-eligible` | the same after that run: `w/node_modules` gone, `w`'s mtime and ctime yesterday (the deletion), its birth time old | the member's eligible: a directory's mtime and ctime are not activity |
| `node-project-new-directory-is-activity-kept` | all old; `p/docs` born yesterday | `not_idle`: a directory's birth time counts |
| `node-project-workspace-member-read-keeps-root` | as the shadowing vector, the member's `z.js` read yesterday | `w/node_modules` `not_idle`: root and members share rule 2's evidence |
| `node-project-outer-node-modules-above-root-filtered` | `/home/tester/node_modules` above the supplied root `p` | `filtered` (checked up to the mount root) |
| `node-project-marker-above-root-unknown` | root `w/packages`, the nearest project `w` (its `.git`) above it | `producer_unknown` |
| `node-project-bind-from-other-filesystem-held-file-kept` | `p/data` binds `/data` of another filesystem, also mounted at `/fast`; a process holds `/fast/data/f` | `in_use`, `fd`: another filesystem than the project's, which the project's path cannot place |
| `node-project-script-argv-kept`, `node-project-relative-argv-kept` | all old; a `node` with cwd `/` and `cmdline: [node, /home/tester/p/server.js]`, or cwd `/home/tester` and `p/server.js` | `in_use`, `command_line` |
| `python-project-interpreter-argv-kept` | all old; `cmdline: [q/.venv/bin/python, q/server.py]` (absolute), exe `/usr/bin/python3`, cwd `/` | `in_use`, `command_line` |
| `node-project-ignore-sweep-read-eligible` | all old; `p/.gitignore` and `p/README.md` read yesterday | eligible (narrowed rule 2, PENDING Mark) |
| `node-project-npm-link-read-outside-kept` | `node_modules/mylib` a symlink to `/home/tester/mylib`; `src/a.js` read yesterday | `not_idle`: a link out falls back to project-wide reads |
| `python-project-editable-install-read-outside-kept` | `.venv/lib/__editable__.q-1.0.pth`; `pkg/m.py` read yesterday | `not_idle`: an editable install falls back to project-wide reads |
| `python-project-interpreter-symlink-no-fallback-eligible` | `.venv/bin/python` a symlink to `/usr/bin/python3`; `README.md` read yesterday | eligible: the interpreter link is not a package |
| `python-project-pth-read-leaves-atime-eligible` | `q/.git`; `q/.venv` and `q/tools/.venv`, each with an import-only `_virtualenv.pth`, all old | both eligible: diskgc's own `.pth` reads (`O_NOATIME`) never make the second look read (the model moves a read file's atime only where Linux refuses `O_NOATIME`) |
| `python-project-pth-path-outside-kept` | `.venv/lib/q.pth` holding `/home/tester/q`; `pkg/m.py` read yesterday | `not_idle`: a `.pth` path outside falls back to project-wide reads |

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
`python_project_contract_takes_any_directory_name`,
`project_contracts_refuse_checkouts_allow`. The project built-ins'
frozen definitions (depth one to five, exact name, contract, 30-day floor,
checkouts refused, no release catalog):
`project_dependency_profiles_have_frozen_depth_contract_and_floor`
(golden.rs).

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
`ensure_is_refused_under_hidepid` (diskgc in a nested namespace whose
`/proc` has `hidepid=invisible`; a shell of tester's uid and another
group, as after `newgrp`, owns a directory; tester cannot list it, root
can; `ensure` below the goal, its dry run and root's `--allow-root` run
are refused, exit 2, `cause: safety`, naming `hidepid` (TASK-263; safety.md, In
use, Visibility: a process list that cannot be complete). Rootless Podman cannot mount
a `hidepid` `/proc` for the initial namespace, so diskgc runs in a nested
one, which the test-container exception lets only `hidepid` refuse. From the initial namespace without
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
Planned with the project contracts (TASK-79's container tests; the
ModelHost vectors above landed first, on Mark's steer, 2026-11-02), not
yet written:
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
`nix-garbage`, mostly path `/nix/store`), in the shape of the Docker
backend vectors. The host model's `nix:` block (the model schema is
tentative, PRD) holds the `--version` text, the answers to `nix config
show store` (or its exit code) and the pinned `store info` query (and a
later one, `store_info_later`, for the recheck), the daemon probe's
result (`serving`, `eof`, `timeout`, `wrong_magic`, or an errno by
name), diskgc's own `NIX_*` variables, the daemon's `--query --roots`
lines, and the GC (`gc`: the store paths it removes, its exit, how long
it takes, a signal while it runs). The tree carries the client's links,
`/nix/store`, `/nix/var/nix`, its socket and the profiles with their
owners and modes, so the client resolution, the layout checks and the
generation listing run on the model's `statx` and `readlink`.
`expect.nix_commands` (and `dry_run_nix_commands`) pin every Nix argv,
in order, and the exact child environment (or diskgc's own, for `config
show store`); the model shows the pinned descriptor as
`unix:///proc/self/fd/N`. A vector that expects a skip expects no GC
command, and one that skips before step 8 also no `--query --roots`. A
dry run never runs either, in any vector (the checker fails one that
does), and the model fails a GC started without the host lock inherited.
The client is the installation's (`/run/current-system/sw/bin/nix-store`
in the model's tree), its `nix` an ELF by its first bytes (`content`).
TASK-36 review round 1 added `nix-client-is-a-script`,
`nix-client-store-name-not-admitted`, `nix-client-dir-writable`,
`nix-anchor-link-not-root`, `nix-anchor-in-a-writable-directory`,
`nix-path-does-not-choose-the-client`, `nix-default-profile-anchor` and
`nix-plugin-files-are-never-loaded` (every argv starts with `--option
plugin-files ""`). Besides the table: `nix-client-not-multicall-link` (`nix-store -> ./nix`),
`nix-store-dir-env-refused` and `nix-store-env-refused` (the two variants
of `nix-state-dir-env-refused`), and
`nix-tier-runs-without-a-complete-process-list` (as the Docker and Podman
kinds, a Nix tier does not rely on diskgc's in-use check).

| Vector | Model | Expected |
|---|---|---|
| `nix-gc-collects-through-the-pinned-daemon` | multi-user layout, multi-call client 2.34.8 in `/nix/store`, daemon 2.34.8, roots answer lists every generation | plan `/nix/store` eligible, `estimated_bytes: null`; commands, each through `<dir>/nix` or `<dir>/nix-store` with `--extra-experimental-features nix-command` on every `nix`: `config show store`, pinned `store info`, `--query --roots <every generation>`, `--gc --max-freed <goal − avail>`, the pinned ones with `--store unix:///proc/self/fd/N` and env exactly `HOME` and `NIX_USER_CONF_FILES=`; the dead path removed, the rooted one and the generations preserved |
| `nix-gc-output-not-forwarded-is-no-failure` | as `nix-gc-collects-through-the-pinned-daemon`, but `command_lines_not_forwarded: 18734`: diskgc's stderr did not take most of the GC client's output (TASK-342) | the GC is still one error-free action: exit 0, `met`; the count goes to the action's log line only |
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

### Podman backend vectors

TASK-93, in `crates/diskgc-model/tests/vectors/backend/` (`podman-*.yaml`,
path `/home/tester`, the store at its default rootless graph root). The
host model's `podman:` block holds the query's answer (`podman
--remote=false info --format json`), and optionally another one for every
later query (`info_later`, the recheck); the model runs no Podman. Every
vector pins `expect.commands`; a skip runs none. The binder alone
(admitted version, remote, rootful, malformed output, absent graph root)
is `crates/diskgc-model/tests/podman_binding.rs`.

| Vector | Model | Expected |
|---|---|---|
| `podman-dangling-prunes-the-bound-store` | rootless store with a subuid-owned layer file | plan the graph root; `[podman, --remote=false, image, prune, --force]` in `/`; nothing below the store touched |
| `podman-unreferenced-prunes-and-measures-the-freed-space` | the prune (an event after the command) removes a 3G layer | plan `estimated_bytes: null`; `... --force --all`; goal met, exit 0 |
| `podman-client-missing` | no `podman` on `PATH` | `missing`, no command |
| `podman-store-on-another-filesystem` | path `/`, store on `/home` | `other_fs`, no command |
| `podman-remote-service-refused` | `host.serviceIsRemote: true` | `remote_backend`, no command |
| `podman-store-replaced-before-prune-changed` | the graph root renamed and recreated before the recheck | `changed`, no command |
| `podman-configuration-changed-before-prune-changed` | `info_later`: another run root at the recheck | `changed`, no command |
| `podman-storage-conf-edited-before-prune-changed` | the user's `storage.conf` replaced before the recheck | `changed`, no command |
| `podman-storage-conf-created-before-prune-changed` | a user `storage.conf` created before the recheck | `changed`, no command |
| `podman-separate-image-store-refused` | `/etc/containers/storage.conf` sets `imagestore` | `unsupported_backend`, no command |
| `podman-tier-runs-without-a-complete-process-list` | `hidepid`, nested PID namespace | prunes, as the Docker kinds |

Not covered: a real `podman image prune --all` in a container (rootless
Podman inside the rootless e2e container is not yet set up; the
`buildah_suite` shows rootless Buildah works there), so what Podman keeps
rests on its documentation, an isolated-store prune of dangling images
(TASK-93 notes) and the development machine's read-only listing.

## 3. Property tests

`diskgc-modelgen` generates random host models (`proptest`) and checks:

- no Cargo lock file ever deleted or moved; nothing deleted that the walk
  did not check; no symlink followed; no mount crossed (on the call trace);
- every deleted entry's owner was authorised;
- **the index is never an authority:** with a randomly stale, partial,
  collapsed (random budget, random blocks) or corrupted index, including
  units with wrong identities, wrong finders, roots or paths, every
  deletion still passes the deletion-time checks, and nothing is deleted
  that a run with a correct index (or `--no-index --tally-budget 0`)
  would refuse; the set deleted is a subset of what that run's own
  discovery finds (diskgc-modelgen `tests/index_authority.rs`, 64 cases
  by default, `DISKGC_PROPERTY_CASES` for more: the correct run is
  `ensure` from an empty index; the corrupted index has units at random
  paths of the generated tree and at missing ones, with wrong
  identities, finders of another profile, definition or root, old
  refusal hints, shifted ages, a random small budget and sometimes a
  collapse to one lump per mount, under a plain, an idle-filtered and a
  `keep_newest` selection (a retention guard ranking siblings), over trees
  that also hold hard links to their files under other names,
  `.diskgc-keep` markers, and directories diskgc can search but not read
  (TASK-252: discovery lists the latter and finds a multiply linked file
  once, under the name it meets first); on a
  still model it also asserts that the set deleted, the non-`stale`
  skips and the exit code are equal: the same skipped candidates, each
  skipped as often and for the same reason unless the run's synchronous
  tally refused it, whose sizing walk checks no owners, keep markers or
  sockets and so can name another of a candidate's refusals than the
  eligibility walk's first, a reporting difference only (TASK-261: the
  sizing walk's refusal reason differs from the eligibility walk's), or
  the three-in-a-row breaker skipped it unchecked (since TASK-340 it
  trips only for a cause other than a busy host, which backs off
  instead; here, a process diskgc cannot read): the breaker counts
  `in_use_unknown` ends in a row and any other end resets the count, so
  where it trips depends on the order the index gives the queue; the
  candidate is still refused, and this property's generated models'
  only cause of `in_use_unknown` (a process diskgc cannot read) is
  host-wide, which each case asserts, so the breaker never changes what
  is deleted there (TASK-295; saved case
  `a_reordered_queue_moves_the_busy_host_breaker`)). Two stated exceptions to "what
  that run's own discovery finds", both outside the generated models:
  re-matching a unit looks names up on the way down instead of listing,
  so a directory over the listing limit is not refused there
  (`too_many_entries`; every per-candidate check still applies), for
  units of this run's tally and for units seeded from an earlier run's
  snapshot alike (a root that grew past the limit after that run's
  listing; [architecture.md](architecture.md#the-reclaim-walk)); and a
  host failure that first appears after the synchronous tally can follow
  deletions of queued candidates (one the tally meets, listing or
  sizing, keeps the run off the index's queue:
  `a_host_error_in_the_tally_keeps_the_tier_off_its_queue`,
  `a_host_error_in_the_tallys_sizing_keeps_the_tier_off_its_queue`;
  [architecture.md](architecture.md#the-reclaim-walk));
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
Property tests also persist each failing seed (proptest's default; an
integration test's goes to `<test>.proptest-regressions` beside it),
committed so the seed reruns first; a few suites turn it off
(`grep -rn 'failure_persistence: None' crates`), as a runner that fails
on purpose must. A seed regenerates its case only while the
strategies are unchanged, and is not written inside the Nix sandbox's
read-only source, so the saved YAML stays the portable artifact
(TASK-295).
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
case checks only if one occurs). The checker's retry, its refusal to reuse a scan that did not
settle for the next candidate, and the waits for a busy host (TASK-364) are model-only: they
rest on the vectors and `busy_host.rs` alone. Here “direct” means no
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
That inventory covers `tests/models/profiles`. A corpus vector below
`tests/vectors` is replayed the same way only where its own container case
names it: `container_storage_vector_matches_linux_outcomes_and_deletion_diff`
replays `walk/container-storage-cargo-targets-in-layers.yaml` (TASK-242:
Cargo targets in Podman, Docker and containerd storage roots, one refusal
per storage root, no deletion inside one).

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

The `vm-release-tmp-reclaim` flake check (in `just e2e`) runs
the 0.1 release package, not a development build, through real `/tmp`
reclaims past the 1 h floor, which no container can: the release binary
has no test clock offset and rootless containers cannot backdate ctime. A
NixOS guest installs the package as a user would (binary on `PATH`, its
tmpfiles.d drop-in provisioning `/run/diskgc/lock` at boot, its example
config) on a tmpfs `/tmp`. With the guest clock held five days back, two
users each make, with the pinned pytest and Nix, two SIGKILLed and three
ended pytest sessions, one live session, a SIGKILLed legacy `nix-shell`
and an exited `nix develop` rc directory, a live one of each, and
unrelated decoys (`diskgc-e2e/vm-release-tmp-reclaim-cohort.sh`). Then,
as ordinary users, the plain binary runs `--select profile:pytest-tmp`,
`--select profile:nix-shell-rc` and the example config (as shipped: exit
2, `cause: config`; with its root pointed at an existing directory: the
stale tier), human and JSON. The script asserts exit codes and statuses,
that `idle-1d` passes and `idle-7d` fails (`not_idle`) on the genuine
ctime, each reclaimed session's `idle_age_seconds` of about five days,
the kept sessions' reasons and evidence (`owner_pid` with the live pid,
`temp_root`, the live shell's `fd`), that exactly the reclaimed trees left
`/tmp`, and that the live pytest then finishes its test. Every command,
exit code, human summary, JSON report and stderr is kept in the check's
output under `evidence/`.

The `vm-real-kernel` flake check (`just e2e-vm-real-kernel`, also in
`just e2e-full`; `diskgc-e2e/vm-real-kernel.{nix,py}`, TASK-304) runs the
rules no rootless container can reach on a real kernel: loop devices,
btrfs, idmapped mounts, a `hidepid` `/proc`, udisks and overlays. The
NixOS guest runs a kernel pinned in the check (`linuxPackages_6_18`) with
bcachefs from NixOS's out-of-tree module; scenario 7 asserts the kernel it
ran on, so a `flake.lock` bump cannot move it unnoticed. The development
package runs inside one privileged rootful Podman container of the VM
image (`localhost/diskgc-e2e-vm`), so both sandbox markers are present.
The container shares the guest's PID namespace (`--pid=host`), so the
in-use check sees every guest process (kernel threads included), and
shares the guest's `/dev`, so it can attach loop devices and see their
partitions. Fixtures and mounts are made in the container's own mount
namespace on a guest ext4 directory bound at `/work`, so the backing
names the kernel prints are the container's paths. The users are
`tester` (uid 1000, not in `disk`), root (`--allow-root`, `owners:
[{uid: 1000}]`, the host lock provisioned) and, for udisks, `alice`.
Each scenario starts with no loop device configured and is cleaned up
whether it passes or not. Each reclaim ends with the deletion oracle:
every path below the fixture roots is listed by type and device:inode
before and after the run, and exactly the trees of the report's
`actions` must be gone, with nothing added. Then come the expected
verdicts (safety.md, In use, Loop devices and Visibility;
architecture.md, Filesystem matching):

1. btrfs on two loop members, mounted by subvolume. A root on a sibling
   subvolume reclaims. A candidate that holds a nested subvolume or a
   snapshot, or is a subvolume itself, is `contains_mount`. `watch
   --tally-only` writes `index-uuid-<FSID>`, where FSID is the btrfs
   UUID (`BTRFS_IOC_FS_INFO`, no `EBADF`), and a second watch on the
   sibling subvolume exits 2. The second member's image lies in a
   protected Cargo target and is named only by `BTRFS_IOC_DEV_INFO`
   (mountinfo names the first): every candidate is `protected`.
2. Loop backing files. An image inside a candidate leaves `tester`'s
   candidates `in_use_unknown`/`loop_device`, while root
   (`LOOP_GET_STATUS64`, seen in strace) keeps that candidate `in_use`
   and reclaims the other. When the backing name was deleted and a hard
   link survives, `tester`'s doubt covers only the surviving parent's
   filesystem (a tmpfs candidate reclaims) and root keeps the link
   `in_use`. For a `losetup -P` partition (major 259), a protected
   image makes its filesystem's candidates `protected`; without the
   protect entry they reclaim.
3. A root-owned image under an idmapped bind (`mount --map-users
   0:1000:1`): `tester` finds it `in_use_unknown`/`loop_device` on the
   bind, while a candidate on a plain mount of the same ext4 reclaims;
   root keeps it `in_use`.
4. `hidepid=invisible` on the container's `/proc`: below the goal,
   exit 2 (`safety`); met, exit 0 with one stderr line; the dry run,
   exit 2.
5. `udisksctl loop-setup` of an ISO by `alice`, through a polkit rule
   that stands in for an active seat session. The device stays
   `root:disk 0660`, every one of her candidates is
   `in_use_unknown`/`loop_device`, and after `udisksctl loop-delete` the
   candidate reclaims. This measures TASK-296's cost; it does not decide
   it.
6. An approximation of a casper live root, not the guest's root: an
   ISO on a QEMU CD-ROM (`/dev/sr0`, `/cdrom`) with its squashfs
   loop-mounted at `/rofs`, an overlay at `/live` over it, and a
   root-owned snap squashfs on the overlay loop-mounted under it.
   `tester`'s overlay candidates reclaim. As root, the snap's overlay
   identity matches exactly on the overlay, while a candidate off it is
   `in_use_unknown`.
7. bcachefs on a loop device (the out-of-tree module): a nested
   subvolume keeps its parent's `st_dev`, and a candidate holding one is
   `contains_mount` by its `stx_subvol` (TASK-307).
8. An overlay whose upper layer is a btrfs and whose lower layer is on
   ext4 (TASK-307 review): a candidate holding a btrfs subvolume of the
   upper layer is `contains_mount`, proved there through `upperdir=`;
   a merged candidate with a lower-only part and a lower-only candidate
   are reclaimed, the lower layer untouched. A second overlay names its
   upper layer through a symlink, which diskgc does not follow: only a
   directory whose `stx_subvol` differs from its candidate's is refused.
9. An overlay with a btrfs upper layer and an ext4 lower layer
   (TASK-305). First the kernel facts: the overlay shows an upper file
   under another device than its upper name's, with and without
   `xino=on`; an unlink through it removes the upper name; and an unlink
   of a lower-only name writes a whiteout and leaves the lower name. An
   image has a hard link in the upper layer and is attached by its plain
   name: root keeps the candidate holding the link `in_use`, naming the
   upper name, and reclaims one with an upper file of no device's and a
   lower-only one holding another attached image's lower name, which
   survives and still reads. Processes of `tester` that hold an upper
   file open, work in an upper directory, or map a copied-up library by
   its upper name (the overlay shows it under its lower origin's inode)
   keep their candidates `in_use` for root. A bind of an empty directory
   over `upperdir=` after the overlay was made leaves the candidate
   holding the link `in_use_unknown`. With the plain name deleted, `tester`'s
   candidates with upper files on that btrfs are
   `in_use_unknown`/`loop_device` and a lower-only one reclaims. Where the
   upper layer cannot be reached (`upperdir=` through a symlink), root's
   candidate holding a regular file is `in_use_unknown`, one holding only
   directories reclaims. Last, the dry-run time of a 20000-file tree
   through the reached overlay (an upper lookup per entry) and the
   unreached one is recorded, not asserted.
0. Root's scan on a real kernel, with every guest process visible
   (kernel threads included), clears an unheld candidate (TASK-306); the
   kernel-thread facts the rule rests on (environ `ESRCH`, no fd, map,
   cwd or root of their own, `PF_KTHREAD` readable by a user) are
   asserted on the guest.

Root cases are dry-run first (the plan's verdicts), then reclaimed.
Every command, exit code, JSON report, stderr and strace (ioctls, and
`statx` for the `stx_subvol` each subvolume filesystem reports) is kept
under `evidence/` and printed in the build log; one run takes about two
minutes.

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
  recorded interval. Workers keep spawning and the probe keeps scanning
  until a scan contains a whole child, bounded per worker count (8 s,
  4096 scans); if no worker count is exercised within the bounds the probe
  fails with `not exercised`, distinct from a safety failure (TASK-197).
  An unsettled scan or checker result must block
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
  profile's three-day floor. The fixture copies `newuidmap` and
  `newgidmap` in setuid; under the container's no-new-privileges (Container
  setup) the setuid bit has no effect, and the test still passes (TASK-60,
  2026-10-31), so this case does not rely on them.
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
- **Nix GC** (TASK-36, its own image `localhost/diskgc-e2e-nix-gc` and
  `just e2e-nix-gc`, part of `just e2e`; diskgc-e2e `nix_gc_suite.rs`): a
  pinned multi-user CppNix 2.34.8 (the development machine's NixOS
  nixpkgs revision) whose `nix-daemon` runs as container root on the
  container's own store, diskgc as uid 1000, untrusted. The image carries
  its own `/nix/store` and database (`includeNixDB`: its paths registered
  and rooted). A pre-scenario check requires no mount at `/nix/store`
  (so no host store) and exactly the database's registered paths in it;
  then the store is copied onto a tmpfs of the scenario's own and moved
  onto `/nix/store` (1775 root:nixbld), so what a GC frees is measured on
  a filesystem nothing else writes (two containers at a time: each holds
  a copy). The deletion oracle declares exactly the dead store paths
  eligible, and below `/nix/var/nix` only the daemon's own database,
  temporary roots, GC lock and GC socket as written concurrently: a
  rooted path, generation, profile or GC root registration that
  disappears is a violation. The survival and interruption scenarios also
  compare every link below `profiles` and `gcroots` (and the out-link)
  before and after: the same object, the same text. Scenarios:
  - `nix_gc_keeps_every_kind_of_root`: a direct root, an indirect out-link
    root (`nix-store --realise --add-root`), two generations of `/nix/var/nix/profiles/system`
    and of a per-user profile, a temporary root held by a paused build,
    and a runtime root (a process with a store file open) all survive;
    dead paths are removed; the oracle lists the store before and after.
    The report gives the root check's duration next to the GC's.
  - `nix_gc_refuses_a_daemon_with_another_root_database`: the daemon
    restarted with `--store 'local?state=/nix/var/nix-b'` over the same
    `/nix/store`, B registering a path rooted only in A; the reclaim
    skips `unsupported_backend` at the root check, and the path survives.
  - `nix_gc_refuses_unlisted_versions`: a second pinned CppNix (the
    flake's nixpkgs, 2.34.9) as the client root's default profile names:
    `unsupported_backend`, with no GC run. Lix and other variants are
    refused by the same allowlist and covered by the model vector
    `nix-lix-client-refused` only (Mark, 2026-11-04: no Lix container).
  - `nix_gc_refuses_single_user_layout`: no daemon, store owned by uid
    1000: `unsupported_backend`.
  - `nix_gc_refuses_redirected_store`: `NIX_REMOTE=local?state=...`, a
    user `nix.conf` `store` setting, `NIX_REMOTE=ssh-ng://x`, and a
    relative `NIX_USER_CONF_FILES=nix.conf` (selecting `ssh-ng://…`) with
    diskgc run from that file's directory: skips as specified, nothing
    deleted in either state.
  - `nix_gc_never_runs_a_wrapper_client` (TASK-36 review): a `nix-store`
    wrapper first on diskgc's `PATH` is never run (the GC uses the
    installation's client); root's default profile naming a store path
    laid out as `…-nix-2.34.8/bin/nix` with a script `nix` is refused
    `unsupported_backend` (not an ELF), and the script never runs.
  - `nix_gc_never_loads_a_configured_plugin` (TASK-36 review): a marker
    plugin named by `NIX_CONFIG` loads into a plain `nix config show
    store` (the control) and never into diskgc's dry run or reclaim, by
    `NIX_CONFIG` or a user `nix.conf`.
  - `nix_gc_daemon_down_is_a_skip`: the daemon stopped with its socket
    kept, and a socket-activated daemon (a listening socket handed to a
    daemon that exits at once): both `backend_unavailable`, exit 1 with no
    other tier; a user excluded by `allowed-users`: the same.
  - `nix_gc_trusted_and_untrusted_user`: the keep-every-root scenario once
    with diskgc's user in `trusted-users`, once without; the same paths
    survive. Every admitted daemon version answers the probe's magic.
  - `nix_gc_sigterm_mid_gc`: 10,000 small dead paths, SIGTERM to diskgc
    once the store shrinks (the GC is deleting): exit 143, the client's
    group killed, the host lock held until the client exits, every rooted
    path present, and a second, uninterrupted GC completes;
    `nix_gc_sigkill_leaves_the_host_lock_with_the_client` the same with
    SIGKILL to diskgc as soon as its GC client starts (TASK-36 AC2).
  - `nix_gc_overshoot_is_bounded_by_the_last_path`: one 64 MiB dead path,
    need 1 MiB: at least 64 MiB freed, one GC, `met`; run with `--timeout
    0` (no deadline).
  - `nix_gc_optimised_store_frees_at_the_end`: `auto-optimise-store =
    true`, dead paths sharing files through `.links`: the space appears
    only after the links phase; measured and reported, not asserted
    against a timing.
  - `nix_gc_on_a_slow_stderr_completes_and_counts_unforwarded_output`
    (TASK-342, from the 0.2 dogfood GC that one slow `deleting` line
    failed with exit 3): 10,000 dead paths while diskgc's stderr is a pipe
    read 4 KiB every 250 ms: the GC runs to its end (every dead path gone,
    every root kept, no error, exit 1 at `100%`), and the forwarded
    `deleting` lines plus the count on the action's line cover every dead
    path.
  - `nix_gc_pinned_socket_survives_replacement`: container root replaces
    the socket pathname with a decoy listener after preflight: `changed`;
    replaced after the recheck instead: the GC still reaches the original
    daemon through the pinned fd, and the decoy receives no connection.
  - `release_candidate_nix_garbage_dry_run_then_bounded_gc` (TASK-335):
    the keep-every-root scenario through the gate-lifted
    `release-candidate` package (`diskgc-release`, which every runner's
    under-test directory carries) instead of the development build. It
    first requires a narrowed catalog that lists `nix-garbage` (`profiles
    list` and `profiles show`); then a dry run: exit 0, one eligible
    `/nix/store` row with `estimated_bytes: null`, no GC and no root
    check, nothing deleted; then a reclaim asking for 1 MiB more than is
    free: one GC whose `--max-freed` is that need (not the store's size),
    every root kept, every dead path gone, and the store loses exactly the
    dead paths. The only Nix GC scenario that runs the package. It runs in
    `_e2e-nix-gc` (`--suite fast` on the Nix GC image); its name puts it in
    the release selection too, but the `--suite release` runs use the base
    and Debian images, which leave Nix-GC-only tests out. No
    release-under-test runner on the Nix GC image: it would install the
    same binary as `diskgc`, and its host PID namespace would show the
    daemon's runtime-root scan the host's processes, where this run shows
    the package running its Nix tier in a PID namespace of its own (not
    refused: the rule of TASK-263 covers `fs` and `cargo-target` tiers).
    That the binary is the gate-lifted one, not the gated
    `diskgc-release-catalog`, rests on the under-test path and the
    `release-candidate-package` check; the test's own identity check sees
    only a narrowed catalog.
- **Signals, deadlines, the CI-hook use of `ensure`, `watch` as a plain
  process.** For `watch`, the static binary in the Nix image, a scratch
  tmpfs as the watched filesystem and the deletion oracle on every run:
  - (diskgc-e2e `watch_suite.rs`, TASK-32 and TASK-236:
    `watch_process_reclaims_to_target_then_stays_quiet_and_stops_on_sigterm`,
    `watch_tally_only_never_deletes_and_a_second_watch_exits_2` (its
    snapshot must hold every candidate, so it binds a quiet PSI file
    over each of `/proc/pressure/{io,memory,cpu}` in its container:
    `/proc/pressure` is the host's, and I/O on the machine running the
    suite engaged the brake before the first batch, TASK-310; the brake
    itself is tested by `pressure_brake_doubles_the_sleep_and_skips_batches`
    and `background_tally_backs_off_under_io_pressure`),
    `watch_tally_pauses_while_another_process_holds_the_host_lock`;
    TASK-257: `watch_second_signal_leaves_a_stubborn_child_holding_the_host_lock`,
    a second SIGTERM during a TERM-ignoring command child's grace: exit 0
    within 4 s, the child still running and holding the host lock, no
    snapshot, one `interrupted` record; the
    names below are still open where these do not cover them.)
  - `watch_process_reclaims_and_streams_json`: `watch --json` started,
    the tmpfs filled below `--min-free` with idle candidates by the
    harness: one stdout line per cycle, each a complete JSON object, the
    candidates gone to the goal; stdout closed by the reader: exit 3;
  - `watch_process_stops_on_each_signal`: SIGTERM, SIGINT and SIGHUP each
    during a slow `fs` cycle (a large candidate) and while idle: exit 0,
    a valid snapshot at `$XDG_STATE_HOME/diskgc/`, the host lock free
    after exit; SIGHUP with SIGHUP ignored at exec (`nohup`): still
    running;
  - `watch_and_ensure_share_a_filesystem`: a `watch` and an `ensure` with
    a higher `--min-free` on the same tmpfs: their reclaims never overlap
    (host lock, `/proc/locks` sampled by the harness), the `ensure`
    reports `index.source: snapshot` and skips its write, and the oracle
    holds for both;
  - CI operator's journey with `watch --tally-only` running beside the
    example pre-job hook: the hook's `ensure` reports `source: snapshot`
    with fewer tally operations than a cold run (`ensure_warms_the_snapshot`'s
    measure), and nothing is deleted outside the hook's runs.
- **Index on the real kernel:**
  - `background_tally_does_not_load_the_system`: `watch --tally-only` over
    the 100,000-entry `/tmp` fixture for 10 minutes beside a foreground
    workload (a fixed file create, stat and remove loop). Asserted: the
    diskgc process's CPU time (from `/proc/<pid>/stat`, read by the
    harness) is at most 7 % of wall time over every 60 s window, and its
    scheduling priority and I/O class are `nice 19` and idle. Measured
    and logged, not asserted (the device and scheduler decide it): the
    foreground loop's throughput and p99 latency with and without the
    tally, and the I/O scheduler of the device;
  - `background_tally_backs_off_under_io_pressure` (kernel VM, where the
    pressure is the guest's own and reproducible): a guest whose test
    disk is throttled by QEMU (`iops` limit) runs `watch --tally-only`
    over a fixture; a writer then saturates the disk until the guest's
    `/proc/pressure/io` `some avg10` exceeds 10 %, then stops. Asserted:
    diskgc logs the back-off, its batches per minute (from its log) fall
    at least 4 times below the unloaded phase while pressure is high,
    and recover within 2 minutes after it falls. A container cannot
    prove this: `/proc/pressure` there is the host's. Flake check
    `vm-tally-pressure` (`just e2e-vm-tally-pressure`,
    `diskgc-e2e/vm-tally-pressure.{nix,py}`): the tally runs as the e2e
    driver's `tally-background` in a container in the guest, over a
    fixture on a QEMU-throttled disk (so it stays busy on cold
    metadata), and a direct-I/O writer saturates a second, tighter
    throttled disk; the unloaded phase must itself stay below 10 %;
  - `sync_tally_is_useful_within_its_budget`: a cold `ensure
    --tally-budget 5s` over the fixture, with two selected tiers and a
    goal that needs both, reports `coverage: complete`, acts on the first
    tier before the second, and is judged by the oracle; the elapsed
    tally time and the time to the first deletion are logged (the dry
    run is not bounded by the budget, so it cannot show this);
  - `ensure_warms_the_snapshot`: three `ensure` runs in a row, with
    candidates added between them; from the second on, `index.source` is
    `snapshot` and the tally's operation count (logged by diskgc) is
    lower than the cold run's; the deletion oracle judges all three;
  - `snapshot_file_is_private_and_atomic`: mode 0600, the lock file
    0600, a run SIGKILLed while writing leaves the old snapshot or the
    new one, never a mix the next run accepts;
  - `collapsed_index_reclaims_the_fixture`: a build with a small budget
    (a test feature, never in the release) collapses the fixture into
    blocks and still reaches the goal, judged by the oracle.
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
- **Survey** (`survey_suite.rs`, fast tier):
  `survey_reports_tmp_by_profile_and_family_and_changes_nothing` runs the
  real `survey --json /tmp`, with and without `--in-use`, as the tester
  over a laptop-shaped fixture
  (a pytest session inside a `nix-shell.*` dir, an rc dir, three `tmp.*`
  dirs, a Claude temp root) while root holds the host lock: both must exit
  0 without waiting for the lock, the blocks counting `eligible` with
  `--in-use` and `not_checked` without (never both); report `pytest-tmp` and `nix-shell-rc`
  with their candidates and the `tmp.*` and `claude-#` families, make no
  traced removal or rename call, leave the oracle clean and write no
  index snapshot.
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
  separately packaged `release-candidate` binary and its installed files. The Docker dind
  image runs the same proof as
  `docker_shipped_tmpfiles_and_example_work_in_container` with Alpine
  `sd-tmpfiles`. That proof names the staged rule file explicitly: Alpine
  `sd-tools` 0.99.0 crashes when asked to scan a scratch root's tmpfiles.d
  directories without a file argument, so this test does not establish
  automatic rule discovery there. Every container test provisions its
  per-test host lock from the packaged rule. The separately built
  `release-candidate` package installs both files and passes the Nix
  package comparison check and the Nix/Debian container application checks.
  This packaging proof does not approve the candidate for distribution.

**Not covered end-to-end (for now):** deletion on xfs and on network
filesystems (an NFS server and client in a VM is TASK-314), and a
reclaiming `watch` as root (`--allow-root`; Deployment units, 6). A
reclaiming `watch` under a live systemd runs from the release candidate
in Deployment units, 5 and 6. Deletion on btrfs,
bcachefs and ext4 runs only in the `vm-real-kernel` scenarios above,
on that check's one kernel: ext4 on the guest's disk and on loop
devices, btrfs and bcachefs on loop devices; ext4 and xfs as separate
mounts of their own devices are TASK-314 too. A private ext4 VM disk covers the 40k
dry-plan cost only. Rootless container behavior follows the test
host's kernel; the pinned VM cases above cover the listed kernels. Other
cases rest on model tests with model-only fields.

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

### Admission evidence

The evidence a development-library built-in needs before Mark decides to
put it in a release catalog (Mark, 2026-11-05: test the in-development
profiles for 0.2; the bar is in the admission tasks, one per family). The
verdicts these scenarios support are in the profiles' rows of
[reference.md](reference.md#liveness-contracts); admission stays Mark's
decision. Per profile: the pinned real tool makes the artifact in the
container; a dry run plans exactly the expected set and none of the
lookalikes; a reclaim under the deletion oracle deletes exactly the
reclaimable part; the tool makes the artifact again; and a live
counterexample runs the tool in or against the artifact during the
reclaim.

**Project-tree family** (TASK-350; diskgc-e2e `project_tree_suite.rs`,
Nix image only). Tools: Node.js 24.21.0 with npm 11.19.0, CPython 3.14.7
(`venv`, its imports), uv 0.13.0, pytest 9.1.1 (the image's), and mypy
2.1.0, which only the under-test bundle links: the tests in `MYPY_TESTS`
(`suite.rs`) get the host's `/nix/store` read-only for its closure. The
containers have no network, so npm installs a local package that the real
npm packed. `node-modules` and `python-venv` keep a project for 30 days
after its last change, read or visible use; their scenarios advance the
gated binary's clock 31 days (the sandbox clock offset) and make "recent"
activity by setting a file's mtime or atime to an hour before that
clock's now. Roots `/home/tester/src` (the development machine's
`~/topics`), `--min-free 100%`.

- `node_modules_real_npm_projects_reclaim_only_idle_projects_then_npm_ci_restores`:
  `npm install` in a project and in a workspace (member linked, package
  hoisted) is reclaimed; a project with a source changed (rule 1) or a
  dependency file read (rule 2) an hour ago is `not_idle`; a hand-made
  `node_modules` without a `package.json` beside it is `filtered`; a
  nested one inside a candidate is never a candidate of its own; an
  install below a hidden directory is never found. `npm ci` restores both,
  and Node loads the dependency.
- `node_modules_live_node_processes_keep_their_projects`: a Node with its
  working directory in an idle project (`in_use`, evidence `cwd`, the
  contract's detail), one started from `/` naming the project's script
  (`command_line`), one holding a `node_modules` file open (`fd`, In use
  itself); once they end, the same reclaim deletes all three.
- `node_modules_home_level_install_filters_every_project_below_it`: a real
  `npm install` in the home (`~/package.json`, `~/node_modules`, as on the
  development machine) keeps every project's `node_modules` below it
  `filtered`, naming `~/node_modules`.
- `python_venv_real_venv_and_uv_projects_reclaim_only_idle_projects_then_recreate`:
  environments from `python -m venv` (with pip), `uv venv` and beside a
  `requirements.txt` are reclaimed; a changed source (rule 1) and an
  editable install's `.pth` with a project source read an hour ago (rule
  2, project-wide) are `not_idle`; an environment without a manifest and
  a hand-made `.venv` without `pyvenv.cfg` are `filtered`. `venv` and
  `uv venv` make them again and they run.
- `python_venv_live_interpreter_and_activated_shell_keep_their_projects`:
  the environment's `bin/python` started from `/` (`command_line`) and a
  shell (`sh -s`) that sourced `activate` and stays in the project
  (`cwd`) keep their environments; the shell's `python` then runs.
- `python_venv_activated_shell_that_left_its_project_is_a_named_residual`:
  the same shell after `cd /` names nothing in the project (`VIRTUAL_ENV`
  is exported after `exec`), so the idle environment is reclaimed, the
  residual named in [safety.md](safety.md#producer-liveness) (Project
  dependency directories); its next `python` exits 127, not found.
- `python_pycache_real_imports_reclaim_then_a_live_importer_recompiles`:
  the bytecode an import and pytest's assertion rewriting wrote is
  reclaimed while the importing Python still runs in the project; it then
  imports another module, which is compiled and cached again.
- `mypy_cache_real_mypy_reclaim_then_recheck_and_a_run_in_flight_is_kept`:
  mypy 2.1.0's default cache is SQLite shards (`3.14/cache.N.db`) behind
  `CACHEDIR.TAG`. One project's cache is reclaimed while a second mypy
  run, paused by a plugin while it parses a new module, holds every shard
  of its own cache open (checked before the reclaim): that cache is
  `in_use` (`fd`), and the run then succeeds. An untagged `.mypy_cache` and `.mypy_cache.bak` are never
  found; the next run rebuilds the cache.
- `pytest_cache_real_pytest_reclaim_then_a_live_session_rewrites_it`:
  `.pytest_cache` of a run with a recorded failure and of a session in
  flight are reclaimed; the session ends with exit 0 and writes its cache
  again; `pytest --lf` without the record runs every test.
- `node_compile_cache_real_node_reclaim_then_a_live_process_recompiles`:
  the caches `module.enableCompileCache()` makes under `TMPDIR=/tmp` and a
  `nix-shell.*` `TMPDIR` are reclaimed while a Node that enabled one runs;
  it then loads another module and exits 0. A cache at a
  `NODE_COMPILE_CACHE` outside the temp-prefix preset is never found.
- `candidate_catalog_project_tree_family_matches_the_development_build`
  (host PID namespace): the release path, below.

Through the gate-lifted package (TASK-359, after Mark admitted the
family to 0.2 on 2026-11-05, `node-modules` held out on 2026-11-06,
TASK-365; host PID namespace, Nix image, in the
fast suite and in the Nix release runner's `--suite release`): each first
requires the `release-candidate` package's narrowed catalog to list the
profile (`profiles list`, `profiles show`), then runs the same scenario
with that binary instead of the development build.

- `release_candidate_pytest_cache_reclaim_then_a_live_session_rewrites_it`,
  `release_candidate_python_pycache_reclaim_then_a_live_importer_recompiles`,
  `release_candidate_mypy_cache_reclaim_and_a_run_in_flight_is_kept` and
  `release_candidate_node_compile_cache_reclaim_then_a_live_process_recompiles`:
  the four scenarios above, live counterexamples included, unchanged.
  None of these profiles has a floor, so no clock offset is needed.
- `release_candidate_python_venv_keeps_projects_inside_the_30_day_floor`:
  the package has no sandbox clock offset, so only the keep side of the
  floor runs. Environments made now by `python -m venv` and `uv venv`
  are planned reclaimable by the development package (gated, so it reads
  the 31-day offset; the release runner's `diskgc` is the candidate),
  the positive control; with the same offset installed, the candidate
  keeps both `not_idle`, and its reclaim deletes nothing. The reclaim
  past the floor, the live interpreter and the activated-shell scenarios
  run only the development build and the candidate catalog below.

`node-modules` is not in the release catalog (Mark held it, 2026-11-06,
TASK-365; successor TASK-357): the gate-lifted package refuses it
(`release_candidate_catalog_refuses_every_excluded_builtin`), and it runs
the release build's catalog path only as a candidate, below.

**Package-manager cache family** (TASK-351; diskgc-e2e `pkg_cache_suite.rs`,
Nix image only, except the last). Class *delegated*: the tool's own cleaner
acts, so what a run in flight survives is the tool's locking, recorded per
profile in [reference.md](reference.md#liveness-contracts). The containers
have no network: each test serves its own registry on `127.0.0.1`
(Node's `http`: npm packuments and tarballs the real npm packed, a PEP 503
index of wheels, PlatformIO archives). A request whose path holds the
test's gate text waits for a go file, which holds the requesting tool in
flight; the log of served paths proves a fetch after the reclaim. Every scenario
also has a real cache under the tool's own variable or option
(`PIP_CACHE_DIR`, `UV_CACHE_DIR`, `npm_config_cache`, `--store-dir`,
`YARN_CACHE_FOLDER`, `CCACHE_DIR`, `GOCACHE`, `PLATFORMIO_CACHE_DIR`,
`--cacheDirectory`) and a hand-made lookalike at a wrong path; neither is
planned, and both are unchanged after the reclaim (for the profiles with a
fixed path below the root this guards only its anchoring; Jest's decoys
exercise its `jest_*` pattern and depth). Each dry run plans the
expected candidates with the profile's command (`operation`); a reclaim
under the deletion oracle then runs it, with diskgc's `HOME` another
directory, so a cleaner that ignored the path it is given and fell back to
its default would miss the measured cache. Where diskgc keeps a cache in flight
`in_use`, the blockers must be processes of that run (`fd` or `map`) and
the run must hold the named file there (read from `/proc` independently of
the report, which names no path); the test then runs the tool's own cleaner
directly to show what the tool does, reading uv's and pnpm's lock waits
from `/proc/locks`. Each test first requires the tool version
`verified_with` names. Roots `/home/tester` (jest: `/tmp`), `--min-free 100%`.

- `pip_cache_real_pip_installs_reclaim_then_refetch_and_an_install_in_flight_completes`:
  pip 26.1.2 caches only through `--trusted-host` on plain HTTP; `pip
  cache purge` empties `http-v2`; an install waiting on its next wheel holds
  nothing in the cache, is purged under and completes. Then Jest's failure
  mode (TASK-360): an install of one wheel held at that wheel, after its
  cache lookup, is purged under by diskgc (the cache then holds no
  entry); it fetches the wheel once, stores it in the purged cache and
  completes without a warning. pip creates the directories it writes into;
  that the purge also keeps them is logged, not relied on.
- `uv_cache_real_uv_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`:
  uv 0.13.0's cache holds `sdists-v10/.git`, an empty regular file, which
  the profile's `empty_checkout_markers` tolerate (TASK-360): `uv cache
  clean` removes the cache and an environment installed from it still
  imports. Before that, dry runs with, one at a time, a `.git` directory
  or a `.git` file with content in that place, an empty `.git` in
  `archive-v0`, and a checkout-shaped `.git` directory (HEAD, objects,
  refs; the image has no git) below `git-v0/checkouts` keep the cache
  `contains_checkout`. An install in flight holds `.lock` (`in_use`), and
  `uv cache clean` run directly waits for it.
- `npm_cache_real_npm_installs_reclaim_then_refetch_and_a_running_npm_keeps_it`:
  `_cacache` goes, `_logs` stays; an install in flight holds its debug log
  (`in_use`); `npm cache clean --force` run directly then removes
  `_cacache`, and the install completes.
- `pnpm_store_real_pnpm_prune_keeps_referenced_packages_and_an_install_in_flight`:
  after `pnpm remove`, the prune removes only the unreferenced package
  (adding it back fetches it again; the other project still loads); an
  install in flight holds `index.db` (`in_use`), and `pnpm store prune` run
  directly waits for pnpm's store-operation lock.
- `yarn_cache_real_yarn_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`:
  an install in flight holds the tarball it downloads (`in_use`); `yarn
  cache clean` run directly then makes it fail (`ENOENT`), Yarn Classic's
  missing lock; a retry installs.
- `ccache_real_compiles_reclaim_then_recompile_and_a_compile_in_flight_completes`:
  both `~/.cache/ccache` and `~/.ccache` are reclaimed; a compile in
  flight, held at the compiler's compile step (not its `-E`
  preprocessing: after ccache's cache lookup for it, the arguments
  checked), holds its inode cache (`in_use`) while the other cache is
  cleared; `ccache --clear` run directly leaves it to complete and store
  its result, and the program runs. diskgc itself never clears under a
  compile here (the inode cache keeps it `in_use`); that needs the inode
  cache off, which was not exercised.
- `go_build_cache_real_go_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept`:
  a build in flight (the compile of package main gated by `-toolexec`) has
  an entry mapped (`in_use`, `map`); `go clean -cache` run directly then
  makes it fail; the next build succeeds.
- `platformio_cache_real_pio_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`:
  the prune keeps installed packages; an install in flight holds its
  download and lock (`in_use`); `pio system prune --cache` run directly
  then makes it fail (`FileNotFoundError`); a retry installs.
- `jest_cache_real_jest_runs_reclaim_then_rerun_and_a_run_in_flight_completes`:
  Jest's default caches in `/tmp` and in a `nix-shell.*` `TMPDIR` are
  reclaimed (`/var/tmp`, another filesystem here, is its `other_fs` row);
  a run in flight (a gated test) holds nothing there, is cleared under,
  and, its held test then loading a module (which recreates the cache
  directory), passes and writes its cache again.
- `jest_cache_clear_under_a_run_with_no_later_module_load_fails_the_run`
  (TASK-360, Codex's counterexample): a run whose one test is held after
  Jest transformed it and requires nothing afterwards is cleared under by
  diskgc; the test passes and Jest exits 1, failing to write its
  `perf-cache-*` timings into the removed directory (`ENOENT`). The test
  asserts that failure, so a Jest that survives its clear fails it.
- `delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`
  (Nix and Debian images): with only `env` on `PATH` each profile's tier
  is `missing` and nothing changes; with stand-ins that would report
  version `0.0.1-stand-in`, each is run once, never asked its version, with
  the profile's argv, working directory and environment. diskgc has no
  cleaner version allowlist: any program of the name first on `PATH` runs
  (safety.md, Command actions).

Through the gate-lifted package (TASK-360, after Mark admitted the
lock-safe cleaners to 0.2 on 2026-11-05, four after Jest left; host PID
namespace, Nix image, in
the fast suite and in the Nix release runner's `--suite release`): each
first requires the `release-candidate` package's narrowed catalog to list
the profile (`profiles list`, `profiles show`), then runs the same
scenario with that binary instead of the development build, live
counterexample included. For uv and pnpm it stops after diskgc's `in_use`
verdict on the install in flight (the install then completes): the
directly run cleaner's lock wait is read from `/proc/locks`, which in the
host's PID namespace lists every lock on the host, more than the harness's
one consistent read takes on a busy host (seen: 105 locks), and it tests
the tool, not diskgc's binary, so only the development variant runs it:
`release_candidate_pip_cache_reclaim_then_refetch_and_an_install_in_flight_completes`,
`release_candidate_uv_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept`
(the refused marker shapes too),
`release_candidate_pnpm_store_prune_keeps_referenced_packages_and_an_install_in_flight`
and `release_candidate_ccache_reclaim_then_recompile_and_a_compile_in_flight_completes`.
`delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path` runs
the development build only. The other five (`npm-cache`,
`go-build-cache`, `yarn-cache`, `platformio-cache`, `jest-cache`) stay out
of 0.2: the
release catalog code path refuses each patched into 0.2 as
`tools/candidate_catalog.py` would
(`the_release_catalog_refuses_the_five_excluded_cleaners_and_takes_the_four`,
diskgc-core), because 0.2's release row lists no command action for it
(`command_actions`, releases.json), and the vector
`release/npm-cache-cleaner-refused-under-the-0.2-catalog` shows the
refusal before anything is read. Not exercised for this family: pip's wheel cache (sdist
builds), uv's non-default link modes, pnpm's `copy` import method, Yarn
Berry, ccache's remote storage, PlatformIO's registry API cache, Jest with
watchman, and the residual windows the liveness rows name (a clean landing
where the run holds nothing in the cache); none is reproduced
deterministically.

**Language and runtime cache family** (TASK-352, folding in TASK-330's
container tests and TASK-329's question; diskgc-e2e
`runtime_cache_suite.rs`, Nix image only; the shared harness of the
three families is `admission.rs`). Plain deletes below a home root, so
these go through the candidate catalog. Tools: Go 1.26.8, Bun 1.4.2,
node-gyp 12.4.0 (npm's own, Node.js 24.21.0), @electron/get 5.1.0 with
extract-zip 2.0.1, huggingface_hub 1.30.0, Cargo 1.99.1 and Nix 2.34.9;
each test first requires the version it names. The containers have no
network: each tool is served by a local tree or server the test makes (a
`GOPROXY=file://` module tree, the npm registry above, a Node.js headers
dist, an Electron mirror, a Hugging Face hub endpoint answering
`resolve` requests, a sparse Cargo registry of crates the real `cargo
package` made, a dumb-HTTP Git repository and a tarball for Nix, whose
store is a local one below the home). Decoys: a real cache at the tool's
own non-default location (`GOMODCACHE`, `BUN_INSTALL_CACHE_DIR`,
`--devdir`, `electron_config_cache`, `HF_HUB_CACHE`, `CARGO_HOME`,
`XDG_CACHE_HOME`) and hand-made lookalikes; both unchanged after the
reclaim. The four profiles with a one-hour floor are reclaimed with the
sandbox clock advanced two hours. Where the live run holds nothing in the
cache, the reclaim deletes the cache under it and the test requires the
failure its liveness row names, then a successful retry. Roots
`/home/tester`, `--min-free 100%`.

- `go_mod_cache_real_go_downloads_reclaim_then_redownload_and_a_build_in_flight_is_kept`:
  the reclaim deletes Go's read-only module tree (`make_writable`); with
  `GOPROXY=off` the next build fails, with the proxy it downloads again. A
  build held by `-toolexec` before compiling a dependency holds no file
  or working directory in the module cache (checked in `/proc`), but the
  go command passes `GOMODCACHE` to every tool it runs (the toolexec
  wrapper `exec`s the compiler with that environment): the cache is
  `in_use` (`environment`, the entry checked in `/proc/PID/environ`) and
  the build completes. Between its subprocesses the go command alone
  names nothing (the row's residual, not reproduced).
- `bun_cache_real_bun_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`:
  both matched paths are reclaimed (the second made through
  `BUN_INSTALL_CACHE_DIR`); the projects' hardlinked `node_modules` still
  load; an install held at the registry holds the cache directory open
  (`in_use`, `fd`) and completes.
- `node_gyp_headers_real_node_gyp_install_reclaim_then_reinstall_and_a_build_between_compiles_fails_once`:
  the image's Node finds its own headers by its prefix, so the scenario
  builds for another target, as electron-rebuild does. Fresh headers are
  `not_idle`; past the floor both versions go, a hand-made version without
  `installVersion` is never planned (the profile's `has`), and the next
  build installs its headers
  again. A build between two compiles holds nothing there: its second
  compile fails (`node_api.h: No such file or directory`, residual 1) and
  the retry installs and builds.
- `electron_download_cache_real_electron_get_reclaim_then_redownload_and_an_extraction_in_flight_is_kept`:
  a hand-made directory in `.cache/electron` is deleted with the downloads
  (residual 4); an extraction paused in extract-zip's `onEntry` holds the
  zip (`in_use`, `fd`); an install paused between its cache hit and the
  extraction holds nothing, fails with `ENOENT` on the zip (residual 1)
  and the retry downloads again. Like the electron package's install
  script, the scenario passes `electron_config_cache` as @electron/get's
  `cacheRoot` (the library reads no variable).
- `huggingface_hub_repos_real_hub_downloads_reclaim_then_redownload_and_runs_in_flight_are_kept`:
  a model and a dataset repository are reclaimed and their `.locks` stay;
  `HF_HUB_OFFLINE` then fails (residual 3) and an online run downloads
  again. A Python that mapped a snapshot file and closed its descriptor
  (`map`, as safetensors loads; Python's own `mmap` would keep a
  descriptor, so the scenario maps through libc) and a download held at
  the endpoint with its `.incomplete` blob open (`fd`) keep their
  repositories, and both complete.
- `cargo_registry_src_real_cargo_extracts_reclaim_then_reextract_and_a_build_in_flight_fails_once`:
  three crate sources are reclaimed, a hand-made one without `.cargo-ok`
  is never planned, and `cargo build --offline` extracts them again from
  `registry/cache` without a download. TASK-329's case: a build whose
  compile of `mid` is held (its working directory is mid's source:
  `in_use`, `cwd`) holds `.package-cache-mutate` shared in `/proc/locks`;
  `top`, compiled after `mid`, is deleted under it and the build fails
  with a missing file (residual 1); the offline retry succeeds.
- `nix_user_cache_real_nix_fetches_reclaim_then_refetch_and_a_fetch_in_flight_is_kept`:
  `gitv3`, `tarball-cache-v2` and `eval-cache-v6` are reclaimed (no floor)
  and filled again; `fetcher-cache-v4.sqlite` stays. A `fetchGit` held at
  the server holds its lock file and Git's working directory in `gitv3`
  (`in_use`; diskgc names Git's working directory) while the other two
  go; the fetch then completes.
- `candidate_catalog_runtime_cache_family_matches_the_development_build`
  (host PID namespace): one fixture of all seven with a Python mapping a
  hub snapshot; the candidate catalog plans exactly what the development
  build does and reclaims exactly the reclaimable set, in two selections
  (`go-mod-cache` alone: its `make_writable` delete cannot share a
  selection with plain deletes); the tools then fill their caches again.

Through the gate-lifted release-candidate package (TASK-370: Mark
admitted `go-mod-cache`, `bun-cache` and `nix-user-cache` to 0.2,
2026-11-07), the three scenarios above run with that binary (host PID
namespace, Nix image; fast suite and the Nix release runner), live
counterexamples and `make_writable` included:
`release_candidate_go_mod_cache_reclaim_then_redownload_and_a_build_in_flight_is_kept`,
`release_candidate_bun_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept`
and
`release_candidate_nix_user_cache_reclaim_then_refetch_and_a_fetch_in_flight_is_kept`.

Not exercised for this family: Go's `go mod download` and workspace
modes, Bun's other install backends (`--backend=symlink`, the residual of
its row), node-gyp's `--nodedir` and Windows `node.lib`, Electron's own
install script (its packages are not available offline; the scenario
runs the same two library calls), the hub's xet transfer and `snapshot_download`, Cargo git
dependencies and its automatic gc, and Nix's `tarball-cache` (v1) of
older versions (matched on the development host, not made here).

**Build-output family** (TASK-354, folding in TASK-331's container tests;
diskgc-e2e `build_output_suite.rs`, Nix image only; each container gets
the host's `/nix/store` read-only for the tools' closures). The CMake,
Meson and Autotools scenarios run in the fast suite; Yocto, Buildroot,
West, Bazel and the candidate catalog only in the full suite (in a whole
fast run they took 185 s to over 300 s each, against its 300 s budget).
The real build
system configures and builds a tiny project in the container: CMake 4.4.3
(Ninja 1.13.2, GNU Make 4.4.1), Meson 1.12.1, GNU Autoconf 2.73, West
1.5.0 with Zephyr 4.3.0 (native_sim/native/64), Buildroot 2026.08.1,
BitBake 2.20.0 with OpenEmbedded-Core yocto-6.1, and Bazel 7.6.0; each
test first requires the version it names. The containers have no network,
so: Buildroot runs `make defconfig` and builds host packages of a
br2-external tree with `SITE_METHOD = local` (every real package needs a
download); BitBake runs a layer's recipe with a local source to a task of
its own after `do_unpack` (every default task after it needs native tools
from the network), its sanity check writing `saved_tmpdir` and
`abi_version` as in any build; Bazel builds a genrule in WORKSPACE mode
(Bzlmod needs its registry). Buildroot and BitBake are copied into the
home, as a user's clones are (BitBake 2.20 run from a read-only tree writes
its parser tables into the working directory and then fails to import
them). Those containers get what every distribution has and the image
lacks: `/bin/sh` becomes bash (OE-Core's sanity check refuses busybox),
`/usr/bin/env` is coreutils' (both tools' scripts start with
`#!/usr/bin/env`) and `/usr/bin/file` file's (Buildroot's dependency check
requires exactly that path); BitBake passes the Nix glibc's
`LOCALE_ARCHIVE` to its server (`BB_ENV_PASSTHROUGH_ADDITIONS`). Every
profile has a three-day floor and every build is made now: each scenario
shows `not_idle` once, then reclaims with the sandbox clock advanced four
days. Each live counterexample holds a build step (`ctl/hold.sh`) during
the reclaim; the build then completes. Roots `/home/tester/src` (West:
`/tmp`; Yocto: `src/yocto`; Bazel: `~/.cache/bazel`), `--min-free 100%`.
The OpenWrt-like tree is OpenWrt's top-level layout made by hand (a real
tree needs its feeds and toolchain downloads) with real package builds in
`build_dir/target-x86_64_musl/`, as OpenWrt's include/*.mk lay them out:
the scenarios read OpenWrt v25.12.6's include/{package,host-build,cmake,
meson}.mk (flake input `openwrt-src`, never run), check the
`openwrt-build` rules, and give each out-of-source package build the
stamps package.mk names (`.prepared_<hash>`, `.configured_<hash>`,
`.built`).

- `cmake_build_anchored_real_cmake_builds_reclaim_then_reconfigure_and_a_build_in_flight_is_kept`:
  Ninja and Unix Makefiles builds (one at depth five) and one below a Git
  checkout are reclaimed; one at depth six and an in-source build are never
  found; a build whose project moved away and one whose source lies inside
  it are refused by the anchor (not planned); a build holding a checkout
  (FetchContent's `_deps/*-src`) is `contains_checkout`; a `cmake --build`
  held at a custom target is `in_use` (Make's `cwd`) and completes. In the
  OpenWrt layout an in-source package build is never found, and a package
  built in `openwrt-build` (`CMAKE_BINARY_SUBDIR`) beside OpenWrt's stamps
  is not planned (the nested-build guard, TASK-367), nor is a real CMake
  build in cmake-js's layout, `napp/node_modules/addon/build` (cmake-js
  itself is not in the image). CMake then configures and builds the
  reclaimed outputs again.
- `meson_build_anchored_real_meson_builds_reclaim_then_reconfigure_and_a_ninja_in_flight_is_kept`:
  builds in and below a checkout are reclaimed; Meson itself refuses an
  in-source build and a build directory around its source; a moved and a
  copied build are refused visibly (`unsupported_layout`, reported at their
  `meson-info/meson-info.json`); a build holding a checkout is kept; a
  Ninja held at a custom target is `in_use` (`cwd`). OpenWrt's meson.mk
  builds every package in `openwrt-build`: beside OpenWrt's stamps, not
  planned (TASK-367).
- `autotools_build_anchored_real_configure_builds_reclaim_then_reconfigure_and_a_make_in_flight_is_kept`:
  `../configure` (`srcdir=..`), an absolute `configure` at depth five and a
  build below a checkout are reclaimed; in-source builds (OpenWrt's
  default too) are never found; a moved and a copied build (`ac_pwd`) are
  refused (not planned); a build holding a checkout is kept; a `make` held
  in a rule is `in_use` (`cwd`). The builds configure and build again.
- `west_build_anchored_real_west_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept`:
  a West build is reclaimed, one whose application moved away is refused,
  a `west build` held at a custom target is `in_use` (Ninja's `cwd`); `west
  build` makes `zephyr.elf` again.
- `buildroot_output_anchored_real_buildroot_reclaim_then_rebuild_and_a_build_in_flight_is_kept`:
  Buildroot copied into a Git checkout below `src`; `make defconfig` and
  `make host-makedevs` (its source is in Buildroot's tree) build
  `output/`, which is reclaimed, and `make host-makedevs` builds it again.
  Refused (not planned): an `O=` output (its `.config` is inside) and the
  output while the top `.config` is away. `make host-makedevs-rebuild`
  with its compiler held (`HOSTCC`, a wrapper) runs from the top
  directory, so no process has its working directory in `output/`; its
  processes' exported `BASE_DIR` keeps the output `in_use`
  (`environment`), and it completes. With a br2-external tree (`make
  BR2_EXTERNAL=... diskgc_defconfig`, its local packages built) the output
  is never planned (TASK-354's fix): on a copy without its output, `make
  syncconfig` drops the tree's option from the top `.config`, as the test
  shows. The nested-build guard (TASK-367) on Buildroot's real package
  directories: `host-cmk`, a real host-cmake-package with
  `SUPPORTS_IN_SOURCE_BUILD = NO` (its host-pkgconf built from the pinned
  source in the tree's `dl/`), and `host-msn`, real Meson run as
  pkg-meson.mk runs it (a generic recipe: host-meson needs host-python3
  from the network), each with `.stamp_configured` and `.stamp_built`
  beside its `buildroot-build` (the test lists them). Neither build is
  planned by `cmake-build-anchored` or `meson-build-anchored` with both
  signals, with the stamps alone (`.br2-external.mk` moved away) or with
  the output's marker alone (the stamps moved away); with neither, both
  are (positive control). With `buildroot-build` moved away, `make
  host-cmk-rebuild` fails, then rebuilds once it is back.
- `yocto_tmpdir_anchored_real_bitbake_reclaim_then_rebuild_and_a_task_in_flight_is_kept`:
  `oe-init-build-env`, `bitbake-layers add-layer` and `bitbake -c
  diskgc_build mini` make `build/tmp`; it is reclaimed and BitBake makes it
  again. Refused (not planned): a copy of TMPDIR (`saved_tmpdir` names the
  original), and TMPDIR while its `abi_version`, the layer or
  `conf/local.conf` is away (TASK-331's TMPDIR without `abi_version`). A
  task held in `do_diskgc_build` keeps TMPDIR: BitBake keeps a FIFO for
  the task's messages in its `temp/` (`contains_ipc`, which the walk finds
  before the in-use scan would see the task's working directory in
  `tmp/work`); the task completes. Then BitBake's own Git fetcher
  (a local repository, `protocol=file`) unpacks a clone into `tmp/work`:
  TMPDIR is `contains_checkout` (the row's residual 3).
- `bazel_output_base_anchored_real_bazel_reclaim_then_rebuild_and_a_live_server_is_kept`:
  the output base of a plain workspace is reclaimed and Bazel builds again
  into the same output base. A Git workspace's output base is
  `contains_checkout` (TASK-331's question: the execroot links every
  top-level entry of the workspace, `.git` included, and the guard counts
  a symlink named `.git` as a checkout). An idle server and a build held
  in a genrule keep theirs `in_use` (each server holds files there; the
  held action runs under its server, not the client, with its working
  directory in the execroot, which the test checks). The
  install base and the repository cache are never planned. Of the output
  bases of a workspace moved away and of one on another filesystem
  (`/tmp`), the first is refused (not planned), the second visibly
  (`other_fs`, reported at the workspace's path). In a container of its
  own PID namespace the harness, its init, reaps each exited server so
  `bazel shutdown` returns.
- `candidate_catalog_build_output_family_matches_the_development_build`
  (host PID namespace; full suite only, about five minutes alone): one fixture of all seven with an idle Bazel server;
  per profile, past the floor, the candidate catalog plans exactly what the
  development build does and reclaims exactly the reclaimable set; the
  tools then build again.

Through the gate-lifted release-candidate package (TASK-370: Mark
admitted the cmake, meson, autotools and west profiles to 0.2,
2026-11-07), only the keep side of the three-day floor runs, as for
`python-venv`: that package reads no clock offset, and the ctime the idle
check reads cannot be set back.
`release_candidate_build_outputs_keep_builds_inside_the_3_day_floor`
(fast suite and the Nix release runner): real CMake, Meson and Autoconf
builds made now are planned reclaimable by the gated development package
past the floor (the positive control), then kept `not_idle` by the
release candidate with the same offset installed, which deletes nothing;
a CMake build in the OpenWrt-like layout beside package.mk's stamps is
planned by neither. `release_candidate_west_build_keeps_a_build_inside_the_3_day_floor`
does the same for a West build, in the full suite only (the release
runner runs fast-tier tests). Their reclaims past the floor ran the
development build and the gated candidate-catalog build (the release catalog code path with the clock offset) only.

Not exercised for this family: a real Buildroot target build or Yocto
image (both need downloads), Buildroot's meson-package infrastructure
(host-meson needs host-python3 from the network; its layout runs in the
Buildroot scenario, cmake-package's for real), Bazel with Bzlmod, a remote cache or `--output_base`, West
with sysbuild, Meson's other backends, Autotools layouts other than
Autoconf's `config.status` plus `Makefile`, and a real OpenWrt tree. A Buildroot
make held while it reads its makefiles (it runs the compiler then) names
nothing in `output/`, the row's residual 4, seen while this scenario was
written; the scenario holds the compile of the package's source instead.

**The candidate catalog** (`diskgc-candidate-catalog` in the under-test
bundle). The release-candidate package carries the real catalog only, and
the PRD lets no other build lift the sandbox gate, so a profile awaiting
admission cannot run through the gate-lifted package: the candidate list
holds `node-modules` (held by Mark, 2026-11-06, TASK-365), TASK-352's
four runtime caches and TASK-354's three build trees that Mark did not
admit (TASK-370 took the other seven into the real 0.2 catalog). Instead the flake
builds the release build's catalog code path (`release-catalog`: this
version's release row, its commands, the user-profile refusal, the
catalog's scope checks and narrowing) from a source copy whose
builtin.yaml also lists `diskgc-e2e/candidate-catalog`'s profiles in this
release's catalog (`tools/candidate_catalog.py`); it keeps the gate (and
so the sandbox clock offset) and, like the release candidate, has no
PID-namespace exception. The scenario checks that its catalog is exactly
the release candidate's plus that list, that the list is part of the
families with a candidate-catalog scenario and disjoint from the release
catalog, and that every member of the family it tests is in one of the
two; then, over one fixture with every member and
a live Node, its dry run plans exactly what the development build's
does, and its reclaims delete exactly the reclaimable set. It still
covers the admitted five, since its catalog holds them too, and is the
only release-catalog-path run of `python-venv` and `node-modules` past
their floor. An empty list is valid (`tools/candidate_catalog.py` then
leaves the catalog the release's). What it does not show: the
gate-lifted package with `node-modules` (not admitted), and, for the
30-day floor without the clock offset, a guest whose clock was held back
as `vm-release-tmp-reclaim` does; no such guest runs `python-venv` yet.

Not exercised: pnpm, Yarn and Bun layouts; `virtualenv`; an editable
install made by a real build backend (none is available offline); mypy's
JSON cache (`sqlite_cache = false`); Python bytecode in a read-only tree.

### Deployment units

With the units and the module
([architecture.md](architecture.md#deployment)); 1 to 3 are TASK-44's,
4 and 5 TASK-45's (their sources in `packaging/nixos/`). "Possible here" means it needs neither a VM nor
more than the flake already builds.

1. **Unit lint** (`tools/unit_lint.py`, run by `tools/test_unit_lint.py`
   in `just check`). Both templates in `packaging/systemd/` carry
   `Type=exec`, `Restart=always`, `RestartSec=30s`,
   `RestartPreventExitStatus=2`, `KillMode=mixed`, `RestrictSUIDSGID=no`
   and the other shipped hardening lines, and no option of a "no" or
   "fatal" row of the hardening table
   ([architecture.md](architecture.md#hardening)), nor one its "not set"
   rows exclude, nor, in the user template, one that implies
   `PrivateUsers=`. The system template requires the host lock
   (`AssertPathExists=/run/diskgc/lock`) and the user template names it
   nowhere (TASK-277). The user template is exactly Deployment's "The user
   unit, in full" block, and the system template that text with the next
   block's additions, so the doc and the files cannot drift. The test
   also bites each rule on edited copies.
2. **`systemd-analyze verify`** (flake check `deployment-units`, built by
   `just e2e`). The development package holds exactly its release
   descriptor row's files, each copy byte for byte (stdenv's move of
   `lib/systemd/user` to `share/systemd/user` accounted for). Each
   template, as the development package installs it
   (compared with its source), is linked under the instance names
   `diskgc-watch@srv-ci\x2dbuilds.service` and `diskgc-watch@-.service`
   (for `/`), its `ExecStart` pointed at the packaged binary, and verified
   in system and `--user` mode with `SYSTEMD_UNIT_PATH` set to the
   templates and systemd's own units for that manager. Measured in the
   build sandbox: `--user` mode verifies as is, with a scratch `$HOME` and
   `$XDG_RUNTIME_DIR`; system mode fails creating `/run/systemd` (the
   sandbox has no `/run` and a read-only `/`; `--root=DIR` does not change
   that), so it runs chrooted into a tmpfs root in a user and mount
   namespace of its own (`unshare`). On a builder without unprivileged
   user namespaces only the system half fails. `verify` exits 0 on unknown keys and bad values (it only
   warns), so any output fails the check; negative controls show a missing
   binary and an unknown key caught. The module's drop-ins are verified
   the same way on the packaged templates (4; the shell function is
   shared, `systemdVerify` in flake.nix).
3. **tmpfiles create**: `shipped_tmpfiles_and_example_work_in_container`
   (Shipped files above); `diskgc check` on `docs/example.yaml` is the
   flake check `config-check-sandbox`. Which package ships the units is
   its release row's (the release descriptor,
   [reference.md](reference.md#built-in-profile-library)): the release
   candidate's file list is exactly its row's, each copy byte for byte
   (flake check `release-candidate-package`; TASK-289: it shipped none
   while it ran `watch --tally-only`), and its `--help` shows the row's
   `watch` lines and only those.
4. **Module evaluation and build-time check** (flake checks
   `nixos-module-eval` and `nixos-module-bad-config`, built by `just e2e`;
   no VM). NixOS evaluations of the user story (user instances, one with
   its own `settings` and a quoted `--select`, one whose argument holds
   `%h`, `$HOME`, quotes and a backslash) and a CI runner (a system
   `tallyOnly` instance, an `allowRoot` one and one naming the global
   file as `configFile`) are compared case by case with the expected
   `ExecStart=` (escaped exactly as systemd unquotes it), the
   `EnvironmentFile=` reset, `User=`/`ConditionUser=`, capability set,
   `PATH` (none in user drop-ins; the system profile in system ones),
   `wantedBy`, `restartTriggers` (only the file an instance reads),
   `/etc` entries (copied, mode 0644, from the checked derivations),
   the tmpfiles package and the linger warning. The `user` option's type
   refuses a newline, the empty string, `@system`, a specifier, a UID,
   whitespace and `:`, and accepts ordinary names; a system instance
   with a newline in `user` fails the evaluation. Each assertion fires
   alone on its own example, with its message (a system-scope user
   typo and a system instance with no checked configuration included).
   With `packages.release-candidate`'s unstamped build as the package, a
   system and a user `tallyOnly` instance and a system and a user
   reclaiming one evaluate with no failed assertion, its templates
   present (TASK-362: its row runs a reclaiming `watch`, pinned); a
   reclaiming instance would fail the evaluation where a release row
   does not run a reclaiming `watch`, as do a package whose
   `passthru.diskgcBuild` lacks the templates and one without it, and a
   mode flag in `extraArgs` (hidden as `--config`'s value, and in
   `--flag=value` form) (TASK-289). The module's default package is the
   flake's choice by version (`module-default-package`).
   The drop-ins, staged on the packaged templates, pass
   `systemd-analyze verify` (2) with no output, and the checked
   configurations build. An invalid `settings` fails the derivation
   `/etc/diskgc/config.yaml` comes from, with `diskgc check`'s message
   (`testers.testBuildFailure`); with the release candidate as the
   package, so does a valid one with a user profile and a tier of a
   built-in 0.2 does not ship, each named in the release's message
   (TASK-362: the build-time check is the release's catalog).
5. **NixOS VM test** (`vm-nixos-module`, one `nixosTest` flake check,
   like `vm-release-tmp-reclaim` but run by `just e2e-full`, not
   `just e2e`), with the release candidate's unstamped build as the
   module's package (TASK-362), the package 0.2 ships the module with. A
   first
   `switch-to-configuration` from a system without diskgc provisions
   `/run/diskgc/lock`, starts the system instances and, by reloading the
   lingering user's manager, the user instances; it exits 0, or 4
   naming only instances refused below (a race of the switch). A runner
   user's system `tallyOnly` instance (running as that user) and the
   root `allowRoot` one are active; a stale generic-Linux `.env` file
   does not reach the process (the `EnvironmentFile=` reset); a user
   instance runs with its manager's `PATH`; of two of one user's
   instances on one filesystem, one is active and the other exits 2
   (`NRestarts=0`, naming the other watch's single-writer lock), as do a
   root instance without `--allow-root` (naming it), an `allowRoot`
   instance whose capability set lacks `CAP_SYS_PTRACE` (naming it), and
   the development package's own binary run as the user (a reclaiming
   `watch` naming the sandbox gate); `systemctl show`
   reports the shipped hardening properties, root's four capabilities,
   and none of the "no" and "fatal" rows of the hardening table
   (`ProtectProc`,
   `ProcSubset`, `PrivatePIDs`, `PrivateNetwork`, `RemoveIPC`,
   `DynamicUser`, `PrivateUsers`, `PrivateTmp`, `ProtectHome`,
   `ProtectSystem`) on the runner, root and a user unit; the `tallyOnly`
   instance's snapshot lands in the runner's `~/.local/state/diskgc` at
   stop and the restarted instance starts from it; SIGHUP stops an
   instance cleanly (its own log line), which restarts after 30 s
   (`NRestarts=1`) and reads its edited configuration afresh (here an
   invalid one: exit 2). A
   reclaiming user instance (`minFree = "40%"`, `targetFree = "50%"`)
   on an ext4 of its own: after real pytest runs make eight
   `.pytest_cache` directories of 8 MiB, a fill below `--min-free` makes
   one cycle (`met`) that deletes the oldest unheld caches in order up to
   `--target-free` and no further (one deletion fewer would not have
   reached it), keeps the oldest, which a process of the user holds
   open (`in_use`), and leaves the rest. Not shown: an `allowRoot`
   instance that reclaims, which is what would prove its capability set,
   and the runner's `ensure` starting from the watcher's snapshot (its
   configuration's one tier is a Cargo one, which the index does not
   hold).
6. **The release package's units under a live systemd**
   (`vm-release-watch`, `just e2e-vm-release-watch`, an `e2e-full`
   stage; TASK-362; about 8 minutes on 2026-11-07). No module: the
   package's files installed as
   [reference.md](reference.md#watch-as-a-systemd-service) tells a
   generic-Linux user, the binary at `/usr/local/bin/diskgc`, the user
   template copied by the user into `~/.config/systemd/user`, the system
   template by root, environment files and configurations as documented.
   Two deviations, both NixOS's read-only `/etc`: the system template and
   its `User=` drop-in go to `/run/systemd/system` (the file compared
   byte for byte with the package's), and the tmpfiles rule is
   provisioned at boot from the package. Each instance watches a small
   ext4 of its own, with real pytest caches of 8 MiB made a second apart
   (so `oldest` is creation order); the user runs 1,000 idle processes
   (a desktop's share; every scan reads them), and the user instance's
   configuration has a `ccache` group before its pytest caches, whose
   cleaner is a stand-in on the instance's `PATH` (its environment file)
   that waits while the test holds it. Every `met` cycle is checked the
   same way: the held cache kept, the pytest caches it deleted taken
   oldest first (any it passed over kept with a reason in its record),
   and stopped at `--target-free` (one deletion fewer would not have
   reached it).
   - the user instance does nothing above `--min-free`; under a fork
     storm of the user (twelve loops starting processes) begun before
     its start, the background tally's pressure brake engages before its
     first batch (the start's sizing, 30,000 files in one cache, is due)
     and releases after the storm;
   - a fill below `--min-free` makes one `met` cycle, the oldest cache
     held open by a process of the user (`in_use`); a fill between the
     thresholds makes no record and deletes nothing;
   - a busy host during a cycle (the storm again: beside the idle
     processes listings keep finding new ones, so the scan is
     unsettled): the cycle logs at least one busy back-off instead of
     giving its candidate up, and once the storm ends a cycle reclaims
     (`met`; one the storm held to its deadline before it is allowed).
     How long the scans stay unsettled is the guest's and the host's
     timing, so nothing more is asserted: in one run the cycle's scan
     settled within seconds, in another, under host load, it was held
     to its deadline. An `ensure` beside it is not asserted either: its
     exit 75 beside a cycle holding the lock is the container test's
     (`release_candidate_watch_and_ensure_take_turns_through_the_host_lock`);
   - a concurrent `ensure` of the user's `ccache` cache, whose cleaner
     holds the host lock it inherited, and a fill: the cycle waits for
     the lock to its `--timeout` (1 minute) and ends `lock_timeout`,
     `retry`, attempt 1, 60 s; released, the cleaner empties the cache
     and the `ensure` ends exit 1 (at `100%`), and the next cycle starts
     55 to 75 s after the last ended and reclaims (`met`);
   - the cycle's own cleaner held past the cycle's `--timeout`: the
     cycle ends `deadline` (`retry`, attempt 1), the cleaner stopped
     before it emptied the cache, the later pytest group not run; once
     released, the next cycle starts 55 to 75 s later and reclaims;
   - SIGHUP: diskgc's own stop line, exit 0 (`ExecMainStatus=0`,
     `Result=success` while the restart waits), a restart after 30 s
     (`Restart=always`, `NRestarts=1`, a new main PID) that starts from
     the snapshot; an environment file with an unknown flag, then SIGHUP:
     the restart exits 2 and the unit stays failed (`NRestarts=2`,
     `RestartPreventExitStatus=2`) with diskgc's message in the journal
     (found by its pid: a process that exits at once can lose its unit
     field to a journald race, so `systemctl status` did not always show
     it);
   - the system instance, as `runner` through a `User=` drop-in, with
     an empty capability set, reclaims its fill to `--target-free`;
     `systemctl stop` ends it with exit 0 (`inactive`, `Result=success`,
     not restarted) and its snapshot lands in runner's state directory.
   Every record and journal is saved under the check's `$out/evidence`.
   How the busy host was made: a fork storm alone, and one or two
   processes whose thread listing kept changing, let every scan settle on
   a guest of about a hundred processes (one that never holds still is
   anyway taken to be stuck after 30 s, so it cannot hold a cycle to a
   1-minute deadline); 1,500 idle processes made even a quiet cycle's
   scans slower than the 1 s freshness bound under host load. The test
   fails, never passes vacuously, when the busy back-off is not logged.
   Not shown: a root instance that reclaims; `nix-garbage` or a real
   package-manager cleaner run by a `watch` cycle in a VM; a watcher over
   days (memory: TASK-134).

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
| 5 | `cargo_reclaim_changed_between_discovery_and_locking` | paused at `flock_try:1`; `--release`, `--target`, or a newer build meanwhile: `changed`, no unlink (step 2); the project renamed into a directory holding `.diskgc-keep` meanwhile: `changed` by the path reached again under the locks (TASK-165) | `/proc/locks`: diskgc holds no lock at the pause; strace: no unlink; lock files keep their inodes | the reason `changed` |
| 6 | `cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds`; `cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions` (full tier, one run: a debug profile 1.99.1 and then the nightly built) | killed at every seeded position: the tree shows step 3's order (every part before the kill gone, every part after it whole, the part itself short by exactly the unlinks made); lock files keep their inodes and hold no lock; every profile keeps a known layout; the recovery builder rebuilds every profile and its binary runs, compiling again once the fingerprints are gone | the harness's plan and walk, strace counts, `/proc/locks` | nothing (the driver dies) |
| 7 | `cargo_race_7_a_check_between_its_locks_and_a_build_keep_the_target` | `cargo check` holding `.cargo-build-lock` and waiting for `.cargo-lock`, a build waiting behind it: `locked` at the first lock with none taken; then a holder on `.cargo-build-lock` alone and a second target: at `flock_try:2` diskgc holds no lock of the first target (step 1, Cargo's order) | `/proc/locks` of both commands, steadily; diskgc paused at the run's second lock attempt (a later target's) holding nothing of the first | `locked` at `.cargo-build-lock`, `held_us=none` |
| 8 | `cargo_race_8_run_or_test_paused_before_its_binary_keeps_the_target` | `cargo run` and `cargo test` paused in the runner (no lock held): `in_use`, nothing deleted, the binary runs; also a `cargo run` started while the driver, having judged the target eligible, is paused before its first lock (step 2: the scan made under the locks): `in_use`, or `changed` in a round where the harness's walk shows `cargo run` wrote into the profile (1.90.0 rewrites `<bin>.d` now and then), retried up to 5 rounds | `/proc/locks`: no lock held; oracle with nothing eligible, no unlink; the walk decides which verdict is right; the pause's length is logged | the reason and the evidence naming the runner |
| AC | `cargo_reclaim_keeps_a_fine_grain_locked_target` (nightly only; logged not applicable on stable) | a `-Zfine-grain-locking` build: while paused in its build script `locked` (step 1); with no build `contains_special` at a unit's `build/<package>/<hash>/.lock` under the locks, before any deletion (step 2) (TASK-205) | the measured lock names: every unit directory has an empty `.lock`, and every other lock-like name below the target is a profile `.cargo-*lock*` or rustc's `incremental/<crate>/s-*.lock`; `/proc/locks`: the paused build holds `.cargo-build-lock` and a unit `.lock`; oracle with nothing eligible, no unlink, every lock file keeps its inode; the next fine-grain build succeeds | the reasons `locked` and `contains_special`, the detail naming the unit lock |
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
  driver's pause is shorter than the in-use scan's reuse limit inside the
  protocol (1 s, Freshness: one scan serves a target's parts, TASK-173),
  and only in a round where `cargo run` wrote nothing into the profile
  (the test logs both, and warns when no round of 5 reached the check; it
  does not fail on that). The model test of that step holds it
  independently.
- Races 2 and 3, `cargo clean -p`, the split build directory and the
  shared target resume a driver paused inside a deletion only once it has
  been paused for at least one second (`resume_past_freshness`): the scan
  that cleared the part in progress started before the pause, so the next
  part's check makes a new scan, which sees the Cargo started meanwhile.
  After a shorter pause that part may still be served by the earlier scan
  (one scan serves a target's parts within Freshness, TASK-173), and
  diskgc may finish the target while the Cargo waits for its locks: safe
  by the `.cargo-lock` invariant, but not the stop these tests check.
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
code under test's fds for `-y`; the code under test has no capabilities),
`--security-opt no-new-privileges` (no setuid binary or file capability
can give the code under test privileges back; the bounding set still holds
`SYS_ADMIN` and `SYS_PTRACE`) and `--network=none`. The harness runs as container root and mounts a fresh
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
uid 1000 (groups, gid, uid) before exec; the credential test
(`tester_cannot_read_other_users_mode000_dir`) requires its inheritable,
permitted, effective and ambient capability sets to be empty and
`NoNewPrivs` to be 1. With `--seccomp-bpf` it stops only
at the traced calls, which the suite checks against a stated bound (a
20,000-file create/stat/unlink churn: one seccomp filter more than the
harness, at most 4 ptrace stops per file, at most 10 s). The tests
listed in `HOST_PID_NAMESPACE_TESTS` (`suite.rs`) also get
`--pid=host`: Owner PID permits only in the initial PID namespace, so a
test that shows a reclaim, or contrasts a nested namespace with the
initial one, needs diskgc there; and the release-candidate build plans
or reclaims only there (TASK-263; safety.md, In use: Visibility, a process list that
cannot be complete), so its tests that do are listed too, and the release
runner (whose under-test directory holds `bin/diskgc-release-default`)
gives every container the host's PID namespace and runs them one at a
time, on the whole host: each such container holds an exclusive `flock`
on `$XDG_RUNTIME_DIR/diskgc-e2e-host-pid.lock` while it runs, so other
runners and worktrees wait too. The development build in an ordinary container (its own PID
namespace) reclaims through the test-container exception of that rule,
which only the e2e build has (the bundle's `diskgc` and
`diskgc-release-catalog`; `diskgc-development` is the development package
itself, and `development_package_refuses_in_its_own_pid_namespace` shows
that it refuses there);
`release_candidate_refuses_in_its_own_pid_namespace` shows that the
release build has none (a nested namespace, with and without `hidepid`:
refused below the goal, the dry run whatever the space, and a met goal
exits 0, TASK-298; from the container's: plans). What that exposes, and how it is
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
diskgc; but it is the host uid of another container's uid 1001 (`OTHER`;
probed: both show as host uid 101000 here), whose processes diskgc would
read as the user's own; no test starts a uid 1001 process today. Their `podman run` uses Podman's default `TMPDIR`, not the dev
shell's (below the checkout in a 0700 home, which crun, running as a
subordinate uid under the shifted map, cannot reach). The runner runs these tests one at a time (each picks a free
namespace pid range by looking at every host process), and in a suite
only after every other test of it has finished, with no other container
of the suite running (TASK-363). Run beside the suite's parallel
containers, whose podman, strace, compilers and tools start processes
all the time, their process scans did not settle and kept candidates
`in_use_unknown` (`release_candidate_ccache_*` failed that way in two
fast suites of four and in two full runs, 2026-11-06); the uid 1001
overlap above can no longer meet them either. That is the suite
disturbing its own tests, a test-isolation defect, not product
behaviour. It does not cover load from other work on the host, such as
another worktree's `just e2e` running its parallel phase beside this
suite's serial tail: a busy
host still keeps a one- or two-candidate tier `in_use_unknown`, as the
executor waits out a busy host only before a tier's fourth and later
candidates, and a plan never (safety.md, In use: Freshness); that is the
product's own task. The release `python-venv` test uses project
names of its own (`rc-venv-*`), defensively:
its contract reads other processes' command lines and working
directories as text, but host-PID-namespace tests never overlap and a
container's processes end with it (probed), so no route is known. If
its positive control fails, the error lists the processes naming the
project root and the same reclaim's skipped rows (TASK-363). Nested PID
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
    `release-candidate` package and runs catalog, action, reclaiming-`watch`
    and package negatives plus real Cargo CLI reclaim, busy-lock and dry-run journeys
    on both Nix and Debian images. The race tests use the test driver and
    do not establish race coverage for the packaged binary. The candidate
    is not approved for deployment;
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
  and force a re-download. Each build replaces its link, and every gate
  drops links no recipe builds any more (Commands, Cleanup), so roots do
  not accumulate. What stays on disk, measured 2026-10-19: closures of 3.0 GB
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
   aborts otherwise. Mounting a scratch tmpfs checks layer 2's markers
   again first (`Scratch::mount`), so a code path that skipped the
   binary's own check still mounts nothing. Fixture writes go through
   `Scratch` (`scratch.rs`): `diskgc-e2e/clippy.toml` disallows std's
   mutating filesystem calls (`fs::write`, `File::create`, `create_dir*`,
   `remove_*`, `rename`, `copy`, `hard_link`, `set_permissions`, `symlink`,
   `chown`, `lchown`) in the rest of the crate, and the `e2e-clippy` check
   fails on a new one. Each existing use outside `scratch.rs` carries an
   `#[expect(clippy::disallowed_methods, reason = ...)]` that says why it
   is not a fixture write (the runner's host logs, the disposable
   container's overlay root or `/proc` controls, a probe acting as its own
   process, a name Scratch's UTF-8 paths cannot express); an expectation
   that stops matching fails the check too. `OpenOptions::open` (it also
   reads), rustix calls and spawned processes are not covered; they rest on
   review and the other layers.

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
check compares the `release-candidate` binary with a positive development
control. The default package remains gated. The separate
`release-candidate` output combines this version's release row (its
commands and its catalog: Cargo, `nix-shell-rc`, `pytest-tmp`,
`nix-garbage`, five project-tree sources, four package-manager caches,
the idle filters) and
lifted gate solely for testing in disposable containers. It is unapproved
for deployment (docs/reference.md, Built-in profile library). The `just e2e` gate runs candidate negatives
and the real Cargo reclaim, busy-lock, rebuild and dry-run CLI cases on Nix
and Debian images through their candidate under-test runners, and
`release_candidate_runs_watch_tally_only`: the gate-lifted candidate's
`watch --tally-only` starts, tallies a Cargo target, stops on SIGTERM with
exit 0, writes its snapshot and no record, and deletes nothing (TASK-289).
The Nix GC stage (`_e2e-nix-gc`) runs the candidate's `nix-garbage` dry
run and bounded GC (`release_candidate_nix_garbage_dry_run_then_bounded_gc`,
TASK-335) on the Nix GC image. The project-tree scenarios through the
candidate (`release_candidate_pytest_cache_*`, `*_python_pycache_*`,
`*_mypy_cache_*`, `*_node_compile_cache_*`, `*_python_venv_*`, TASK-359:
Admission evidence) run on the Nix image only, in the fast suite and the
Nix release runner.

Every check, container and VM test runs the **unstamped tested build** of
each package (flake.nix, `mkDiskgc`): the same source, features and
compile as the shipped `packages.default` and `packages.release-candidate`,
without the commit compiled into `--version` (it prints `(unknown)`), so
that a commit changing no source reruns nothing. The shipped packages
recompile only the binary crate with `DISKGC_GIT_REV`; the `version-stamp`
check builds them and asserts their `--version` line and that nothing but
`bin/` differs from the tested packages. So the tests establish the
behaviour of equivalent source and features, not of the exact stamped
executable. For a release version there is no stamp, and the shipped
binary is the tested one (`release-candidate-package` compares them).

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

The dev profile of all three Cargo projects (the workspace, `diskgc-e2e`,
`fuzz`) keeps no incremental state and no debug info (`[profile.dev]` in
each `Cargo.toml`, TASK-371). The flake builds release and is unaffected;
the mutation containers already set `CARGO_INCREMENTAL=0`. Panic messages
and proptest failures still name `file:line`; `RUST_BACKTRACE=1` frames
name functions only. The cost is a several times slower rebuild after an
edit (Disk budgets, `check` row). Export `CARGO_INCREMENTAL=1` for an
edit-heavy session (its `incremental/` stays in `tmp/target` afterwards),
and `CARGO_PROFILE_DEV_DEBUG=true CARGO_PROFILE_DEV_STRIP=none` for a
debugger.

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
| `check` | fmt, clippy, unit, planner, model tests, property tests (few cases), profile golden sets, in the dev shell | constantly; seconds |
| `e2e` | `check` plus fast Nix-image tests, the `e2e-one` retention check, candidate 0.1 artifact probes on Nix and Debian, the native Docker smoke, a profile-vector replay under Docker Engine, and the four-vector QEMU guest check | before every commit (the gate); each ordinary runner has a 5-minute budget including setup, and its host-PID-namespace tests a serial tail with an allowance of its own (deadlines below) |
| `e2e-full` | every Nix-image test, then the Debian and native Docker suites, the four-vector QEMU guest check, the NixOS module and release `watch` VM checks, and the cold ext4 scale VM check | at the end of every wave, in CI on every push, and before any task labelled `irreversible` is marked done; runner deadlines guard against hangs |
| `e2e-debian` | `check` plus every test but the Nix-only ones on the Debian image (Container setup: Images) | with changes to the static binary's behaviour on a non-Nix machine, the image, or the descriptor; a 15-minute deadline only guards against hangs |
| `e2e-docker` | `check` plus Docker-only tests in the pinned native dind image | with changes to Docker fixtures or Docker kinds |
| `e2e-one NAME` | one container test, keeping the container on failure | while working on one |
| `e2e-vm-release-watch` | the release package's units and reclaiming `watch` under systemd in a NixOS VM (Deployment units, 6); an `e2e-full` stage | with changes to `watch`, the units, the release row or the walk's back-off and busy-host rules |
| `e2e-images [--apply] [--grace-hours H]` | the e2e images' Podman tags this checkout's flake no longer uses and no worktree used in the last 24 h; `--apply` removes them (every gate recipe does, at its end) | to see or force what the gates prune |
| `e2e-kept [--drop] [--legacy]` | the containers failed `e2e-one` runs kept; `--drop` removes this checkout's | when done inspecting one |
| `e2e-gcroots [--apply] [CHECKOUT]` | a checkout's `.gcroots/` links no recipe builds any more; `--apply` drops them (every gate recipe does, for its own checkout) | after renaming or retiring a runner |
| `e2e-debian-one NAME` | the same on the Debian image | while working on one there |
| `e2e-docker-one NAME` | one native Docker test in a disposable dind container | while working on one there |
| `soak` | property tests with many cases | before a release, nightly CI |
| `fuzz [TARGET] [DURATION]` | `cargo-fuzz` targets (grammar, YAML loader, mountinfo, `maps`, `environ` parsers), with 1,800 seconds per target by default | hardening blocks, nightly CI |
| `mutants` | complete selection and executor mutation sweep in bounded Podman shards, with verified outcomes | hardening blocks |
| `docs-check` | offline local links and heading fragments in the root `README.md`, `reference.md` and `safety.md`, plus the core's document contract tests | after contract edits |
| `scan-timing [SCANS] [--pytest-facts]` | read-only: the in-use process scan's age when returned on this host (min, median, p95, max), with the host (gnu) build and the shipped static musl target | with changes to the `/proc` readers or the scan; against docs/safety.md's Freshness (TASK-115) |
| `ci` | `nix flake check -L` plus `just e2e-full` | CI |

`just fuzz` enters the pinned 2026-10-01 nightly shell without changing the
product toolchain. Its five targets live in `fuzz/fuzz_targets/` and call the
production parsers directly. Seeds are versioned in `fuzz/seeds/`; evolving
corpora and crash artifacts stay in the worktree's `tmp/fuzz/`. Run a single
target with `just fuzz grammar 1800`, or all five with `just fuzz`. A crash is
not done until its input is reduced, saved as a regression and fixed. The
runner also keeps full build and run logs in `tmp/fuzz/logs/`.

First full run (TASK-27), 2026-11-04 on this laptop: `nix develop -c just
fuzz TARGET 1800` for each target in turn, all exiting 0. That is one
libFuzzer process per target and the pinned nightly, with builds from another
worktree running alongside (load average 1 to 16 on 14 cores). Every target
ran its full 1,800 s from the versioned seeds with no crash, timeout, leak or
out-of-memory artifact. The runner's `-verbosity=0` prints no coverage lines,
so `cov` and `ft` come from replaying each final corpus afterwards with
`nix develop .#fuzz -c cargo fuzz run TARGET tmp/fuzz/corpus/TARGET -- -runs=0`.

| Target | Executions | exec/s | cov | ft | Corpus files | Peak RSS |
|---|---|---|---|---|---|---|
| `grammar` | 16,760,180 | 9,306 | 905 | 3,618 | 629 | 615 MiB |
| `yaml_loader` | 2,331,968 | 1,294 | 12,340 | 37,936 | 5,069 | 492 MiB |
| `mountinfo` | 8,202,765 | 4,554 | 442 | 2,138 | 573 | 511 MiB |
| `maps` | 30,341,957 | 16,847 | 278 | 1,088 | 386 | 549 MiB |
| `environ` | 155,745,566 | 86,477 | 88 | 270 | 70 | 546 MiB |

The `just mutants` inventories all five named sources, finds complete nonempty
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

**Deadlines.** Each runner's budget is also the deadline of its setup: the
interlock canary, the Podman checks, the base image load and the test
listing run in their own process groups, which are killed when the run's
deadline passes (for `one`, 15 minutes for setup, then 24 hours for the
test, which only guards a hang), and the run fails naming the step. In the
gate recipes `check` runs under `tools/deadline.py 3600` (a hang guard: a
cold `check` took 841 s on 2026-10-31). Unlike coreutils `timeout`, it
stays in the terminal's foreground process group and forwards Ctrl-C to the
command's, so interrupting `just e2e` stops Cargo too. Every Nix build a
recipe starts runs with `max-silent-time = 1800`, which the justfile
appends to `NIX_CONFIG`: a builder that prints nothing for 30 minutes
fails (the NixOS VM checks also have their test driver's own global
timeout). Nix evaluation and substitution are not bounded by it
(downloads have Nix's `stalled-download-timeout`). These limits guard
hangs; external CPU load can still use up a suite budget before every
selected test starts. A suite's host-PID-namespace tests (Container
setup) run after it, one at a time, by a deadline of their own: 20 s for
each such test (`HOST_PID_TEST_ALLOWANCE`, about 1.5 times the slowest
of them on a busy host) or a longer allowance a test is listed with
(`HOST_PID_LONG_TESTS`: TASK-354's build-output candidate catalog, 480
s, full suite), summed, from the end of the parallel phase.
Each phase has its own bound because they share no time: under load from
other work the parallel phase can end at its budget, and a tail counted
against the same 300 s then ran out before its tests started (TASK-363,
2026-11-06: one timed out, three not run). The report gives each phase's
time and bound, and a run fails if either is exceeded. Waiting for the
host-wide host-PID lock counts against the tail's allowance: a runner
that cannot take it in time fails that test with a message naming the
holder from `/proc/locks` (its pid, directory and command line), or
`unknown`.

**`e2e-one` retention.** `tools/e2e_one_retention.py` runs the real
runner's `one` twice in `just e2e` and `just e2e-full`: on
`runner_failure_probe_fails_on_purpose` (`Tier::Probe`: in no suite, it
fails on purpose after setup), whose container must be kept, exited, with
the probe's message in its log; and on `sandbox_trivial`, whose container
must be gone. It then removes only those containers. Kept containers are
bounded: see Cleanup below.

**Old images.** A runner loads its image under a tag that is the image's
Nix hash and never removes one, so each change of an image's inputs leaves
a multi-gigabyte image behind, and an image a kept `e2e-one` container
uses stays until that container is removed. `just e2e-images` lists the
tags under `localhost/diskgc-e2e` and `localhost/diskgc-e2e-*` as current
(the flake at this checkout uses them), in use (a container uses them),
recent (used within the grace period, 24 h by default) or old, and removes
nothing; `just e2e-images --apply` runs `podman rmi` (never `--force`) on
the old ones. Nothing else is listed or removed. A tag old here may be
current in another worktree on other inputs; that is what the grace
period is for. Each tag has a last-used stamp, an empty file named after
it in `${XDG_STATE_HOME:-~/.local/state}/diskgc-e2e/image-stamps/`
(per user, as Podman's image store is), whose mtime is its last use:
`just _runner` stamps its runner's image before every run (the runner
package records it in `share/diskgc-e2e/image-ref`), `--apply` stamps this
checkout's current tags and gives a never-stamped tag its first stamp,
and a removed tag's stamp goes with it. A worktree idle for longer than
the grace period therefore loses its images to another worktree's
cleanup and loads them again on its next run (minutes, not an error).
`--grace-hours 0` drops the grace for a one-off cleanup.

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

### Cleanup

Every e2e recipe leaves the host as it found it, except bounded, named
debug artefacts (TASK-311). Only this harness's own artefacts are ever
removed:

- **Containers.** Suite runs start every container with `--rm`. A failed
  `e2e-one` keeps its container for inspection; the runner labels every
  container it creates `diskgc-e2e.checkout=<checkout>` (runner.rs
  `CHECKOUT_LABEL`), and after every runner run (`just _runner`)
  `tools/e2e_kept.py --prune` keeps the newest five exited ones per
  checkout and removes older ones and those of checkouts that no longer
  exist. A container counts as the harness's only with the `diskgc-e2e-`
  name prefix and that label; walk containers (`walk-diskgc-*`) and every
  other program's are never touched. `just e2e-kept` lists them,
  `--drop` removes this checkout's, and `--drop --legacy` also removes
  unlabelled `diskgc-e2e-<pid>-*` containers on harness images that are
  over a day old (made before the label existed). Removal is
  `podman rm --volumes`, so an image's anonymous volumes go too.
- **Images.** Every gate recipe ends with `just _e2e-cleanup`, which runs
  `e2e-images --apply` (Old images, above).
- **GC roots.** `_e2e-cleanup` also runs `e2e-gcroots --apply`: a
  `.gcroots/` link whose name no recipe of the checkout's justfile builds
  (a retired runner such as `e2e-runner-release-01`) is dropped; only
  links into `/nix/store` are. `git worktree remove` deletes a worktree's
  `.gcroots/` with it (ignored files do not block it; checked
  2026-11-02), so removing a worktree needs no recipe; its kept
  containers become orphans the next prune anywhere removes. Nix GC stays
  manual and host-wide: the machine owner's call. The last line of
  `just e2e`, `tools/nix_dead.py`, prints how much `nix-store --gc` would
  free (the NAR size of `nix-store --gc --print-dead`, an upper bound).
  Listing dead paths makes the Nix daemon unlink dangling
  `/nix/var/nix/gcroots/auto` registrations, so the tool skips the
  estimate, naming them, when any dangling registration is outside this
  repository.
- **VM checks.** The NixOS test drivers run inside the Nix build sandbox
  (`/nix/var/nix/builds`), and their VM disks are removed with the build
  directory. Checked 2026-11-02 around a cold `just e2e-vm-real-kernel`:
  no new entry in `/tmp` or `$XDG_RUNTIME_DIR` from the driver, none in
  the worktree. Every `nix develop` itself leaves two empty directories
  in `/tmp`, `nix-develop-PID-*` and `nix-shell.*` (Nix 2.34.8; seen
  with `-c true`, with a shell fed `exit 0`, and with a bare
  `mkShellNoCC`, so not this flake's shell hook). That is Nix's, not the
  harness's: once per gate invocation (`nix develop -c just e2e`) and per
  `just fuzz`.
- **Self-check.** `just e2e` and `just e2e-full` start with
  `tools/e2e_selfcheck.py snapshot` and end, after `_e2e-cleanup`, with
  `verify`, which fails
  listing anything new beyond the retained set: a container labelled with
  this checkout, a Podman volume, a harness image that is neither one of
  this checkout's runners' nor stamped within the grace period, an entry
  in the checkout's `tmp/system`, an entry named like `diskgc*`,
  `nix-build-*`, `vm-state-*` or `*.qcow2` in `/tmp` or
  `$XDG_RUNTIME_DIR`, or a new untracked file in the source tree. Parallel
  runs in other worktrees are not attributed to this one.

### Disk budgets

Disk is the binding constraint of the e2e runs. Each heavy line of a
gate recipe, `fuzz`, `mutants` and the VM recipes runs behind
`tools/disk_stage.py STAGE`: before it starts, the checkout's filesystem,
the Nix store's and (container stages) Podman's graph root must each have
the stage's budget free, or the stage does not run and the recipe fails
naming the stage, the filesystem and both numbers. While it runs, free
space is sampled every second; afterwards it prints free space before and
after, the difference and the peak use (lowest free space seen). A budget
is the stage's measured peak use, rounded up to whole GiB, plus 4 GiB left
for the rest of the host, so that a stage does not start when it would
leave the disk nearly full. The peak is the drop in free space, so other
work on the host counts too; where that inflated a number, the table says
so. Never lower a budget to make a run start; free space instead
(`just e2e-kept --drop`, `just e2e-images --apply`, and with the owner's
consent `nix-store --gc`). The mutation runner keeps its own guard as well.

| Stage | Budget | Measured (2026-11-02, this laptop; other work ran alongside) | Lines |
|---|---|---|---|
| `check` | 10 GiB | cold, in a new worktree (empty `tmp/target`): 11.29 GiB peak, -11.07 GiB after (987 s), but `tmp/target` grew by about 5 GiB (to 5.3 GiB): other builds on the host account for the rest. With the TASK-371 dev profile (2026-11-07, not through `disk_stage.py`), a cold `just check` leaves `tmp/target` at 1.9 GiB instead of 6.0 GiB; one edit to `diskgc-core` then rebuilds `cargo test --workspace --no-run` in 90 s, against 18 s and 1.6 GiB of `incremental/` with `CARGO_INCREMENTAL=1` (one run each, load average about 20) | `just check` in the gate recipes |
| `flake-check` | 6 GiB | `e2e-clippy` rebuilt on its cached dependency artifacts: 0.23 GiB peak (29 s); a cold Crane dependency build was not measured and is what the budget allows for | each `nix build` of a non-VM flake check |
| `containers` | 17 GiB | a cold load of the base image (4.4 GiB kept) into a private Podman store: 12.70 GiB peak, -4.34 GiB after; `e2e-one` with the image loaded: 0.03 to 0.16 GiB peak; the first, which also built the changed runner: 1.57 GiB. The base image is unchanged by TASK-354; its build-output tools (Bazel 7.6.0 with its JDK, Buildroot's and OE-Core's host tools, three source trees) add 0.84 GiB to the under-test bundle's store closure (2.6 GiB in all), fetched once by the runner's build, and its first `e2e-one` of those tests (also building the runner) peaked at 1.75 GiB | each runner suite, `_e2e-debian`, `_e2e-docker`, the `e2e-one` recipes |
| `vm` | 8 GiB | `vm-historical-ctime` after its inputs were built: 0.51 GiB peak, -0.31 GiB after (86 s); the first VM check after a harness change builds those inputs too, as `vm-real-kernel` below did | each NixOS VM check, `e2e-vm-tally-pressure` |
| `vm-real-kernel` | 8 GiB | cold, building the changed harness binaries and VM image first: 3.05 GiB peak, -2.82 GiB after (251 s) | `e2e-vm-real-kernel` |
| `vm-cold-scale` | 8 GiB | not measured; the same 4 GiB guest disk as `vm-real-kernel` | `e2e-vm-cold-scale` |
| `fuzz` | 10 GiB | first build plus 30 s of `grammar`: 5.98 GiB peak, -5.97 GiB after (278 s), but the worktree grew by only 0.35 GiB (`tmp/target`, `tmp/fuzz`): other builds on the host account for the rest. The full five-target run (2026-11-04) left `tmp/target` at 0.5 GiB and `tmp/fuzz` at 28 MiB, with per-target peaks of 0.04 to 9.79 GiB: the first target also fetched the nightly shell into the Nix store, and the rest is other work on the host | `fuzz` |
| `mutants` | 16.06 GiB | the mutation runner's own start guard (16 GiB plus 64 MiB), never lower | `mutants` |

### Walkthrough driver

Walks (the phase3.1 acceptance signal) run in a disposable rootless Podman
container from the e2e base image with the 0.1 release package on `PATH`,
seeded data on size-limited tmpfs mounts and a host-shaped process load
(`tools/walk/`). Every reclaiming run in a walk happens there.

| `just` recipe | What |
|---|---|
| `walk-up [NAME]` | container and tmux session `walk-diskgc-NAME` (default `podman`), with the tester's shell |
| `walk-reset [NAME]` | regenerate the data from `WALK_SEED`; `WALK_TMP_BULK=N` adds N bulk `/tmp` leftovers, or `WALK_TMP_LAPTOP=RC,SESSIONS,ROOTS` the 2026-11-01 laptop `/tmp` shape (`140372,6424,54600`, bytes scaled down; `WALK_TMP_SIZE=3g` at `walk-up`) |
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

### Watch dogfood on a laptop

The one reclaiming run on a developer's own machine, an exception to
the rule that none runs there (CLAUDE.md, Test isolation), and only when
Mark asks for that run: his go is recorded in TASK-362's notes before
step 4, or nothing past step 3 runs. His rule for host dogfood: a dry run
first, every time, a release-catalog package, never a gated build or a
gate workaround. It
follows the release candidate's reclaiming `watch` from its dry run to a
service (TASK-362). The candidate built from master is a development
version: the dogfood is Mark's go for this host, not a deployment.

1. **Build and name it.** `nix build -o tmp/rc .#release-candidate`;
   record `tmp/rc/bin/diskgc --version`, its `sha256sum`, the commit,
   `uname -r`, the load (`uptime`), and `df -B1 --output=avail,size PATH`
   for the watched `PATH`. Keep everything under `tmp/dogfood/`.
2. **A narrow configuration**, `tmp/dogfood/watch.yaml`, with tiers this
   host's earlier dogfood already reviewed (for example
   `profile:nix-shell-rc && filter:idle-1d` and `profile:pytest-tmp &&
   filter:idle-1d` for `/tmp`, or `profile:pytest-cache` with `roots:`
   on one project directory); `tmp/rc/bin/diskgc check --config
   tmp/dogfood/watch.yaml`.
3. **Thresholds that make one bounded cycle.** With `A` the available
   bytes now (measured again right before steps 4 and 6, and the
   thresholds recomputed: other work moves it): `--min-free` = `A + 1G` and `--target-free` = `A + 2G`, so
   the first sample starts one cycle that stops after about 2 GiB.
4. **Dry run first**, with the same configuration, thresholds and a long
   deadline: `tmp/rc/bin/diskgc ensure --dry-run --json --config
   tmp/dogfood/watch.yaml --min-free MIN --target-free TARGET --timeout
   30m PATH > tmp/dogfood/dry.json 2> tmp/dogfood/dry.log`; exit 0.
   Read the plan: every eligible row's profile and tier expected, the
   kept rows' reasons sensible. Stop here on anything unexpected.
5. **Read-only watch**: `tmp/rc/bin/diskgc watch --tally-only --config
   tmp/dogfood/watch.yaml --min-free MIN PATH` in a terminal for a few
   minutes, then Ctrl-C (exit 0): its start line, the snapshot written,
   pressure-brake lines if any; nothing deleted.
6. **One reclaiming cycle in the foreground**: `tmp/rc/bin/diskgc watch
   --json --config tmp/dogfood/watch.yaml --min-free MIN --target-free
   TARGET --interval 10s --timeout 30m PATH > tmp/dogfood/watch.jsonl
   2> tmp/dogfood/watch.log`. After the first record, Ctrl-C (exit 0,
   "stopping on SIGINT"). Check: the record's `status` (`met`, or
   `exhausted` with the reason), every `actions[].candidate` among the
   dry run's eligible rows, no `errors`, available space after at or
   above `TARGET` when `met`.
7. **As a service**, the user template as
   [reference.md](reference.md#watch-as-a-systemd-service) installs it,
   with a user drop-in that points `ExecStart=` at the absolute store path
   `$(readlink -f tmp/rc)/bin/diskgc` (reset it with an empty
   `ExecStart=` first; on NixOS there is no `/usr/local/bin`), thresholds below `A` this time (`--min-free` =
   `A - 5G`, `--target-free` = `A - 2G`: quiet until the disk fills)
   and `--json`; `systemctl --user daemon-reload`, `start` the instance,
   leave it for a working day, then read `journalctl --user -u
   diskgc-watch@INSTANCE` (cycles, back-offs, busy-host waits) and
   `systemctl --user stop` it (exit 0, `inactive`).
8. **Record** each step's command, exit code, the environment of step 1
   and what the records show in TASK-362's notes; keep `tmp/dogfood/`
   until the notes are written. A deletion outside the dry run's
   eligible rows, an exit other than 0 at a stop, or an error record
   stops the dogfood and is a finding.

### Profiling a walk

The dev shell has `perf` and `inferno` (TASK-292). Neither is in the walk
image: copy perf's closure in (`nix-store -qR` of the dev shell's `perf`,
streamed with `tar` into `podman exec -i walk-diskgc-NAME tar -x -C /`),
then, as `tester` in the container, `perf record -F 499 --call-graph fp -o
FILE -- diskgc-TAG ensure ...` on a build with frame pointers
(`RUSTFLAGS="-C force-frame-pointers=yes"`,
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`, its own
`CARGO_TARGET_DIR`). On the host, `perf script --symfs DIR` (a directory
holding the binary at its container path and a `nix` link to `/nix`) piped
into `inferno-collapse-perf` gives folded stacks for `inferno-flamegraph`.
Limits: with the usual `kernel.perf_event_paranoid` of 2, only user-space
time is sampled, so syscall cost is invisible (count syscalls with
`strace -c` on a short run instead; under `strace` the in-use scans miss
their one-second bound, so it distorts timing); and on hybrid CPUs one run's
rate swings with the cores the scheduler gives it (`cpu_core` against
`cpu_atom` samples), so compare builds interleaved, never one run each.
