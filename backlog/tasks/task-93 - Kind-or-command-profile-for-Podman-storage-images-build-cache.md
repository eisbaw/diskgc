---
id: TASK-93
title: 'Kind or command profile for Podman storage (images, build cache)'
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
labels:
  - home
dependencies:
  - TASK-37
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the home survey (2026-10-18) found ~8.7 GiB of rootless Podman images in ~/.local/share/containers/storage, and diskgc's own e2e images live there. Rootless overlay files are owned by subuids, so a plain delete is refused (not_owner) and would corrupt storage anyway; Podman must clean itself (podman image prune [-a], podman system prune), like the Docker kinds (task-35). Decide kind vs command action, how 'unused' and age are judged (podman's own filters: until=, dangling), what it reports freed, and refusal while a podman process is running.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile (or kind) prunes Podman images with Podman's own command, never by deleting storage files; examples and a model test
- [ ] #2 Verified in a container or read-only on a real machine (dry run lists what podman would prune)
- [ ] #3 Docs say how it relates to the Docker kinds and to rootful /var/lib/containers
<!-- AC:END -->
