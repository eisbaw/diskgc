---
id: TASK-150
title: >-
  docs/agent-pre-job.py: example deletes fresh targets, and exit 2/124/130/143
  become abort_invalid_report
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 00:54'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 docs-setup branch (b7ddaeb): example uses filter:idle-1d; decisions per documented exit (0 start, 1 defer_no_space, 75/124 retry_later, 3 abort_error, 2 abort_provisioning|abort_config by cause, 130/143 abort_interrupted, else abort_invalid_report/4). tools/test_agent_pre_job.py in just check; agent journey adds an exit-2 case (passed via just e2e-one). Walked real codes 0,1,2(parse),2(provisioning),75,124,143: tmp/walks/2026-10-25-docs-setup/30..33. Not walked: 3 and 130 (3 covered by the container journey; 130 is the SIGINT twin of 143).

Merged to master as d2c0554 on 2026-10-25T00:54Z (branch docs-setup, rebased; just check exit 0). Orchestrator accepted two deviations: JSON cause named 'provisioning' (fault class, matches exit-2 wording); 'check' warns on a missing supplied root with exit 0 (check is machine-independent; build-time checks), while ensure exits 2.
<!-- SECTION:NOTES:END -->
