---
id: TASK-57
title: 'Spike: containerd image-store root discovery for Docker filesystem matching'
status: Done
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-20 04:46'
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
- [x] #1 The method is documented with the setups it was verified on
- [x] #2 Setups it cannot determine are listed and map to unsupported_backend
<!-- AC:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Commit 57476a2: strict Docker info parser derives the bundled containerd image root from DockerRootDir and the verified Docker-managed socket. Native Docker Engine 29.8.2 dind imported an image with a digest blob under that root while separate /var/lib/containerd remained empty. Classic vfs is recognized; external/system containerd, ambiguous metadata, and other classic drivers fail closed as unsupported_backend. docs/safety.md records method and limits. Full just e2e passed: 125/125 Nix and 2/2 Docker tests; QA and MPED reviewed.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
