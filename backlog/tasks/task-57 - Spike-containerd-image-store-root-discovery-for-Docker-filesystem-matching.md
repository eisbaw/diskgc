---
id: TASK-57
title: 'Spike: containerd image-store root discovery for Docker filesystem matching'
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - spike
dependencies:
  - TASK-34
references:
  - docs/safety.md#docker-kinds
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Docker 29 defaults to the containerd image store, so finding its storage root is the main path, not an edge case. Find a reliable way to determine where image data lives (bundled vs system containerd, rootful daemon), with dind experiments; write the findings and the chosen method into docs/safety.md#docker-kinds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The method is documented with the setups it was verified on
- [ ] #2 Setups it cannot determine are listed and map to unsupported_backend
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
