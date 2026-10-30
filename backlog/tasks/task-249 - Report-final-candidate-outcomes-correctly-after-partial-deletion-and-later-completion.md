---
id: TASK-249
title: >-
  Report final candidate outcomes correctly after partial deletion and later
  completion
status: To Do
assignee: []
created_date: '2026-10-26T23:15:00Z'
labels:
  - bug
  - reporting
dependencies: []
references:
  - crates/diskgc-core/src/report/outcome.rs
  - crates/diskgc-core/src/report/human.rs
  - crates/diskgc-core/src/executor.rs
priority: medium
type: bug
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Review finding P2 (2026-10-27): the human per-candidate summary disagrees with the final filesystem outcome in two reproduced cases.

1. A candidate is partially deleted in tier early and gets a changed skip, then tier later deletes the remainder successfully. The summary still says "reclaimed part-way ... then kept: changed [early]" although the candidate no longer exists. In report/outcome.rs, live() records the latest action tier but then marks part_way for a skip matching ANY action tier, including an obsolete earlier tier.

2. A file is created during deletion; the parent unlink fails ENOTEMPTY after another file was removed. The candidate and new file remain, but the summary prints "reclaimed /tmp/candidate". live() receives actions and skipped only, so a partial deletion ending in errors cannot be distinguished from completion. The run still reports the error and its status; the defect is the candidate verdict.

Both were reproduced through executor::ensure_grouped on ModelHost, not just by constructing report rows. Fixture: a 1 GiB /tmp mount, provisioned host lock, user profile matching /tmp/candidate, and an unreachable 1 GiB min-free goal. For case 1 use two tiers selecting that profile, files sub/x and sub/y, and an event replacing sub/y after unlinking sub/x. For case 2 use one tier with sub/x and an event creating sub/new after unlinking sub/x. The call trace confirms complete deletion in case 1 and ENOTEMPTY with surviving contents in case 2.

Derive human verdicts from each candidate's final applicable outcome, including errors/stops, rather than treating every action entry as completion or retaining obsolete skips. Preserve JSON actions as attempts and existing exit-code precedence.

Local reproduction: nix develop -c cargo run --offline --manifest-path tmp/review-probe/Cargo.toml (exit 0). Scratch source: tmp/review-probe/src/main.rs; observed output: tmp/review-probe.log. These ignored artifacts need permanent regression tests in the project.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A candidate partly deleted with a skip in one tier and completed in a later tier is reported as reclaimed by the later tier, without the obsolete part-way refusal.
- [ ] #2 A deletion ending in ENOTEMPTY or another candidate error after removing entries is reported as partial/failed with its cause, not as completed; cover a deadline stop after partial deletion too.
- [ ] #3 ModelHost regressions assert both the filesystem/call trace and the rendered summary for the two reproduced cases, and fail against the current implementation.
- [ ] #4 Preserve action-attempt JSON semantics, observed-delta accumulation and exit-code precedence; update the output contract if wording or fields change and pass just e2e before committing.
<!-- AC:END -->
