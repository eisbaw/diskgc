---
id: TASK-245
title: >-
  Index reclaim: reuse the synchronous tally's sizing as the candidate's
  read-only tally
status: To Do
assignee: []
created_date: '2026-10-26 08:05'
labels:
  - wave-3
  - performance
dependencies:
  - TASK-31
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First-tier candidates taken from the index are walked three times before deletion: sync tally sizing (pre-lock, tally/size.rs), the per-candidate fs_kind::tally in judge_from_index (refusals, idle_age_seconds, expected bytes) and the locked eligibility walk. The sizing walk could produce what the tally needs (newest change, entries, bytes, refusals) for units sized in this run, dropping one walk per candidate. Measure on the 100k fixture first (fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm).
<!-- SECTION:DESCRIPTION:END -->
