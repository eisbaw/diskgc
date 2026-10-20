---
id: TASK-95
title: >-
  System profiles under /var: journald vacuum and systemd coredumps (root,
  command actions)
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
updated_date: '2026-10-20 02:47'
labels:
  - library
dependencies:
  - TASK-37
  - TASK-21
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: /var holds regenerable system cruft diskgc does not cover: the systemd journal (/var/log/journal) and core dumps (/var/lib/systemd/coredump). Both are root-owned, so they need --allow-root (task-21) and the tool's own commands (journalctl --vacuum-size/--vacuum-time, coredumpctl or the coredump.conf limits) rather than plain deletes. Decide which belong in the library, their safe defaults, and how freed space is reported.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Profiles with examples run the tools' own vacuum commands, only as root with --allow-root; never plain deletes of journal files
- [ ] #2 Verified in a container (journald and coredumps present) or read-only on a real machine
- [ ] #3 Docs say what is kept (e.g. the current boot's journal) and why
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 feasibility: mainline `Config::validate_mode` explicitly refuses any selected command action under `--allow-root` (`command actions are refused under --allow-root`), matching docs/safety.md root policy: a trusted cleaner launched in a user-controlled directory can load user code/config. journald uses 4,302,761,984 B and systemd coredumps 4,269,649,920 B on this host (read-only du/journalctl query). A YAML command profile for journalctl/coredumpctl would fail static validation. Needs a dedicated root-safe system kind/privileged command protocol and isolated systemd fixture; do not relax the generic command restriction merely to make the profile load. No host files changed or vacuumed.
<!-- SECTION:NOTES:END -->
