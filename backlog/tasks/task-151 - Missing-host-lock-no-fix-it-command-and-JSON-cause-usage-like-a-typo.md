---
id: TASK-151
title: 'Missing host lock: no fix-it command, and JSON cause ''usage'' like a typo'
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 00:54'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/32-no-host-lock.txt; /run/diskgc moved aside, i.e. tmpfiles not applied). The error prints the two tmpfiles lines but not how to apply them now (e.g. 'sudo systemd-tmpfiles --create /path/to/diskgc.conf', and where the package installed the drop-in: lib/tmpfiles.d/diskgc.conf). Under --json the error has cause 'usage' and status usage_error, the same as a mistyped flag, so a harness cannot tell 'the host is not provisioned' (alert ops) from 'my call is wrong' without parsing message text. reference.md documents exit 2/usage_error for it; the cause could be 'host' (contract question, CLI surface is irreversible: decide before 0.1).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 docs-setup branch (53cc311): decision (mark-emulator for Mark): cause `provisioning` (fault-class name, matches the exit-code table's 'provisioning error'), for a missing or unusable host lock; status usage_error, exit 2 unchanged. Message prints exact fix-it commands (drop-in written out via printf|tee to /etc/tmpfiles.d + systemd-tmpfiles --create; mkdir/touch/chmod fallback). Walked: tmp/walks/2026-10-25-docs-setup/10-no-host-lock-human.txt, 11-no-host-lock-json.txt, 12-fix-it-commands-applied.txt (both command forms produce the packaged drop-in's result). Vector exit/missing-host-lock-file + e2e ensure_without_the_host_lock_file_exits_2_printing_the_tmpfiles_lines (passed via just e2e-one).

Merged to master as d2c0554 on 2026-10-25T00:54Z (branch docs-setup, rebased; just check exit 0). Orchestrator accepted two deviations: JSON cause named 'provisioning' (fault class, matches exit-2 wording); 'check' warns on a missing supplied root with exit 0 (check is machine-independent; build-time checks), while ensure exits 2.
<!-- SECTION:NOTES:END -->
