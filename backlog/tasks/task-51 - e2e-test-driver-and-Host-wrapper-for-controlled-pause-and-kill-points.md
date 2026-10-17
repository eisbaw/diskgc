---
id: TASK-51
title: e2e test driver and Host wrapper for controlled pause and kill points
status: In Progress
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 11:40'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
  - TASK-3
  - TASK-59
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-1 container tests need a diskgc process before the ensure command exists, and race tests 5 and 6 plus the replace-between-walk-and-unlink test must stop it at an exact call. Build the e2e-only test driver (a small binary over diskgc-core and LinuxHost that runs single operations on a fixture: walk, delete, in-use check, Cargo discovery and protocol) and the test-only Host wrapper from docs/testing.md#controlled-pause-and-kill-points (pause or SIGKILL at call k, driven by the harness). Neither is part of the release build.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The driver runs a walk and a delete on a container fixture, and the harness pauses it at a chosen Host call and resumes it, and kills it at a chosen call
- [x] #2 The release build is verified not to contain the driver or the wrapper (a test inspects the release package)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) Binaries reach containers through e2eUnderTest in flake.nix: its $out/bin is bind-mounted read-only at /opt/diskgc-e2e/bin. Add the driver there; code changes never rebuild the base image. (2) under_test::run_as_tester blocks until exit and records strace; pause/kill control needs a spawn variant. Control files the uid-1000 driver must read or write belong on the scratch mount: /run/diskgc-e2e is root-only. (3) Container root has CAP_SYS_PTRACE, so it can read /proc of the driver and kill it.

Forward-carried from task-59: (1) Judging is mandatory. The spawn/pause/kill variant of run_as_tester must enter its run in ctx.ledger (under_test::Ledger), record Run::started before spawning, and the test must pass the finished Run to Oracle::judge_runs. A run killed at call k is judged like any other; its strace record may end in '= ?' (Outcome::Unknown, counted as possibly effective). An oracle armed after a run started refuses it. (2) strace runs with --seccomp-bpf (under_test::STRACE_OPTIONS), which needs -f; a 'strace: ...seccomp...' stderr line fails the run, so keep -f in any new invocation. (3) Bite tests can be registered with suite::Expect::FailWith(text): the test passes only if it fails with that text. (4) Traced paths that do not match the before snapshot are classified and logged (oracle::Unmatched). A driver that renames or reopens directories will show SymlinkPrefix, NonCanonical or DeletedDirectory lines; those are diagnostics, and the snapshot diff is the verdict.

Forward-carried from task-3: diskgc-e2e now depends on diskgc-core directly as well as diskgc-linux. Keep diskgc-e2e/Cargo.lock in sync (cargo metadata --locked). A pausing or killing Host wrapper can be generic over H: Host and delegate to LinuxHost; every port call is a method, so call positions are countable. Probe pattern: diskgc-e2e/src/host_probe.rs runs LinuxHost as tester, prints indexed result lines, and host_suite.rs judges every run with the oracle.

From task-50: the test driver and the Host wrapper must delete only through LinuxHost's Delete methods, so through the sandbox gate. The source scan deleting_syscalls_only_behind_the_gate covers diskgc-linux only, so a raw unlinkat in the driver would not be caught: do not add one. The wrapper forwards Delete calls and adds pause/kill points; it must not mint permits.

Update from task-50's review: the scan (crates/diskgc-linux/src/source_scan.rs) now covers every production crate in crates/, but still not diskgc-e2e, where the test driver lives. A raw unlinkat in the driver would still not be caught; the driver must delete only through LinuxHost's Delete methods.

