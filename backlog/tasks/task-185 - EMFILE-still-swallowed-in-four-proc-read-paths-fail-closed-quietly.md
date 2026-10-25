---
id: TASK-185
title: EMFILE still swallowed in four /proc read paths (fail closed quietly)
status: Done
assignee: []
created_date: '2026-10-25 05:34'
updated_date: '2026-10-25 12:45'
labels:
  - hardening
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 review: proc_root_lookup's /proc/<pid>/root open, the uid_map read in proc_status, diskgc's own status read (.ok()), and pid_table's pid-namespace reads turn EMFILE into a plain error and fail closed quietly. Reviewer's fix: an Exhausted variant of ProcError so every match must handle it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. ProcError::Exhausted (from_host, ProcError::failed). Fixed: uid_map (uid_map_of), process-root open + post-open statx/climb (root_unopened, errno_erased), own status (.ok()), pid_namespace before/after and NSpid (pid_table returns Result), socket query, and from review: Owner PID pidfd probe (Probe = Result<bool, HostError>, owner_pid -> Result) and pytest P listing. Evidence: model EMFILE cases for own status, pid_namespace x2, proc_nspid, bound_unix_sockets (all SWALLOWED on master), vector nix-shell-rc-probe-out-of-descriptors-is-a-run-error (master producer_unknown, branch exit 3), linux unit test out_of_descriptors_is_kept_apart_in_every_errno_erasing_reader. Container inuse_running_out_of_descriptors_... PASS. Host walk ulimit -n 64 dry run: exit 3, cause host, advice text. Not fixed (follow-up): resolve_in/open_beneath callers that turn a ResolveError::Host EMFILE into a per-candidate unsafe_path (app lock parent, contract_veto parent; ~20 resolve call sites).

Merged to master as ee643a6 on 2026-10-25T12:45Z.
<!-- SECTION:NOTES:END -->
