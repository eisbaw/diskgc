---
id: TASK-119
title: 'Design: systemd unit and NixOS module options; fix systemd-analyze doc drift'
status: Done
assignee: []
created_date: 2026-10-23 16:19
updated_date: '2026-10-31T02:37:00Z'
labels:
  - design
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. No systemd unit, no NixOS module and no nixosModules flake output exist although architecture.md Deployment names them and Implementation lists nixosModules.default. docs/testing.md (~L1319) claims just check runs systemd-analyze verify; the justfile has no such step. Design: template unit instance naming, ExecStart, Restart, sandboxing options compatible with /proc and pidfd needs, user vs system service; NixOS module option schema and instance model. Fix or remove the testing.md claim.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Unit and module designs in docs/architecture.md Deployment with option names and instance model
- [x] #2 testing.md systemd-analyze claim matches the justfile (recipe added or claim removed)
- [x] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): Default shape: USER SERVICE BY DEFAULT (diskgc watch under systemd --user for the laptop owner: sees own processes, non-root), plus a SYSTEM TEMPLATE UNIT (instance per filesystem) as an option for CI runners and --allow-root use. The NixOS module must model both. Sandboxing options must not break /proc and pidfd needs; state which hardening is compatible.

Mark's decisions 2026-10-25 (his own) on the watch design: the systemd unit must use Restart=always and RestartPreventExitStatus=2 (SIGHUP = restart with fresh config).

2026-10-31 design written on branch t119-deploy-design (worktree tmp/worktrees/t31), commit cd3b7f4, not merged. architecture.md Deployment: diskgc-watch@.service in both managers (user default, system template for CI and --allow-root). Instance = systemd-escape --path PATH (%f). Flags in an optional %E/diskgc/watch/%i.env. Restart=always, RestartPreventExitStatus=2, RestartSec=30s, KillMode=mixed. Hardening rule: no namespace of diskgc's own, with a verdict table. Module option schema; CI tally-only example; 11 choices to confirm. testing.md: the just check systemd-analyze claim is removed (AC2), and a planned Deployment units test section is added (VM test deferred). Evidence: unit text extracted from the doc passed systemd-analyze verify (system and --user) on systemd 260; check_docs exit 0; just e2e not run (docs-only, no builds allowed). AC3 (mped-architect GO) still open.

2026-10-31 mped-architect review of cd3b7f4: NO-GO, with blocker B1 (CAP_SYS_PTRACE mandatory but not enforced). The fixes are in commit 297c3d4 on t119-deploy-design. B1 is now TASK-44 AC #4: --allow-root exits 2 at start without CAP_SYS_PTRACE in CapEff. TASK-44 AC #5 covers the install steps. S1-S5 and all nits are addressed in text. The reviewer accepts all 11 choices to confirm with these adjustments: module typed options only where they shape the unit, with extraArgs for the rest; a linger warning; RestrictSUIDSGID=no explicit; four capabilities without CAP_DAC_READ_SEARCH; no After=local-fs.target; the VM test in e2e-full. GO is pending the coordinator's read of 297c3d4. AC3 stays open until that GO is recorded.

Correction to the previous note: the review-fix commit is 070d018, not 297c3d4.

2026-10-31: GO by orchestrator after the mped review fixes (8fd62c8); merged ab4aa26. 11 choices accepted by the reviewer with adjustments; listed for Mark's confirmation (TASK-44/45 implement them).
<!-- SECTION:NOTES:END -->