Implementation (task-51):
- Port change: Host::read_dir(&Fd) -> Result<Vec<OsString>, HostError> (diskgc-core host.rs). Names only, deliberately: d_type can be DT_UNKNOWN and is stale when used; every caller must statx each name anyway. LinuxHost: openat(fd, ".", O_RDONLY|O_DIRECTORY|O_CLOEXEC) then getdents64 to the end through a shared helper dir_names(OwnedFd) using rustix Dir::new (no reopen); ProcRoot::names uses the same helper now (the Dir::read_from item of task-61). Works on O_PATH fds (as resolve returns). ModelHost: Op::ReadDir (read_dir), names from the inode's entries in name order, ENOTDIR for a non-directory; schema regenerated. Tests: diskgc-linux read_dir_lists_every_name_on_each_call (3000+ names, O_PATH and readable fds, repeated listings, ENOTDIR; bite: dup instead of reopen fails it), diskgc-model read_dir_lists_the_names_of_a_directory_inode.
- Wrapper: diskgc-e2e/src/controlled.rs, Controlled<H: Host> (Delete when H: Delete). One counter over every Host and Delete method; points K or OP:N (method name, e.g. unlinkat:2) from --pause-at/--kill-at; acts before the call; unreached points fail the run. Pause = stdin/stdout handshake (control line 'diskgc-e2e-control: pause call=K op=OP nth=N pid=P', block in read(0), a line resumes, EOF exits FAIL). Kill = control line then SIGKILL to itself. SIGSTOP rejected: under strace a self-stopped tracee shows 't (tracing stop)'/wchan ptrace_stop in signal-delivery-stop, group-stop and every traced syscall alike (observed on the host with strace --seccomp-bpf -f), and a SIGCONT during the signal-delivery-stop would leave it stopped for good (kernel reading). Rationale in docs/testing.md.
- Driver: diskgc-e2e/src/driver.rs + bin diskgc-e2e-driver (interlock first). Operations generic over H: Delete: walk DIR, delete DIR (rm -r, sorted names, statx right before each unlinkat, line reports the checked ino). Output: indexed lines 'I <type> <path>' / 'I unlinked <path> ino=N' / 'I err <tag> <path> -- msg', control lines, final 'done calls=N'. New ops go into driver::run. Deletes only via Delete::unlinkat (LinuxHost: sandbox gate); no raw syscall, no permit.
- Harness: under_test::spawn_as_tester -> Spawned {wait_paused, resume, wait}; run_as_tester is spawn+wait. Ledger entry and Run::started before spawn; STRACE_OPTIONS unchanged. wait_paused checks PPid == strace pid, state S, /proc/<pid>/syscall 'read(0)'. Run gained 'signal'. Scratch::rename added (checked op).
- Container tests (fast): driver_walk_sees_a_change_made_while_paused, driver_delete_pauses_and_resumes_at_a_host_call, driver_delete_killed_at_a_host_call (absolute K calibrated by a paused run on an identical tree; oracle eligible = exactly the entries before K), driver_delete_unlinks_what_replaced_an_entry_while_paused (the accepted identity-check-to-unlink window, hit at an exact call). All judged by the oracle.
- Release check: flake check release-excludes-e2e runs diskgc-e2e-release-check on the shipped diskgc binary (needles: 'diskgc_e2e', v0-mangled '10Controlled', the control prefix, the driver's usage line), on cargo tree --package diskgc --edges normal,build (releaseDepTree), and on the driver binary as positive control (must contain every needle); also requires a symbol table in the shipped binary and diskgc-linux in the tree.
- Bites (mutate, run, restore): pause that does not block -> all 4 driver tests FAIL (process ran on and exited); kill not delivered -> killed test FAIL (exit 0, victim/c,d deleted); wrapper acting after the call (read_dir, unlinkat) -> all 4 FAIL for their own reason; marker string in diskgc main -> release check FAIL; controlled.rs copied into crates/diskgc and wrapping LinuxHost -> release check FAIL on '10Controlled' and the prefix; diskgc depending on diskgc-e2e -> check FAIL on the dep tree (in nix the build itself fails first: diskgc-e2e is outside workspaceSrc); read_dir not counted -> 2 wrapper unit tests FAIL; stale needle (first guess Controlled$LT$, legacy mangling) caught by the positive control.
- Limits: positions count port calls, not syscalls (no stop inside resolve or a /proc reader's rounds); the driver's delete is a stand-in, not the engine; positions are reproducible only because the driver sorts names; the source scan does not cover diskgc-e2e, so 'deletes only through Delete' rests on review; the release check is byte-needle based (a renamed and reworded copy of the wrapper in the product would pass it; review is the other line).
<!-- SECTION:NOTES:END -->
