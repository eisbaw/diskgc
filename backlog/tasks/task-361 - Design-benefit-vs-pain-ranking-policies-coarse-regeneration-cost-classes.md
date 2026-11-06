---
id: TASK-361
title: 'Design: benefit-vs-pain ranking policies (coarse regeneration-cost classes)'
status: To Do
assignee: []
created_date: '2026-11-05T20:58:00Z'
updated_date: '2026-11-06T08:44:00Z'
labels:
  - design
  - ranking
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-05 brainstorm: priorities across profiles instead of one profile at a time; score = benefit (bytes) vs pain (cost to get the data back; download is cheaper than rebuild). Answers: pain = coarse classes declared per profile (free / download / rebuild / expensive), weighted by size and idle age; ranking: 'we need several ranking policies' (selectable, e.g. today's ordered tiers, bands with score inside, one global score, score-ordered tiers). Deliverable: PRD + reference.md design of the policies (names, config/CLI, defaults, explainability in the summary/survey, safety: scoring orders eligible candidates only, never makes one eligible), reviewed; then implementation tasks. Open questions for Mark during design: online/offline awareness; per-profile tau for reuse decay.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark 2026-11-05: policies = ordered (today's tiers; DEFAULT), global-score (every eligible candidate competes on benefit/pain across all profiles; tiers only group configuration), cheapest-first (by pain class only: free, download, rebuild, expensive, until the goal). Not banded-score.

Mark 2026-11-06: deferred until after 0.2 (0.3).
<!-- SECTION:NOTES:END -->
