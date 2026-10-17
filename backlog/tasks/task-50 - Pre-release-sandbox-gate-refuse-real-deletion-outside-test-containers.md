---
id: TASK-50
title: 'Pre-release sandbox gate: refuse real deletion outside test containers'
status: In Progress
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 11:02'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-2
references:
  - docs/safety.md#pre-release-sandbox-gate
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: from the first deleting code on, developers and AI agents could delete real data on their own machines with a work-in-progress build. Implement the gate in docs/safety.md#pre-release-sandbox-gate inside LinuxHost's deleting primitives (unlink, rmdir, the delete walk, and spawning prune/GC/command children), so every entry point is covered, including wave-1 test drivers: they refuse unless /.diskgc-test-sandbox and /run/.containerenv both exist. Compiled in by default; only the v1 release package (one build flag, set by the release task) removes it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Calling a LinuxHost deleting primitive on a scratch directory outside a container returns the gate error and deletes nothing (test run outside the harness)
- [x] #2 Non-deleting primitives (stat, read, walk without delete) work outside a container
- [x] #3 The default build contains the gate; a test inspects the build configuration so the gate cannot be removed except by the release flag
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: diskgc_linux::sandbox already holds MARKER_PATH, CONTAINERENV_PATH and MARKER_CONTENT (include_bytes! of crates/diskgc-linux/src/sandbox-marker, which the flake copies byte for byte into the e2e base image) plus check() -> Result<(), NotInSandbox>. The e2e interlock (diskgc-e2e/src/interlock.rs) uses it. The gate must reuse it, not define the markers a second time. check_under(root) is private and unit-tested; expose it (pub(crate) or cfg(test)) if the gate's tests need a fake root. Pattern for 'refuses outside a container': the runner runs diskgc-e2e-canary on the host before every e2e run and requires exit 86.

