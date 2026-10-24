---
id: TASK-128
title: Dry-run plan drops build_evidence for union and negated fs selections
status: Done
assignee: []
created_date: '2026-10-23 22:08'
updated_date: '2026-10-24 20:48'
labels:
  - contract
  - report
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
docs/reference.md (Output) says a source-anchored build candidate whose source was established at discovery has build_evidence {marker, source, idle_age_seconds}. executor::plan_fs sets it; plan_fs_union (used for every fs selection with || or !profile:) never does, so 'profile:meson-build-anchored && !profile:pin' plans the same candidates without build_evidence. Found by TASK-121 vectors builtins/meson-build-anchored.yaml and builtins/west-build-anchored.yaml, which pin the current (wrong) rows without source: and say so; when fixed, add source: to their plan rows. Report-contract only: no deletion decision depends on it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented 2026-10-24 on branch task130 (commit c2a2dc4 + review rounds 8d9ddc9, 8b76af2). plan_fs and plan_fs_union share eligible_build_evidence (judgement-verified source + walk age) and skipped_build_evidence (discovery source, no age); every union row sets it (eligible, guard veto, liveness veto, checkout refusal, first skip). Contract reading (architect's recommendation, conservative): an eligible row reports only the evidence its own term's judgement verified; a skipped row reports only its own term's discovery evidence; a row never reports another profile's anchor. meson/west-build-anchored vectors now expect source: and pin full build_evidence (idle_age_seconds 1220400 computed from clock and mtimes); new select/union-keeps-build-evidence (anchored row has evidence; plain-term rows, including one also found by the Meson term, have none). Bite: HEAD executor fails meson/west/union vectors; blanking discovered() fails meson/west. Reviews: mped-architect GO, qa-test-runner GO.

Merged to master (fast-forward) at 5e81db1 after nix develop -c just e2e exit 0 (2026-10-24T20:47Z; see git note on 5e81db1).
<!-- SECTION:NOTES:END -->
