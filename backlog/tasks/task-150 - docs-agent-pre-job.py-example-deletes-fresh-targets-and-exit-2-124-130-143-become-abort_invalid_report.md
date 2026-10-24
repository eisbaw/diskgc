---
id: TASK-150
title: >-
  docs/agent-pre-job.py: example deletes fresh targets, and exit 2/124/130/143
  become abort_invalid_report
status: To Do
assignee: []
created_date: '2026-10-24 21:10'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/41-agent-pre-job.txt). The script's docstring example is --select 'profile:cargo-target' with no idle filter. Run as written in the walk it reclaimed six targets whose last build was seconds old (the running build and the running binary were kept by the safety checks). For an agent harness with parallel agents that deletes sibling agents' targets between their builds. The reference CI example uses filter:idle-1d.
Also: the script accepts only exits 0/1/3/75; a missing host lock or a typo'd --timeout (exit 2), a deadline (124) and a signal (130/143) all become {decision: abort_invalid_report, error: 'unsupported diskgc exit 2: usage_error'}, which tells the scheduler the report was invalid when it was a provisioning or caller error.
Fix: an idle filter in the example; explicit decisions for 2 (abort_config, with errors[].cause), 124 (retry_later) and 130/143.
<!-- SECTION:DESCRIPTION:END -->
