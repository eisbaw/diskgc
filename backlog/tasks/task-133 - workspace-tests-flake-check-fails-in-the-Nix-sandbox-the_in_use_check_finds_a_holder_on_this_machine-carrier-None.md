---
id: TASK-133
title: >-
  workspace-tests flake check fails in the Nix sandbox:
  the_in_use_check_finds_a_holder_on_this_machine (carrier None)
status: To Do
assignee: []
created_date: '2026-10-24 15:23'
labels:
  - testing
  - ci
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by qa-test-runner during TASK-115 (2026-10-24). nix build .#checks.x86_64-linux.workspace-tests exits 1 on master 3dfad95 and on ebdffae alike, deterministically (3/3): host::tests::the_in_use_check_finds_a_holder_on_this_machine asserts table.carrier(holder.pid().0) == Some(holder.pid()) and gets None; it prints namespace Ok(PidNamespace { initial: false, hidepid: false }), i.e. the Nix build sandbox's own PID namespace. Passes in the dev shell (gnu and musl). Consequences: just ci / nix flake check is red on master; the check runs cargo test without --no-fail-fast, so diskgc-model and diskgc-core integration binaries never run in the sandbox. Root cause not investigated (likely the test assumes the initial PID namespace for Owner PID's carrier).
<!-- SECTION:DESCRIPTION:END -->
