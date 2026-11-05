---
id: TASK-344
title: >-
  Command output drain edges: signal late by up to 1 s, suspend during flush,
  uncounted partial line on the error exit
status: To Do
assignee: []
created_date: '2026-11-05T01:12:00Z'
labels:
  - command-actions
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of TASK-342 (2026-11-05), each reproduced by Codex in an isolated harness: (1) pre-existing, host.rs finish_command_output: if the leader exits but a descendant holds a pipe open, SIGTERM/SIGINT during the drain is noticed only after the 1 s limit, so termination and its grace start late; fix: check interruption every drain iteration. (2) host.rs start_command_permitted -> command_output.rs Forwarding::finish: the tier deadline is CLOCK_BOOTTIME but the flush converts its budget to an Instant (excludes suspend); a suspend during a blocked flush can leave ~1 s of wait after resume past the tier deadline; fix: also check the absolute host-clock deadline each iteration, keep the 50 ms tail. (3) host.rs drive_command_output / finish_command_output: 'sleep 30 & printf partial; exit 0': the drain errors after 1 s and drops the buffered partial OutputLine without queuing or counting it (lines_not_forwarded: 0), contrary to 'never under-counted'; fix: flush or count both OutputLine buffers on every error exit. None affects deletion or the action result.
<!-- SECTION:DESCRIPTION:END -->
