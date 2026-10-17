# diskgc testing

Normative for how the PRD's acceptance criteria and the guarantees in
[safety.md](safety.md) are tested.

**No test ever runs diskgc against the developer's live system.** Tests run
against a modelled host (in-process) or inside a rootless test container.

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
   tmpfs mounts (`/tmp`, `/run/diskgc`, `/home/tester`; Container setup),
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
   (counted), or reached through a symlink, written with `.` or `..`, or
   relative to a deleted directory (each logged); the snapshot still judges
   the object itself. The oracle is computed from the fixture's spec, never
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
  a JSON Schema is generated for editors.
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
  /home/mark/src/app/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug:
      .cargo-lock: {}
      deps: {size: 8G, mtime: 2026-10-01T00:00:00Z}
locks:
  - {path: /home/mark/src/app/target/debug/.cargo-lock, holder: 4242, mode: exclusive}
processes:
  - {pid: 4242, exe: /nix/store/x-cargo/bin/cargo, cwd: /home/mark/src/app}
events:
  - at: {op: flock, path: /home/mark/src/app/target/debug/.cargo-lock, nth: 1, when: after}
    create: /home/mark/src/app/target/release/.cargo-lock
expect:
  exit: 1
  skipped: [{candidate: /home/mark/src/app/target, reason: locked}]
