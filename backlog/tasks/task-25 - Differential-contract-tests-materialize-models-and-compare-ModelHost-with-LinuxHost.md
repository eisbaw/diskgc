---
id: TASK-25
title: >-
  Differential contract tests: materialize models and compare ModelHost with
  LinuxHost
status: In Progress
assignee:
  - '@codex'
created_date: '2026-10-16 22:41'
updated_date: '2026-10-21 23:47'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-24
references:
  - docs/testing.md#4-differential-contract-tests
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the model is only useful if it behaves like the kernel. Implement materialize(model) on the scratch tmpfs (files, owners via subuid, bind/stacked mounts, symlinks, helper processes holding locks/files) and run the same scenarios on both hosts; mark model-only fields.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Model-only fields are rejected by materialize with a clear message
- [ ] #2 Outcomes and the deletion-oracle diff match between ModelHost and LinuxHost for all fixtures and 100 generated models per run (call traces are not compared: the Host trait is tentative)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-14: model-only fields and behaviours to reject or skip in materialize: statx_missing, kernel, clock/elapsed and advance events, size/other_used exact statvfs, proc: denied|failed, ext4/nfs/btrfs fs types. ModelHost answers an empty fd table/maps/environ as Ok(empty) where LinuxHost reports Failed, and its link text is the open-time path; compare outcomes, not texts.

2026-10-22 partial implementation: model_materialize.rs preflights kernel/hostname/own-process/gate/scripted-command/events/processes/locks, statx_missing, other_used, unsupported mounts, and unsupported node attributes before fixture writes; it creates representable files, directories, symlinks, FIFOs, sockets and owners on disposable scratch tmpfs. differential_suite.rs registers a Fast container test that replays the checked-in Python pycache vector against ModelHost and the real diskgc/LinuxHost, comparing dry plan, reclaim exit/actions/skips, and disappeared fixture paths; Oracle independently judges all traced runs. Evidence: cargo check e2e passed; e2e crate Clippy all-targets passed; model-only rejection unit test 1/1 passed; nix develop -c just e2e-one python_vector_matches_linux_outcomes_and_deletion_diff passed 1/1 in Podman with host canary refusal exit 86. Full AC is OPEN: only one checked-in vector is compared, not all fixtures; no 100 generated models; no process/lock helpers, bind/stacked mounts, exact statvfs/time behavior. Nominal mount size/device/id and required clock are not replicated and this differential case uses an unreachable 100% goal that does not depend on them. Keep task In Progress until these gaps are addressed and full gates pass.

2026-10-22 review follow-up: fixed dangling-symlink deletion diff on both hosts. Linux uses symlink_metadata, counts absent only on NotFound and propagates other I/O failures; ModelHost uses fd-relative statx because stat_path refuses final symlinks. The Python container fixture now includes a dangling symlink that must survive. A checked-in ProfileVector::check runs before host comparison; a unit regression removes both cache candidates and proves the independent expected plan rejects vacuous agreement. Linux explicitly verifies expected removed/preserved path postconditions with lstat semantics. Materializer now sets explicit mtimes after all fixture writes and creates valid CACHEDIR.TAG content. Focused Podman e2e PASS on Python, Bun, and mypy vectors; Bun covers old mtime, mypy covers tag; host canary refused exit 86. E2E crate Clippy all-targets PASS, vacuous-agreement unit PASS. Classification unit test triaged all 30 checked-in profile vectors after adapting the nominal /home mount: 23 structurally materializable (autotools, bun, cargo-target, chromium-tmp, claude-code-tmp, claude-code-versions, cmake, codex-releases, cursor-agent-versions, cursor-extensions, go-mod-cache, meson, mypy, nix-user-cache, node-compile-cache, nested node-compile-cache, playwright-tmp, pytest-cache, python-pycache-checkout, renode-tmp, west-build, xdg-cache-entry-v2, zephyr-tmp); 7 explicit model-only (Brave/Chrome/Chromium/Edge browser cache fixtures use /home/clear outside scratch; containers-image-age, pytest-tmp-unsafe-live-no-fd, python-pycache-in-use require process helpers). Classification is only structural, not differential proof for the other 20. TASK-25 stays In Progress: all fixtures and 100 generated models/run not yet done, and process/lock, bind/stacked mount behavior remains unsupported.

2026-10-22 TASK-25 increment: registered all 18 checked-in profile vectors that replay with equivalent outcomes on fresh tmpfs. Each container case calls the independent ProfileVector::check, runs ModelHost and the real binary/LinuxHost for dry-run and reclaim, compares sorted plan/exit/actions/skips/disappeared entries, verifies required removed/preserved objects, and has the strace deletion oracle judge both runs. All 18 focused Podman cases PASS, with the host interlock canary refusing exit 86. A new Fast container test generates 100 deterministic disjoint Python-cache models per run with varied file sizes, an extra eligible cache, checkout-protected cache, and decoy paths; 100/100 independent expectations and LinuxHost differential replays PASS in 7.38 s. Pure differential unit tests 3/3 PASS; nix develop -c just check PASS; e2e-crate Clippy all-targets warnings denied PASS. Twelve of 30 checked-in vectors remain model-only for this materializer: 5 (autotools, CMake, Meson, West, XDG cache) require historical ctime for idle_for, which fresh kernel tmpfs cannot create by setting mtime; 4 browser-cache fixtures use /home/clear outside scratch; 3 require process/lock helpers (containers-image-age, pytest-tmp-unsafe-live-no-fd, python-pycache-in-use). This generator covers only the Python cache profile. Bind/stacked mount and process/lock replay are still unsupported. AC #1/#2 and DoD stay open until the contract and all fixture classes are resolved; no parity is claimed for model-only vectors.

2026-10-22 TASK-25 process increment: materialize(model) now accepts a narrow, preflight-validated readable tester process with one existing named open file, kernel-selected fd, optional scratch cwd, and ordinary tester credentials. It rejects locks and unsupported process exe/maps/environ, root, name, credentials, namespace, dumpability, privileged state, deleted or numbered fds with contextual model-only errors before writing the fixture. A Holder lifetime guard keeps the helper alive, verifies its actual /proc/PID/fd symlink points at the promised file after setup, after dry-run, and after reclaim, then closes stdin and requires a clean exit; early errors drop and reap it. New Fast Podman differential test projects the checked-in python-pycache-in-use vector onto this supported process shape, retains its independent expected in_use plan/skip and preserved pyc, and compares ModelHost with LinuxHost plus deletion oracle. Evidence: projected vector pure expectation PASS; new focused Podman E2E PASS, base Python differential regression PASS, e2e materializer unit tests 2/2 PASS, e2e Clippy all targets with warnings denied PASS. Original in-use YAML still contains a nominal /usr/bin/python exe/maps/environ and remains model-only without the projection. Lock replay, historical ctime, browser paths outside scratch, other process forms, bind and stacked mounts remain open; TASK-25 stays In Progress and AC unchecked.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
