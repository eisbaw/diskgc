---
id: TASK-342
title: >-
  Command actions fail when one forwarded stderr line takes >50 ms: the 0.2 Nix
  GC stopped after 17.2 GiB with exit 3
status: To Do
assignee: []
created_date: '2026-11-04T19:03:00Z'
labels:
  - release-blocker
  - nix
  - command-actions
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Dogfood 2026-11-04, release-candidate 0.2.0-dev (539f8d5, /nix/store/swnd9wskkv8df6qc7vqs4251raknz958-…), non-root, ext4 on LUKS: ensure --json --timeout 2h --min-free 200G --select profile:nix-garbage /nix/store, stdout and stderr redirected to regular files under tmp/walks/2026-11-04-dogfood/ (a1-rel-real.{json,err}). After 147 s: 'forward command output …/nix-store: stderr is not accepting output without blocking', status error, exit 3; avail +17.2 GiB of ~62-68 GiB dead (narSize). No nix-store left running.

Cause (code read, crates/diskgc-linux/src/host.rs write_command_stderr / command_output_writer, since adcbc3f5 2026-10-20): every child stderr line goes through a rendezvous channel (sync_channel(0)) to one writer thread with a COMMAND_STDERR_BACKPRESSURE of 50 ms per line; exceeding it fails the action. nix-store --gc prints one 'deleting …' line per path (~19k here); a single write to the stderr file stalled >50 ms while the same filesystem was unlinking GiBs (journal), so the GC was failed mid-way. Not yet reproduced in a test; the stall hypothesis is unconfirmed.

Same path serves every command action (docker/podman prune, the fs command action), so any chatty child on a slow stderr (file on a busy disk, a slow terminal, journald under load) can fail a reclaim.

Do: decide the policy so that a slow or blocked stderr never fails or hangs an action: e.g. keep draining the child's pipes always, forward lossily with a bounded buffer and a 'N lines of command output dropped' note, never fail the action for diskgc's own log stalls (the action result comes from the child's exit status and the measured change). Reproduce first (a model/Host test with a stderr writer that stalls >50 ms, and an e2e with a chatty command action and stderr to a slow pipe), prove the fix bites, re-run the Nix GC e2e. Check: does a failed forward kill or orphan the child (here it finished/was gone)?
<!-- SECTION:DESCRIPTION:END -->
