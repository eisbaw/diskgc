---
id: TASK-294
title: >-
  Mount table freshness across deletion steps: reuse only while poll(POLLPRI) on
  /proc/self/mountinfo reports no change
status: To Do
assignee: []
created_date: '2026-11-01T09:06:00Z'
updated_date: '2026-11-10T17:52:00Z'
labels:
  - safety
  - performance
  - needs-mark
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-292 review (mped, 2026-11-01). After TASK-292 the reclaim main thread spends ~30% re-reading /proc/self/mountinfo, which safety.md's Deletion protocol reads afresh at several steps (step 1 'read now', step 4 'read again', before the delete). Sharing one table ACROSS steps changes the documented protocol, so it is Mark's safety decision. Recommended rule: 'unchanged since read' as told by the kernel, not by time: hold /proc/self/mountinfo open, poll for POLLPRI/POLLERR before reuse, reuse only while a zero-timeout poll reports no change; any event or poll error means read again. Exact for diskgc's own mount namespace (same scope as today); the mechanism libmount's monitor and systemd use. Needs a change-event model in ModelHost and a test where a mount appears between steps. Reject a time-based reuse window (adds a new accepted-limit window). Weaker fallback if declined: share one table across the read-only steps (step 1, tally reach, step 2), keep the read under the lock and the one before the delete separate. Measure first (TASK-274's within-step collapse may be enough).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark 2026-11-10 ('do all first four', TASK-334 step 3): go for the recommended rule, reuse only while a zero-timeout poll(POLLPRI/POLLERR) on the held /proc/self/mountinfo reports no change; no time-based window. The protocol change in safety.md goes with it.

2026-11-10 recon (no code): the poll rule is NOT exact. Verified on kernel 7.2.9 in unshare -rm and in mounts_poll/touch_mnt_namespace source: a rename of a directory holding a mount point, or of a bind source, changes the mount point/root fields of /proc/self/mountinfo with no poll event (also, from source only: a deleted bind source, a superblock remount from another namespace). diskgc relies on those fields (contains_mount by path and place, systemd-private root refusal, protect translation, hidepid/upperdir). A cached table would miss a mount renamed into a candidate until an unrelated mount event: an unbounded window, the property the time-based window was rejected for; through a non-recursive bind nothing else catches it (TASK-290 partial deletion). Also: poll consumes the event (not level), reading does not clear it, only poll-then-read is safe, the event belongs to the open file description. Not implemented. Exact alternative: one read per protocol step shared by all consumers at that step, including the nested-build guard's per-term reads. Measured gain of any variant: <0.5% laptop, 13-36% of the container floor. Awaiting Mark's ruling; probes in the session scratchpad (probe.py, probe2.py).
<!-- SECTION:NOTES:END -->
