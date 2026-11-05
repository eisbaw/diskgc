---
id: TASK-347
title: >-
  README in the hyp visual pattern: banner, badges, model and states figures,
  one real session
status: Done
assignee: []
created_date: '2026-11-05T06:26:00Z'
updated_date: '2026-11-05T10:31:00Z'
labels:
  - docs
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-05: '/readme-improver like hyp on e github eisbaw projects readme, same pattern'. Reference: github.com/eisbaw/hyp README (banner.svg, badge line, one-line what + why bullets, model.svg, quick start, one real session with trimmed output, states.svg, short sections: for agents, storage, when not to use, documentation list, related tools, license). Apply to the repo-root README.md; packaging/README.md stays the package's own README (shipped as share/doc/diskgc/README.md).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-05 orchestrator: implementer in tmp/worktrees/t347 (branch t347-readme-visual).

Merged 375317dadebc88ba70a1732567ad5ffebe091a6e (2026-11-05). Follow-ups: safety.md 'kept' vs keep_marker; PRD doc map lacks docs/assets/; docs/diskgc-design.svg 'from the index' looks stale; no test ties SVG enum names to code; check_docs.py misses reference-style links; producer_unknown on pidfd_open ENOENT worth a look.
<!-- SECTION:NOTES:END -->
