---
id: TASK-368
title: >-
  Bazel output base is never reclaimed for a Git workspace: execroot's .git
  symlink trips the checkout guard
status: To Do
assignee: []
created_date: '2026-11-07T15:33:00Z'
labels:
  - profiles
  - bazel
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-354 finding 2, answering TASK-331: the execroot links the workspace's .git as a symlink; the checkout guard counts it, so bazel-output-base-anchored is always contains_checkout for a Git workspace and reclaims almost nothing. Decide a guard exception scoped like empty_checkout_markers (a symlink to the workspace's own .git at the execroot), or leave Bazel out.
<!-- SECTION:DESCRIPTION:END -->
