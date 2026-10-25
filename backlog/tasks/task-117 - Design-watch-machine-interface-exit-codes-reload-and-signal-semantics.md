---
id: TASK-117
title: 'Design: watch machine interface, exit codes, reload and signal semantics'
status: Done
assignee: []
created_date: '2026-10-23 16:18'
updated_date: '2026-10-25 20:16'
labels:
  - irreversible
  - design
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/reference.md 'watch' is planned with intended syntax only. Missing: exit-code table, startup and fatal-error behaviour, whether config is re-read (SIGHUP is not mentioned anywhere in docs/), structured per-reclaim output (PRD: every action produces one log line and one JSON actions entry; watch today logs to stderr only), one-process-per-filesystem rule, snapshot write on signal exit, signal during tally, --tally-only semantics (single-writer lock? snapshot cadence? CI job lifecycle), back-off classification of mixed outcomes and whether --interval sampling continues during back-off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/reference.md watch section specifies exit codes, --json/event output, reload/SIGHUP, startup/fatal behaviour, --tally-only semantics
- [ ] #2 docs/testing.md names watch scenarios incl. the PRD 'steady between thresholds => zero deletions, duty cycle kept' test
- [ ] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): (1) Machine output: with --json, watch emits JSON LINES ON STDOUT, ONE OBJECT PER RECLAIM CYCLE, shaped like the ensure report; the human log stays on stderr. (2) Config reload: RESTART ONLY, NO RELOAD - SIGHUP is not a reload; systemd restart is the mechanism (keep SIGHUP semantics simple: document whether it is ignored or terminates like SIGTERM, pick one). (3) Fatal policy: KEEP RUNNING WITH BACK-OFF for environment errors (backend down, lock held, etc.), logged and emitted in JSON, back-off capped; exit non-zero only for invalid startup config or internal errors.

Design settled in 2d6c1ba (branch watch-design, not merged): docs/reference.md watch section (flags, start, cycle, back-off classes, JSON lines, log, signals, exit codes, tally-only, beside ensure), architecture.md pause probe/seed/unit restart requirement, testing.md watch scenarios. mped-architect review: FIX FIRST; blockers 1-4 (vanished root, exit paths vs decision 3, exit_code null contract, pause-probe vs tests) fixed in that commit; non-blockers 5-7 also addressed. Open for Mark: stdout write failure exits 3 (exception to keep-running); tally-only optional --min-free; SIGHUP terminate relies on Restart=always (TASK-119).

Mark's decisions 2026-10-25 (his own) on the watch design: (1) a failed stdout write under --json exits 3 (supervisor restarts). (2) --tally-only: optional --min-free is the floor for snapshot writes; without it only a failed write is skipped. (3) SIGHUP terminates like SIGTERM (exit 0); the shipped unit must use Restart=always (TASK-119). All three as documented in 2d6c1ba.

Merged to master as 3c6ff0f.
<!-- SECTION:NOTES:END -->
