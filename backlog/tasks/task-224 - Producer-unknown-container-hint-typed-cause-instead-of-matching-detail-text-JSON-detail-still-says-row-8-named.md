---
id: TASK-224
title: >-
  Producer-unknown container hint: typed cause instead of matching detail text;
  JSON detail still says 'row 8 (named)'
status: To Do
assignee: []
created_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-221 merge: the human hint finds its cause by matching the end of the detail text (constants in liveness.rs). A typed cause would be cleaner. The JSON detail for script authors still reads 'liveness_contract pytest row 8 (named)'.
<!-- SECTION:DESCRIPTION:END -->