```

Fixtures live in `tests/models/<area>/<case>.yaml`, one named test per file;
JSON reports are snapshot-tested with durations, timestamps and seed
redacted.

Model scenarios required by [safety.md](safety.md) and
[architecture.md](architecture.md), among others:

- an entry renamed out of a candidate, or replaced by another inode,
  between the walk and its unlink: the replacement is never deleted, the
  candidate is `changed`;
- a candidate directory renamed elsewhere mid-delete: deletion continues on
  the checked object only;
- a user-owned directory containing another user's file: `not_owner`;
- a bucket whose later member is opened by a process after an earlier
  member was deleted: the later member is skipped (`in_use`);
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

**Profile golden sets.** Each built-in profile has a fixture tree and a
golden set of what it matches there. Any change to that set fails CI unless
the profile was renamed ([reference.md](reference.md#built-in-profile-library)).
This also covers the profile's uses under `!profile:` and `protect`,
since those use the same matching.

## 4. Differential contract tests

`materialize(model)` builds a host model for real on a scratch tmpfs inside
a test container (directories, `fallocate`d files, owners via subuid ranges,
bind and stacked mounts, symlinks, helper processes holding locks or
files), and the same scenarios run on `ModelHost` and on `LinuxHost`;
traces and outcomes must match. Model fields that cannot be built for real
in a rootless container (clock, kernel capabilities, exact `statvfs`
numbers, filesystem types other than tmpfs, NFS, scripted Docker/Nix,
delayed freeing) are marked *model-only*; tests that use them say so.

## 5. End-to-end tests in containers

The real binary in **rootless Podman containers**, built from images Nix
produces. They run everywhere, need no KVM, and isolate the test's
filesystem, processes and mounts from the developer's system.

- **Cargo race tests**, each with real Cargo processes, against every
  supported Cargo version (at least the oldest supported stable and one
  dated nightly), on fixture projects with no registry dependencies,
  offline. Races are made deterministic (a helper holding `.cargo-lock`
  makes Cargo take `.cargo-build-lock` and block; a `build.rs` waits on a
  control file; a `runner` wrapper pauses between compile and run):
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
  deletion including the eligibility walk).
- **Dormant shells:** real `nix-shell` and `nix develop` sessions left idle
  with their `TMPDIR` under `/tmp`: their directories are skipped.
- **Docker** in its own container (`docker:dind` under rootless Podman,
  verified to work on the development machine), images preloaded offline;
  backend binding: a non-default builder or a remote context is skipped
  with its reason.
- **Nix GC** in a container with its own store and daemon.
- **Signals, deadlines, the CI-hook use of `ensure`, `watch` as a plain
  process.**
- **Not only Nix:** the same tests in a container from a common non-Nix
  distribution image (e.g. Debian) with the static binary, without Nix, and
  a variant without Docker.
- **Shipped files:** the systemd unit is checked with `systemd-analyze
  verify` (in `just check`); the tmpfiles drop-in is applied with
  `systemd-tmpfiles --root=<scratch> --create` in the container harness
  and the resulting lock file checked. The NixOS module is checked by
  evaluation in `nix flake check`.

**Not covered end-to-end (for now):** real block filesystems (ext4 or xfs
as separate mounts, btrfs) and network filesystems cannot be mounted in a
rootless container; behaviour on specific kernel versions follows the test
host's kernel; systemd is not run for real. These rest on model tests with
model-only fields. A VM suite can be added later.

Why not FUSE: it has its own `flock`, `statx` and mount-id semantics, needs
privileges, and is a large extra moving part.

Container tests cannot run inside the Nix build sandbox, so they run from
`just e2e`, not as `nix flake check` checks.

## Controlled pause and kill points

Some tests must stop diskgc at an exact point (race test 5 pauses it
between discovery and locking; race test 6 kills it at seeded call
positions; the replace-between-walk-and-unlink test needs the replacement
at one exact moment). In the model these are events on the `Host` call
trace. In containers, the e2e image uses a **test-only `Host` wrapper**
(compiled only into the e2e build of the binary, never into the release
package) that pauses or aborts at a call position given by the harness.
This is the one sanctioned exception to "no hooks in the product": it lives
outside the product build, and the release build is checked not to contain
it.

## Container setup

Each container test runs in its own container (clean `/proc`, tests run in
parallel) with `--cap-add SYS_ADMIN` (effective only inside the user
namespace, needed for tmpfs, bind and stacked mounts and `unshare`),
`--cap-add SYS_PTRACE` (container root's strace needs it to resolve the
code under test's fds for `-y`; the code under test has no capabilities)
and `--network=none`. The harness runs as container root and mounts a fresh
tmpfs per test at `/tmp` (the scratch for fixtures), at `/run/diskgc`
(0755 root, where it provisions the host lock `/run/diskgc/lock`, 0644
root, as the tmpfiles lines in [safety.md](safety.md#host-lock) do on a
host whose `/run` is a tmpfs) and at `/home/tester` (0755 tester); it keeps
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
or reload the base. The container root must be Podman's native overlayfs:
the runner checks `podman info` before starting anything and each test
checks its root mount, and both refuse fuse-overlayfs or another storage
driver, whose inode numbers and birth times the snapshot keys were not
validated on.

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

## Commands

| `just` recipe | What | When |
|---|---|---|
| `check` | fmt, clippy, unit, planner, model tests, property tests (few cases), profile golden sets, `systemd-analyze verify` of the shipped unit, in the dev shell | constantly; seconds |
| `e2e` | `check` plus the fast container tests: interlock, deletion oracle, core filesystem scenarios, a Cargo smoke on one toolchain | before every commit (the gate); budget under 5 minutes for the container phase (canary, image check, every test), durations reported per test; over budget it fails, and tests still running at the deadline are killed |
| `e2e-full` | every container test, fast and full tiers: all race tests on all toolchains, dind, the Nix daemon, per-tool caches, the Debian variant, differential tests | at the end of every wave, in CI on every push, and before any task labelled `irreversible` is marked done; a 1-hour deadline only guards against hangs |
| `e2e-one NAME` | one container test, keeping the container on failure | while working on one |
| `soak` | property tests with many cases | before a release, nightly CI |
| `fuzz` | `cargo-fuzz` targets (grammar, YAML loader, mountinfo, `maps`, `environ` parsers), in a nightly dev shell | hardening blocks, nightly CI |
| `mutants` | `cargo-mutants` on the safety-critical modules (workspace tests) | hardening blocks |
| `ci` | `nix flake check -L` plus `just e2e-full` | CI |

Requirements for `just e2e`: Nix and rootless Podman on the host (native
overlay storage driver, see Container setup), with a
subuid range covering uids 1000 and 1001 (the dev shell does not provide
Podman; CI runners that are themselves containers need nested Podman).
Each test's tier is declared in `diskgc-e2e/src/suite.rs`. Coverage of unit and model tests
via `cargo llvm-cov`.
