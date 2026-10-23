---
id: TASK-126
title: >-
  pidfd_open under container runtimes' default seccomp profiles: which ones make
  diskgc refuse to start
status: To Do
assignee: []
created_date: '2026-10-23 17:12'
labels:
  - hardening
  - deployment
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-64's Linux 5.8 VM (2026-10-23): inside a Podman 2.1.1 container (NixOS 20.09) with its built-in seccomp profile, pidfd_open returns EPERM, and diskgc refuses to start (docs/safety.md, In use: no process could be confirmed gone without pidfd_open). That refusal is correct and fail-closed, but CI runner fleets often run jobs (and so a pre-job ensure) inside Docker or Podman containers. Which default profiles in use today deny pidfd_open or pidfd_send_signal is unmeasured; the claim that 'container runtimes of 2020 do this' rests on one data point. Measure in disposable containers/VMs: Docker 20.10 and current, Podman 3.x/4.x/5.x, containerd/nerdctl, Kubernetes RuntimeDefault; record the minimum versions in docs/safety.md and make the refusal message name the remedy (a runtime or profile that allows pidfd_open and pidfd_send_signal, or running ensure on the host). Do not weaken the refusal.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed runtime/version is measured (pidfd_open and pidfd_send_signal allowed or refused) in a disposable container, with the command and output recorded
- [ ] #2 docs/safety.md names the measured minimum versions and the refusal message names the remedy
- [ ] #3 A test pins the refusal message text for the EPERM case
<!-- AC:END -->
