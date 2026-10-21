---
id: TASK-21
title: >-
  ensure: full CLI contract (config, --select/--roots, --dry-run, --json,
  --allow-root rules)
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-21 11:59'
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
- [x] #1 Every flag and rule in docs/reference.md#ensure has a test
- [x] #2 --dry-run deletes nothing and takes no lock (deletion oracle + lock probe)
- [x] #3 Bare numbers and month durations are usage errors
- [x] #4 --allow-root: owners required and resolving to non-system users; the config file and every directory above it root-owned and not writable by others; command actions refused
- [x] #5 Under --allow-root, a selected or protected source containing {uid} is rejected before planning or deletion until per-authorized-owner expansion is defined for one and multiple owners; disposable-container tests cover both owner counts and protection
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CLI baseline from TASK10: parse_ensure accepts --flag value and --flag=value; repeated scalar flags, unknown flags, relative PATH, empty or relative config paths, bare sizes/durations and month durations are usage errors. --json is detected before parsing, so usage failures still emit one object. --roots requires --select; --timeout 0 removes the deadline. Unreadable explicitly selected config is an error, never a fallback. Config symlinks are allowed if protected in root mode. A partial deletion appears in actions and skipped/errors. Check now performs static validation; ensure without --select still lacks config-group execution (TASK20), protect enforcement and ipc live-only remain refused. Full flag-rule sweep and remaining TASK21 criteria are open.

2026-10-20 root mode implementation: --allow-root requires a loaded owners: policy. getpwnam_r resolves names at invocation; unknown names, uid <1000 and duplicate resolved UIDs fail before planning. Static musl resolves local passwd users; NSS-only LDAP/sssd requires TASK91. The opened config inode, every original path prefix, and every canonical symlink-target ancestor must be root-owned and not group/other writable. Resolved owner IDs feed the ModelHost-tested eligibility walk for plan and reclaim. Under root mode, process inspection also includes uid 0, without admitting root-owned candidate entries. Generic command and Docker prune actions are refused because they cannot honor owners:. Future Action enum variants require an exhaustive policy decision.

Focused ModelHost ownership and unreadable-root-process tests passed; disabling root-process inclusion made the latter fail, then restoring passed. Disposable Nix container tests passed for positive root-owned symlink policy, root holder in_use then reclaim, and refusal of writable file/ancestor, unsafe origin/target symlinks, unknown/system owner, generic command and Docker prune. Deletion oracle was clean. Full just e2e and QA/MPED integration gates remain pending; do not mark Done. TASK95 still needs a dedicated root-safe system command protocol.

Compass domain review: Tier::select currently receives process uid 0 in root mode, so {uid} in find roots, patterns, excludes or app_not_running silently expands to 0 instead of an authorized owner. Reject these policies before acting; TASK20/TASK16 must define eventual per-owner expansion and overlap/protect semantics.

Compass/MPED follow-up: --allow-root now rejects every executable {uid} substitution point before planning; literal-only where guards reject {uid} at config load for both sources and filters. Disposable-container cases cover one/two owners, nonempty protect policy, protected literal guard, empty plan and deletion oracle. Config group/protect execution remains deferred to TASK20/TASK16.

TASK91 is now Done: explicit {uid: N} owner mapping covers NSS-only accounts when the operator trusts stable numeric UID assignment. Earlier NSS dependency note is superseded.

2026-10-21 CLI acceptance probes integrated: fast disposable-container tests now exercise dry-run planning while a host lock is held, zero actions, mount in the report, and single JSON usage-error objects for bare sizes/month durations, malformed --json, and post-measure threshold errors. Parser/report implementation was already present; tests were format/diff checked but not run under no-deletion restriction. AC1-3 and DoD remain unchecked until integrated E2E and independent review.

2026-10-21 integrated gate: dry-run lock/oracle and JSON flag cases passed in disposable Nix containers; full just e2e passed 152/152 Nix and 6/6 native Docker tests. QA and MPED GO. AC1 remains open for a complete flag/rule matrix and Definition of Done review.

2026-10-21 AC1 coverage audit: full ensure PATH symlink/mount refusal, --select replacing configured groups, dry-run with spent tally budget, operational --timeout 0, --no-index parity and --json --help precedence need explicit probes. Added ModelHost regression a_spent_tally_budget_does_not_truncate_the_dry_run_plan: compares both complete plans with no budget vs zero budget; focused nix develop -c cargo test -p diskgc-model --test ensure a_spent_tally_budget_does_not_truncate_the_dry_run_plan --locked passed 1/1. AC1 and DoD remain open pending other probes and integrated review.

2026-10-21 added ModelHost no_timeout_waits_past_the_default_ten_minutes_for_the_host_lock: with Request.timeout None (the parser mapping of --timeout 0), the virtual host lock is released after 11m and ensure observes newly free space without deletion. Focused nix develop -c cargo test -p diskgc-model --test ensure no_timeout_waits_past_the_default_ten_minutes_for_the_host_lock --locked passed 1/1. Full CLI flag matrix, integrated E2E and reviews remain open.

2026-10-21 PATH audit correction: a PATH on a bind mount is valid under docs/safety.md Path traversal because resolve_in chooses the deepest mount as root. The earlier note calling for generic mount refusal was too broad. The added disposable-container test covers final and intermediate symlink refusal; a separate mount race test would require a shadowed/stale mount table fixture, not rejection of a normal bind mount.

2026-10-21 TASK21 AC1 focused disposable Nix container probes: ensure_refuses_symlinked_path_without_mutation (final and intermediate PATH symlinks, exit 2 usage_error, no mutation), select_replaces_configured_groups (only CLI-selected user profile deleted; configured-group candidate preserved), json_help_precedence_is_text (--json --help and malformed-size --json -h return text help/exit 0), and no_index_matches_current_dry_run_plan (one eligible candidate and equivalent plan with/without --no-index, no deletion) each passed 1/1 via nix develop -c just e2e-one. The independent deletion oracle was armed before every traced run and clean. A PATH on a bind mount is valid per docs/safety.md Path traversal; no mount-refusal claim is made. AC1 and DoD remain open pending complete flag/rule matrix, full integrated gate, and reviews.

2026-10-21 final TASK21 AC1 audit after green 158/158 Nix and 7/7 Docker E2E: still lacks direct binary/container probes for no config and no --select => exit 2, environment config precedence through ensure, operational --timeout 0 and --tally-budget 0 spellings, paired --no-index reclaim parity, JSON objects on exits 124 and SIGINT 130, and --roots validity errors. Parser/model/library tests cover parts indirectly. Keep AC1 and DoD open until these direct probes run; QA/MPED final reviews found no blocker in the implemented slice.

2026-10-21 final AC1/DoD evidence: nine new Fast disposable-container probes cover default/no-config usage, searched config then DISKGC_CONFIG then --config precedence with exact reclaim actions, --roots validity, --timeout 0 versus short nonzero lock behavior, --tally-budget 0 reclaim, --no-index reclaim parity, exit 124 and SIGINT 130 as single JSON reports with partial actions, and dry-run planning of an individually flocked candidate. The stronger >10m no-timeout and tally-bound semantics are covered by ModelHost plus parser tests. The final settled-tree nix develop -c just e2e passed 167/167 Nix image and 7/7 native Docker tests. Parallel qa-test-runner and mped-architect reviews found no blockers; final AC1 audit found no remaining material ensure flag/rule gap. A native Docker prune gate missed its 10s test cutoff once under full load, but two loaded Docker suite repeats and final gate passed; exact cause unproven, timeout unchanged, early-exit diagnostics improved. No host deletion.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
