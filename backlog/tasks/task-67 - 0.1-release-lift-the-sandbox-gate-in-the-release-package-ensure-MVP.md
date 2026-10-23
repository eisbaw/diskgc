---
id: TASK-67
title: '0.1 release: lift the sandbox gate in the release package (ensure MVP)'
status: To Do
assignee: []
created_date: '2026-10-17 23:55'
updated_date: '2026-10-23 00:33'
labels:
  - wave-2
  - release
  - irreversible
  - safety
dependencies:
  - TASK-26
  - TASK-21
  - TASK-22
  - TASK-55
  - TASK-11
  - TASK-13
  - TASK-65
  - TASK-66
  - TASK-63
  - TASK-64
  - TASK-84
  - TASK-88
  - TASK-106
references:
  - PRD.md
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the human approved a scoped MVP release, "diskgc 0.1, the ensure MVP" (PRD.md, Scope, "Releases"; docs/safety.md, Pre-release sandbox gate). It is the first build that may delete on a real machine, so it is irreversible for whoever runs it.

What:
- Build the release package with the release-without-sandbox-gate feature set of diskgc-linux in exactly one place (e.g. the diskgc package's cargoExtraArgs in flake.nix). The e2e binaries, the test driver and every check stay gated.
- Update the test sandbox_gate_flag_is_enabled_nowhere (crates/diskgc-linux/src/sandbox.rs) to allow exactly that place (it must also scan nested .cargo/config files, task-64 group K). Consider a flake check that the non-release package links the marker path /.diskgc-test-sandbox and the release package does not (task-49 note).
- Deep review (QA, mped-architect, Codex) of the release scope against PRD.md's 0.1 scope.
- Release notes stating the honest limits (PRD.md, docs/safety.md) and what is out of scope (watch, the index, survey, Docker, Nix, command actions).

Evidence, by task: Cargo race tests 1-9 on every pinned toolchain (task-11); journeys for the CI operator (task-12, and on Debian with the static binary, task-63), the /tmp laptop owner (task-22) and the agent harness (task-55); ensure's full CLI contract (task-21); the contract freeze review (task-26); wave-1 hardening (task-13, task-64); the /tmp built-ins (task-65); packaging (task-66). Wave-2 hardening for 0.1 is only task-27's mutation-testing slice on selection and the executor: that slice must be green before this task closes, but the rest of task-27 (fuzzing, doc validation, other deferred findings) is not a dependency.

The v1 release (task-49) builds on this one.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The release package is the only build with the gate flag, enforced by a test (sandbox_gate_flag_is_enabled_nowhere allows exactly one place)
- [ ] #2 All MVP evidence is green: Cargo race tests 1-9 on every pinned toolchain; the journeys for the CI operator, the /tmp laptop owner and the agent harness; the contract freeze review (task-26); wave-1 and wave-2 hardening (task-13, task-64, and task-27's mutation-testing slice on selection and the executor)
- [ ] #3 Release notes list the limits and the deferred scope
- [ ] #4 The gate-lifted 0.1 release binary rejects Docker and generic command actions, whether selected from built-ins or user configuration and whether selected through a config tier or --select, before any backend call or filesystem mutation; prove this with disposable-container negative tests against the actual release artifact.
- [ ] #5 The gate-lifted 0.1 artifact enforces a reviewed runnable built-in catalog limited to Cargo and the explicitly signed-off TASK65 /tmp profiles; profiles list/show/help reflect that catalog, and disposable-container tests prove every excluded built-in is refused before deletion or backend calls. Operator-authored fs/delete policy is explicitly decided and documented.
- [ ] #6 The 0.1 release operator can set checkouts: allow or refuse for an admitted built-in profile in YAML without replacing its find, kind, or action. With no operator setting, the built-in default applies; profiles without an explicit exception refuse checkouts. This setting cannot admit excluded profiles, Docker or command actions, or arbitrary delete patterns. Disposable-container tests cover checkout-marker fixtures for admitted built-ins and verify excluded npm and uv profiles stay excluded.
- [ ] #7 Before the first real-machine run, review a dry-run of the actual gate-lifted release binary in a disposable real-kernel Docker or Podman fixture with representative 0.1 profiles. Record which deployment contracts that fixture proves and which it cannot; keep release blocked until any unproved safety-critical contract has evidence that respects the user restriction against running diskgc on the developer host.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-63's review: the e2e images run the development build with the pre-release sandbox gate. Before 0.1 ships, run the Debian image's set (just e2e-debian) against the actual release artifact (the gate lifted), not the development build, e.g. a runner variant whose under-test directory holds the release binary.

2026-10-20 user constraint supersedes prior test approach: do not test diskgc on developer host filesystem. AC4 currently requires a real-machine dry run and must be reconciled before release; substitute a reviewed disposable Docker Engine / real-kernel fixture only if it proves the relevant deployment contract, otherwise keep release blocked. The pre-release sandbox gate remains enabled until all other dependencies and this evidence are satisfied.

Compass domain review: implementing a v1 built-in does not authorize it in 0.1. Current builtin.yaml embeds more profiles than PRD.md 0.1 scope; release catalog must be enforced in the actual release artifact, or a reviewed PRD scope change must precede gate lift.

2026-10-21 read-only release audit: flake.nix still builds only gated development package; sandbox flag test requires zero enablers, and Library::builtin exposes profiles outside 0.1. Release artifact needs a separate static package with exactly one gate-lift site, pre-backend rejection of excluded built-ins/Docker/command actions from configs, tiers and --select, and a consistent list/show/help catalog. Test the actual release binary through runner --under-test in disposable Nix and Debian containers, including static/package/gate and negative scope probes. TASK66 AC3 remains open until this artifact exists. This is a plan, not implementation or release evidence. Five broad /tmp profiles in TASK65 remain unsafe; a reviewed scope decision or sound liveness protocol is required before TASK67 AC2/6 can close.

2026-10-21 Compass reconciliation: AC4 now uses a reviewed disposable real-kernel fixture and keeps unresolved deployment evidence as a release blocker, honoring the 2026-10-20 no developer-host diskgc test constraint.

Forward-carried from TASK-106: the e2e harness now has a feature-built proposed 0.1 catalog binary under test, but it remains sandbox-gated. Container tests using it prove policy rejection in that provisional build only. TASK-67 still must build and test the actual gate-lifted release artifact before release.

2026-10-22 Compass TASK-65 review: the current proposed 0.1 catalog admits no /tmp profile. Of the six TASK-65 proposals, only containers-image-tmp is implemented; its existing held-spool test uses a one-second user profile, not the exact three-day built-in or gate-lifted artifact, and dormant-spool reuse is unproved. The other five broad profiles have checked-in live no-fd/no-lock, post-exec TMPDIR, or PID-namespace counterexamples and remain unshipped. Do not lift the sandbox gate or claim AC5 until the signed-off /tmp subset and release scope are explicit, and the actual release binary proves admitted and excluded behavior in disposable real-kernel fixtures. No developer-host diskgc run.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
