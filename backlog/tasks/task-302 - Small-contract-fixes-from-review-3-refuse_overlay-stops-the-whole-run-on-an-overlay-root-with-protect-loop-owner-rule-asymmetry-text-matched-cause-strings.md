---
id: TASK-302
title: >-
  Small contract fixes from review #3: refuse_overlay stops the whole run on an
  overlay root with protect; loop owner-rule asymmetry; text-matched cause
  strings
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
labels:
  - contract
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped SHOULD 7: refuse_overlay stops the run (Stop::Failed) when a candidate is on overlay and protect exists; in a --pid=host CI container with / on overlay every run stops; skip that candidate instead. SHOULD 5 (input to TASK-296): a backing name resolving to an owner's file blocks every candidate; with the same accepted decoy limit it could block only the resolved filesystem (unknown_on) and record the identity as a use, so a udisks-mounted ISO in ~/Downloads stops blocking /tmp. SHOULD 12: report/human.rs recognises causes with ends_with(HIDEPID); match a type. NITs: snapshot guard message redundant test; Measured::space doc says space_of; bind_view re-implements ancestry and re-reads mountinfo; place_in_filesystem joins //deleted as a component (refuse those roots).
<!-- SECTION:DESCRIPTION:END -->
