---
id: TASK-52
title: >-
  CI pipeline: runners, Nix binary cache, e2e-full on push, nightly soak, fuzz
  and Cargo canary
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 07:04'
labels:
  - wave-1
  - tooling
dependencies:
  - TASK-2
references:
  - docs/testing.md#commands
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: docs/testing.md relies on CI for e2e-full, nightly soak and fuzz, the outside-container interlock canary and a latest-stable Cargo canary. Choose the CI provider and runner setup (rootless Podman with subuid/newuidmap, or a privileged runner for nested containers), a Nix binary cache, and the scheduled jobs; record the decision in docs/architecture.md#deployment.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 just e2e-full runs on every push and is green on the scaffold
- [ ] #2 A nightly job runs soak, fuzz and the Cargo race tests against the latest stable Cargo (canary), reporting failures without blocking pushes
- [ ] #3 The interlock canary (a do-nothing binary) is run outside a container in CI and must refuse
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: (1) 'just ci' = nix flake check -L, then just e2e-full. Runners need Nix >= 2.31.2 (crane 0.24.0) and rootless podman. (2) Until task-2/13/24/27 land, e2e-full, soak, fuzz and mutants print 'NOT RUN ...' to stderr and exit 0. CI must not report them as coverage. Consider grepping CI logs for 'NOT RUN'. (3) The flake supports x86_64-linux only.

Forward-carried from task-2: (1) The NOT RUN placeholders for e2e and e2e-full are gone. just e2e = check + nix build .#checks.x86_64-linux.e2e-tests + the fast container suite (5-minute budget); just e2e-full = check + all tiers (1-hour hang guard). (2) Runner hosts need rootless Podman with a subuid range covering uids 1000 and 1001. Containers run with --cap-add SYS_ADMIN --cap-add SYS_PTRACE --network=none --pull=never. (3) AC3 (interlock canary refuses in CI) holds for every CI run of just e2e or e2e-full. The runner executes diskgc-e2e-canary on the host first and stops unless it exits 86. Log line: 'interlock canary refused to run on the host (exit 86), as it must'. On a containerised runner /run/.containerenv may exist, but the image marker does not, so the canary still refuses. (4) Per-test logs go to target/e2e-logs/<test>.log; archive them as CI artifacts.

From task-3 round 10: the diskgc-linux unit tests the_host_is_refused_under_another_namespaces_proc and the_host_is_refused_under_an_ancestors_proc_with_an_equal_pid need unprivileged user namespaces on the runner (user.max_user_namespaces > 0, kernel.unprivileged_userns_clone = 1 where it exists, no AppArmor restrict_unprivileged_userns, e.g. on Ubuntu 24.04 runners), and inside them unsharing pid and mount namespaces, mounting proc and writing kernel.ns_last_pid. They fail, naming that precondition, where it is missing; they do not skip. The nix build sandbox on the development machine allows all of it.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
