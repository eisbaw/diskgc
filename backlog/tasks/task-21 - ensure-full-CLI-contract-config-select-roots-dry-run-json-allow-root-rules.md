---
id: TASK-21
title: >-
  ensure: full CLI contract (config, --select/--roots, --dry-run, --json,
  --allow-root rules)
status: In Progress
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-20 09:15'
labels:
  - wave-2
  - cli
dependencies:
  - TASK-20
  - TASK-54
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: CI hooks and harnesses depend on the exact flags and exit codes. Complete ensure per docs/reference.md#ensure: config groups, --select/--roots validity rules, --dry-run plan, --json one object always, --allow-root (owners required, command actions refused), size and duration syntax, mount printed in every report.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every flag and rule in docs/reference.md#ensure has a test
- [ ] #2 --dry-run deletes nothing and takes no lock (deletion oracle + lock probe)
- [ ] #3 Bare numbers and month durations are usage errors
- [x] #4 --allow-root: owners required and resolving to non-system users; the config file and every directory above it root-owned and not writable by others; command actions refused
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CLI baseline from TASK10: parse_ensure accepts --flag value and --flag=value; repeated scalar flags, unknown flags, relative PATH, empty or relative config paths, bare sizes/durations and month durations are usage errors. --json is detected before parsing, so usage failures still emit one object. --roots requires --select; --timeout 0 removes the deadline. Unreadable explicitly selected config is an error, never a fallback. Config symlinks are allowed if protected in root mode. A partial deletion appears in actions and skipped/errors. Check now performs static validation; ensure without --select still lacks config-group execution (TASK20), protect enforcement and ipc live-only remain refused. Full flag-rule sweep and remaining TASK21 criteria are open.

2026-10-20 root mode implementation: --allow-root requires a loaded owners: policy. getpwnam_r resolves names at invocation; unknown names, uid <1000 and duplicate resolved UIDs fail before planning. Static musl resolves local passwd users; NSS-only LDAP/sssd requires TASK91. The opened config inode, every original path prefix, and every canonical symlink-target ancestor must be root-owned and not group/other writable. Resolved owner IDs feed the ModelHost-tested eligibility walk for plan and reclaim. Under root mode, process inspection also includes uid 0, without admitting root-owned candidate entries. Generic command and Docker prune actions are refused because they cannot honor owners:. Future Action enum variants require an exhaustive policy decision.

Focused ModelHost ownership and unreadable-root-process tests passed; disabling root-process inclusion made the latter fail, then restoring passed. Disposable Nix container tests passed for positive root-owned symlink policy, root holder in_use then reclaim, and refusal of writable file/ancestor, unsafe origin/target symlinks, unknown/system owner, generic command and Docker prune. Deletion oracle was clean. Full just e2e and QA/MPED integration gates remain pending; do not mark Done. TASK95 still needs a dedicated root-safe system command protocol.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