Forward-carried from task-3: No deleting primitive exists yet. Add unlinkat/rmdir (and child spawning) as separate Host methods on LinuxHost and gate exactly those with diskgc_linux::sandbox::check(). The non-deleting primitives (openat2, statx, statvfs, flock, /proc readers) must keep working outside a container (AC #2). diskgc-linux's unit tests already exercise them on the dev machine read-only (a temp dir only for flock).

Forward-carried from task-14: the deleting operations are the trait diskgc_core::host::Delete (unlinkat so far), separate from Host. Gate LinuxHost's impl Delete; ModelHost (test-only, never in the binary) implements it without the gate since it deletes nothing real.

From mped's review of task-14: a gate refusal should be a distinct, non-errno HostError (e.g. errno None with a detail naming the sandbox gate and which marker is missing), so callers and tests can tell it apart from a kernel refusal.

Implementation (staged, not committed; awaiting the deep review):
- Gate: diskgc_linux::sandbox::permit() returns a Permit (private field, so only sandbox.rs can make one) or the reason. It is decided once per process with a OnceLock over sandbox::check() and keeps a refusal too, because the markers do not change during a run and per-unlink checks would add two file reads per entry. With the cargo feature release-without-sandbox-gate it always grants.
- LinuxHost implements Delete::unlinkat (host.rs). Order: gate(op, subject), then the single-name check (diskgc_core::host::is_single_name, now shared with ModelHost), then unlinkat_permitted(&Permit, ..), the one rustix::fs::unlinkat in the crate.
- Port change: HostError.errno: Option<Errno> became cause: Cause {Errno(Errno), Invalid, SandboxGate}, with kernel_errno() and is(). A gate refusal therefore cannot be confused with a kernel errno or with Invalid, and inconsistent states cannot be built. Call sites updated (core resolve.rs and ProcError::from_host; model tests).
- Flag: the cargo feature release-without-sandbox-gate is defined in crates/diskgc-linux/Cargo.toml and enabled nowhere.
Tests (bite = mutation, run, restore, from staged copies):
- AC1+AC2: diskgc-linux host::tests::deleting_outside_a_container_is_refused_by_the_gate. On the dev machine and in the Nix sandbox, unlinkat of its own temp file and empty dir gives Cause::SandboxGate, errno None, and a detail naming the gate and the missing marker. Both entries stay. statx, openat2, statvfs and mountinfo work afterwards. Bite: permit() always granting fails it (and deletes the test's own temp file); building with the feature fails it.
- Structure: host::tests::deleting_syscalls_only_behind_the_gate scans every src/*.rs of diskgc-linux, listed from the directory, outside its tests. It blanks out comments and strings and requires deleting syscalls (unlink*, rmdir, rename*, remove_*) and child starts (Command, spawn, fork, exec*) to appear only in unlinkat_permitted, with exactly one call seen there as a positive control. Bites: a rustix unlinkat in statvfs fails it; a Command::new in kernel_release fails it; Permit(()) in host.rs does not compile (E0423).
- AC3: sandbox::tests::the_gate_is_compiled_into_this_build (cfg!) and sandbox::tests::sandbox_gate_flag_is_enabled_nowhere. The flag must be defined exactly once and not in default. No workspace manifest, diskgc-e2e/Cargo.toml, flake.nix, justfile or .cargo config may name it or use all-features. In a git checkout those files must exist; the flake's filtered source lacks flake.nix, the justfile and the e2e manifest, so there 'just check' covers them. Bites: default feature, a features= on diskgc's dependency, --all-features in the justfile, --features in the flake's cargoExtraArgs and features= in the e2e manifest each fail it. cargo test --features release-without-sandbox-gate fails the cfg test and the AC1 test.
- Container: diskgc-e2e host_unlinkat_deletes_inside_the_sandbox (fast tier), with the new probe host-unlink. Inside the sandbox it deletes a file and an empty dir; a non-empty dir gives ENOTEMPTY (a kernel errno, not the gate); the sibling and inner file stay. Two oracles judge the same run: one with the two entries eligible is clean, and one with nothing eligible flags exactly those two in both the snapshot and the strace view. Bites: a permit forced to refuse fails it (err sandbox_gate); a no-op unlink fails it.
- Wrong or missing markers: the existing sandbox::tests wrong_marker_content_refuses and missing_containerenv_refuses on fake roots. Bites: skipping the content comparison fails the first; dropping the ? on containerenv fails the second.
Gates: nix flake check -L exit 0; nix build --rebuild of workspace-tests, workspace-clippy, e2e-tests and e2e-clippy exit 0 (the gate tests are listed as run in the musl build); just check exit 0; just e2e twice exit 0 (21 of 21 each); just e2e-full exit 0 (22 of 22).
Limits: an accident guard, not a security boundary (root on the host can create both markers); a --features typed by hand outside the repo is not seen; the shipped diskgc binary links no deleting code yet (the marker path is absent from it), so no test inspects the binary, only the build configuration (see the task-49 note); walk-without-delete for AC2 does not exist in the port yet (no read_dir), and the readers that do exist are covered.
Notes added to tasks 5, 35, 36, 37, 49, 51, 54 and 61.

Review round 1 (mped GO conditional on S1-S3; Codex NO-GO on scan completeness) fixed, staged and not committed:
- S1: the scans moved into one test-only module, crates/diskgc-linux/src/source_scan.rs (declared #[cfg(test)] mod source_scan; in lib.rs). It blanks comments and literals with positions kept, and walks src/ recursively. It excludes test code by module: an inline #[cfg(test)] mod is skipped by brace matching and code after it is scanned; an out-of-line test module's file is excluded. Any other #[cfg(test)] is an error, so nothing is truncated. Block comments and raw strings (also br"..") are errors. procfs_is_reached_only_through_the_held_fd now uses the same scanner.
- S1b: GATED forbids, outside allowlisted wrappers: unlink*, rmdir, remove_*; rename, renameat, renameat_with; TRUNC, WRONLY, RDWR, CREATE, APPEND, TMPFILE, OpenOptions, File::create, fs::write, fs::copy, set_len, ftruncate, fallocate; Command, fork, vfork, execve*, posix_spawn; setns, unshare, chroot, pivot_root, move_into_*, rustix::mount; libc, syscall, include!. Grouped imports (std::fs::{write}) are resolved. Because no production code may name a write, create or truncate flag, ProcRoot::open can only receive read-only flags (doc comment added).
- S2: every crate in crates/ is scanned except the test-only diskgc-model and diskgc-modelgen, which test_only_crates_are_never_production_dependencies keeps out of every production manifest except under dev-dependencies. diskgc-e2e is excluded because it is test infrastructure outside the product build that deletes only inside containers behind its own interlock (stated in source_scan.rs and docs/safety.md).
- S3: corrections appended to tasks 35, 36, 37 and 54 (query run on Host, ungated, constant argv; deleting run on Delete, gated; command actions always gated; one allowlisted wrapper each). Notes added to task-30 (state writes: one ungated port operation relative to the state-dir fd, allowlisted) and task-51 (update). docs/architecture.md, The Host port, says which trait each run belongs to and where state writes live.
- Docs: docs/safety.md 'How it holds' no longer overclaims; it describes the new scan and adds limits (writable bind mount into the marked image, by hand or by harness; text-scan blind spots; the cached decision assumes no namespace or root change, which the scan forbids).
- H2: near_miss_marker_contents_refuse (no trailing newline, extra line, empty, same prefix with another UUID).
- H5: read_marker opens with O_NOFOLLOW|O_NONBLOCK, requires a regular file (new NotInSandbox::MarkerNotRegular) and reads at most N+1 bytes. Tests: a_symlinked_marker_refuses, a_fifo_marker_refuses_without_blocking (5 s bound), the_marker_read_is_bounded.
- Filed on task-61: H3 (ModelHost injects SandboxGate), H4 (gate status without a Permit, also on task-10), the structured reason from permit(), crates/*/.cargo/config.toml not scanned by the flag test.
Negative controls (in-memory or temp-tree scripted sources, all in source_scan.rs tests): every_gated_operation_is_caught (each GATED pattern has a case and each case matches exactly one pattern, so adding a pattern without a case or removing one fails), harmless_look_alikes_are_not_caught, code_after_an_inline_test_module_is_scanned, a_cfg_test_in_a_doc_comment_hides_nothing, a_test_only_item_that_is_not_a_module_is_refused, unreadable_syntax_is_an_error_not_a_guess, out_of_line_modules_nested_or_test_only_are_handled, the_walk_finds_nested_modules, only_the_allowed_function_may_name_a_gated_operation.
Bites (mutate, run, restore from staged copies; all failed as expected):
- Scanner: non-recursive walk fails the_walk_finds_nested_modules. Truncating at the test module fails code_after_an_inline_test_module_is_scanned. Not blanking comments fails the doc-comment control, the look-alike control and both real scans. Tolerating a non-module #[cfg(test)] fails a_test_only_item_that_is_not_a_module_is_refused. Not excluding the out-of-line test module file fails out_of_line_modules_nested_or_test_only_are_handled.
- S1b: dropping each of TRUNC, WRONLY, RDWR, CREATE, OpenOptions, File::create, fs::write, fs::copy, set_len, ftruncate, fallocate, rename, renameat, renameat_with, setns, chroot, pivot_root and unshare from GATED fails every_gated_operation_is_caught ("caught by [], expected exactly one pattern").
- Real sources: unlinkat in LinuxHost::statvfs, std::fs::write in crates/diskgc/src/main.rs (S2), OFlags::TRUNC passed to ProcRoot::open, an undeclared nested crates/diskgc-core/src/actions/mod.rs with remove_file, and #[cfg(test)] fn helper mid-file in diskgc-core host.rs each fail deleting_syscalls_only_behind_the_gate. Excluding the diskgc crate fails it and the manifest test. diskgc-model as a [dependencies] entry of crates/diskgc fails test_only_crates_are_never_production_dependencies.
- H2/H5: a trim_ascii compare and a starts_with compare each fail near_miss_marker_contents_refuse (starts_with also fails the bounded-read test). Dropping O_NOFOLLOW fails the symlink test. Dropping the regular-file check fails the FIFO test (wrong variant). Dropping O_NONBLOCK fails the FIFO test at its 5 s bound. An unbounded read fails the_marker_read_is_bounded.
Gates round 2: just check exit 0; nix flake check -L exit 0 (workspace 90 passed, 1 ignored, in diskgc-linux; the 11 source_scan tests ran in the musl build); nix build --rebuild of workspace-tests, workspace-clippy, e2e-tests and e2e-clippy exit 0 (90 and 34 re-run); just e2e twice exit 0 (21 of 21 each); just e2e-full exit 0 (22 of 22).

Review round 2 (Codex NO-GO on three ordinary-code gaps; mped GO with should-fix items) fixed, staged and not committed. All in crates/diskgc-linux/src/source_scan.rs unless noted.
- Codex 1 (std writes via constructors and aliases): File::options and File::create_new added to GATED (create_new is forbidden: it opens for writing, like every other writing open). hiding_forms refuses import renames (only `as _` allowed) and glob imports in production code, so std paths cannot be hidden.
- Codex 2 (source outside src/): hiding_forms refuses #[path]/#![path]. layout_problems refuses build.rs, a [package] build key, and a [lib]/[[bin]] path outside src/ or containing '..'. rust_files refuses symlinks under src/ (fixed here rather than filed).
- Codex 3 (dependency boundary): PRODUCTION_DEPENDENCIES pins, per production crate, the exact lines of [features] and every non-dev dependency section (target-specific, build, [dependencies.X] tables), with workspace entries resolved, rustix features included. source_overrides refuses [patch]/[replace]. Editing the list is marked as a safety review point.
- mped 1 (lexer): raw C strings cr".."/cr#".."# are now refused; the '\'' char-literal escape is fixed.
- mped 2: File::options added; File::create_new forbidden (decision above); E5 (#[cfg(test)] mod evil; plus #[path = "evil.rs"] mod evil_prod;) has a control; OFlags::from_bits, from_bits_retain and from_bits_truncate are forbidden.
- mped 3: os_access, a stricter rule for diskgc and diskgc-core (OS_FREE_CRATES): no std::fs, std::os::unix::fs, rustix or libc; from std::process only ExitCode. GATED still runs on every production crate.
- mped 4: Allowed entries carry gate: Gate::Permit | Gate::Ungated(reason). check_allowed requires each function to exist exactly once, a Permit entry's signature to name Permit, and an ungated entry to have a reason. The GATED_ALLOWED doc comment is fixed.
- mped 5: Command is now Path(process, Command), include is Macro (include!), syscall is CallOrMacro (syscall( or syscall!). A control proves that Action::Command and fields named include and syscall pass.
- mped 6: notes added on tasks 5, 37, 30, 35 and 36. mped 7: docs/safety.md 'How it holds' lists the new rules, and the limits say the scan is a tripwire against ordinary mistakes, not a proof, and name what it misses. The ProcRoot::open doc now says 'outside the allowlisted wrappers'. docs/architecture.md pins the query-run enum and the state-write shape. The syn-visitor idea is filed on task-61.
New controls: renames_globs_and_path_attributes_are_refused, a_build_script_or_outside_target_fails_the_layout, a_new_dependency_or_feature_fails_the_boundary (a plain, target-specific, build or table dependency; a workspace rustix feature; a dependency feature; a default feature; a patch section), os_access_in_core_or_cli_is_caught, an_allowlist_entry_marked_gated_must_take_a_permit, lexer_corner_cases_hide_nothing, a_path_module_reusing_a_test_module_file_is_caught, command_include_and_syscall_match_only_their_real_forms; the_walk_finds_nested_modules now also covers a symlinked directory; every_gated_operation_is_caught has cases for the new patterns.
New real tests: production_crates_compile_only_what_the_scan_reads, production_dependencies_are_the_reviewed_ones, core_and_cli_reach_the_os_only_through_the_port; check_allowed and hiding_forms run inside deleting_syscalls_only_behind_the_gate.
Bites (mutate, run, restore from staged copies; all failed as expected):
- Scanner: dropping File::options, create_new or from_bits_retain fails every_gated_operation_is_caught. Tolerating renames, globs or #[path] fails the hiding-forms control. Tolerating build.rs, a build key or a '..' target path fails the layout control. Pinning only plain [dependencies], or not resolving workspace entries, or ignoring [patch] fails the boundary control. Accepting cr"..", or the old char-escape step, fails the lexer control. Command, include or syscall as bare words fail the look-alike control. Not checking for a Permit fails the allowlist control. Allowing std::process or std::fs in core/CLI fails the os_access control. Walking past symlinks fails the walk control.
- Real sources: a `use std::fs as filesystem` in mountinfo.rs, a #[path] module in diskgc-core, and File::options truncation in LinuxHost each fail deleting_syscalls_only_behind_the_gate. A rustix "mount" feature in the workspace manifest fails production_dependencies_are_the_reviewed_ones. `use std::fs` in diskgc-core fails core_and_cli_reach_the_os_only_through_the_port. A build.rs in diskgc-core fails production_crates_compile_only_what_the_scan_reads.
Gates round 3: just check exit 0 (diskgc-linux 101 passed, 1 ignored); nix flake check -L exit 0 (the 22 source_scan tests ran in the musl build); nix build --rebuild of the four checks exit 0 (101 and 34 re-ran); just e2e twice exit 0 (21 of 21 each); just e2e-full exit 0 (22 of 22).
<!-- SECTION:NOTES:END -->
