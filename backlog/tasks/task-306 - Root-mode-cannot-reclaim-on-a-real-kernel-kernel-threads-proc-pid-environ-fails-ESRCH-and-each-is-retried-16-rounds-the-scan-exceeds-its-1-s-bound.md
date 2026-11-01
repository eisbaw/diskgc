---
id: TASK-306
title: >-
  Root mode cannot reclaim on a real kernel: kernel threads' /proc/<pid>/environ
  fails ESRCH and each is retried 16 rounds, the scan exceeds its 1 s bound
status: To Do
assignee: []
created_date: '2026-11-01T20:57:00Z'
labels:
  - safety
  - inuse
  - bug
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-304 real-kernel VM test (vm-real-kernel, NixOS guest Linux 6.18.56; branch t304-vm-real-kernel a100ac4; logs tmp/worktrees/t304/tmp/t304-logs/run{A,B,C}.log, 3 identical runs). On 6.18 reading a kernel thread's /proc/<pid>/environ fails with 'No such process' (cat /proc/2/environ exits 1; maps and cmdline read fine). diskgc-linux tasks.rs read_through_live_task/collect treats it as a vanished task and retries 16 rounds x 2 ms polls per kernel thread (95 of them); a root scan of ~113 processes takes 6.3-7.7 s, past the 1 s freshness bound, so every candidate is in_use_unknown and root reclaims nothing. Repro (VM container, root): diskgc-development ensure --allow-root --json --config /work/cfg/s0-root.yaml --select profile:scratch-probe --min-free 100% /work/s0 -> exit 1, 'the scan of 112 processes took 6313 ms', 91 lines 'pid N (?): initial environment unreadable ... no trustworthy read in 16 rounds'. Affects every --allow-root run incl. the NixOS module's root instance; containers never showed it (no kernel threads). Fix: recognise kernel threads (PF_KTHREAD in /proc/<pid>/stat flags, or empty cmdline + ppid 2 + no mm) and skip their environment (they hold no user files' paths in an environment); a genuine ESRCH on a user task keeps today's retry. Prove with vm-real-kernel scenarios 2/3/6 as root.
<!-- SECTION:DESCRIPTION:END -->
