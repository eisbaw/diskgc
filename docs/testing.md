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

**Checked-in profile vectors.** `crates/diskgc-model/tests/models/profiles/*.yaml`
wrap an ordinary `host_model: 1` under `host:` with a selected built-in
`profile`, `path`, optional `roots`, `min_free`, and explicit `expect` data.
The shared `tests/profile_vectors.rs` runner loads every YAML file, runs
`--dry-run` and reclaim on *separate* `ModelHost` copies, and compares the
complete plan, action, and skip sets. `expect.removed` names paths that
existed before reclaim and must disappear; `expect.preserved` names paths
that existed before reclaim and must survive, including missing-source
outputs that discovery cannot plan. This lets Cargo retain its tagged target
directory while removing contents. The runner checks that dry-run never
called `unlinkat`, that each action has a removed-path postcondition, and
that reclaim unlinked nothing outside the expected actions. The Python
vector's matcher mutation changes every
`__pycache__` path pattern to `__wrong_cache__`; the unchanged vector then
fails at its expected plan, proving the gate bites.

To add a vector, copy a case in that directory, keep `profile_vector: 1`,
model the candidate and protected neighbors under `host.tree`, and write
the expected plan and reclaim results independently of diskgc's report.
Put the candidate's recorded source text in a file's `content:` when the
profile reads one. Use `processes[].fds` for an in-use file and model
`mtime` for an age boundary. Then run
`nix develop -c cargo test -p diskgc-model --test profile_vectors` and
`nix develop -c just check`. Unknown YAML keys and invalid host references
fail loading; a new YAML file is picked up without registering a Rust test.

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
  namespace's; `none`: absent). The top-level `own_uid_map` is diskgc's
  own (same default, `none` too), `own_uid` its user id (default 1000; 0
  models running as root), and `sandbox_gate` the pre-release gate as
  `LinuxHost` has it (`open`, the default; `closed`: `may_delete` and the
  deleting calls are refused; `refuses_deletes`: only the deleting calls
  are). `command_exit` is the exit code of a modelled command action
  (default 0); `command_error_after_start: true` models a forwarding or
  wait error after spawn, so the executor must still report an action and
  observed space change. The model records argv, environment and checked
  cwd but starts no real child. `locks` give their holder an fd of its own. As on
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
  trigger, `when: before`, on `openat2`, `statx`, `read_dir`, `unlinkat`,
  `fchmod`, `bound_unix_sockets` or `statvfs`), `mount` (`{id, at, fs, dev,
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
source marker admission, term filters, unions and raw `!profile:` overlap.
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
process projections, two single-age clock projections, and four mixed-age
originals requiring the QEMU guest. Every direct or projected
entry names its registered container case. Here “direct” means no
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

The two other checked-in host-model YAMLs are classified separately:
`sketch/locked-cargo-target.yaml` uses a call-time event and exact external
space accounting, while `docker-bundled.yaml` supplies scripted Docker
responses. Both fail materializer preflight with a named model-only field.
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
- **Fork churn during the in-use scan:**
  `host_inuse_scan_settles_after_fork_churn` runs the real Linux scanner in
  a disposable container with 0, 1, 4 and 8 bounded child-spawning workers.
  It timestamps each child's spawn and wait with `CLOCK_BOOTTIME` and
  requires one entire spawn-and-wait interval inside a measured scan's
  recorded interval. An unsettled scan or checker result must block
  clearing. It reports process-list rounds, duration, whether the 16-listing bound
  was hit, and whether the checker retried. After reaping every worker,
  it requires the unused candidate to clear. This exercises the
  current host kernel. Four Nix QEMU checks run the same case in disposable
  Podman containers on pinned Linux 5.15.221, 6.1.189, 6.6.158, and 6.12.112
  guests. The 5.15 guest has four vCPUs; the others have two. The guest
  asserts its online vCPU count before testing, so the scanner and fork
  workers can overlap. The 5.15 VM uses a separate exact Nixpkgs pin
  (`6f3b13fcf904691a274955cc80c9f2ed655ff6a9`) for the whole test
  driver and NixOS guest; its image and static binaries still come from the
  primary project pin. A direct guest probe holds a pidfd across child
  release and reap: the live child has no poll event and accepts signal 0;
  the released pidfd polls readable without HUP on 5.15, 6.1 and 6.6,
  with HUP on 6.12, while
  `pidfd_send_signal(pidfd, 0)` returns ESRCH on all four. In each guest,
  `host_pidfd_release_after_reap` pins a live child with
  `LinuxHost::proc_identity`, checks that it is not released, reaps it while
  retaining the pidfd, then requires `LinuxHost::proc_identity_released` to
  return true. The 5.15, 6.1 and 6.6 cases directly bite the no-HUP fallback;
  6.12 is a HUP control. Podman's graph root is on each guest's private ext4
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
  The VM checks cover four kernel points, not every kernel in the supported
  5.8–6.8 range. Linux 5.8 and 6.8 and a loaded-CI host remain to be
  measured for TASK-64 G.
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
  `mktemp-dir`, `nix-shell-tmpdir`, `nix-shell-rc` and `tmp-entry` to remain
  absent from the built-in library. The user-configured probe is only run
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
- **Nix GC** in a container with its own store and daemon.
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
  `met` (0), `exhausted` (1), a runtime misconfigured tier reported as
  `error` (3), and `lock_timeout` (75) while another real reclaim holds the
  host lock. The deletion oracle judges every run; the competing command
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

**Not covered end-to-end (for now):** real block filesystems (ext4 or xfs
as separate mounts, btrfs) and network filesystems cannot be mounted in a
rootless container; behaviour on specific kernel versions follows the test
host's kernel; systemd is not run for real. These rest on model tests with
model-only fields. A VM suite can be added later.

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
  rather than wait); the race fixture's `build.rs`, which waits at the gate
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
harness, at most 4 ptrace stops per file, at most 10 s). A second
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
It also runs a separate exact `containers-image-tmp` dry run with genuine
five-day kernel ctimes: a held spool is `in_use`, a free spool is eligible,
a fresh spool is `not_idle`, and a live producer's dormant spool is eligible.
The producer reads and closes its spool before going idle, has no candidate
reference in `/proc`, and reopens the same bytes when resumed. This is a
counterexample to general dormant-spool protection, not evidence that
Buildah itself reuses such a spool. The original vector independently
verifies free deletion and held preservation with the exact built-in.
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
`release-01-candidate` output combines the proposed Cargo-only catalog and
lifted gate solely for testing in disposable containers. It is unapproved
for deployment until TASK-67 closes; /tmp admission and operator policy
decisions remain unresolved. The `just e2e` gate runs candidate negatives
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
| `e2e-full` | every Nix-image test, then the Debian and native Docker suites and the four-vector QEMU guest check | at the end of every wave, in CI on every push, and before any task labelled `irreversible` is marked done; runner deadlines guard against hangs |
| `e2e-debian` | `check` plus every test but the Nix-only ones on the Debian image (Container setup: Images) | with changes to the static binary's behaviour on a non-Nix machine, the image, or the descriptor; a 15-minute deadline only guards against hangs |
| `e2e-docker` | `check` plus Docker-only tests in the pinned native dind image | with changes to Docker fixtures or Docker kinds |
| `e2e-one NAME` | one container test, keeping the container on failure | while working on one |
| `e2e-debian-one NAME` | the same on the Debian image | while working on one there |
| `e2e-docker-one NAME` | one native Docker test in a disposable dind container | while working on one there |
| `soak` | property tests with many cases | before a release, nightly CI |
| `fuzz` | `cargo-fuzz` targets (grammar, YAML loader, mountinfo, `maps`, `environ` parsers), in a nightly dev shell | hardening blocks, nightly CI |
| `mutants` | `cargo-mutants` on the safety-critical modules (workspace tests) | hardening blocks |
| `ci` | `nix flake check -L` plus `just e2e-full` | CI |

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
