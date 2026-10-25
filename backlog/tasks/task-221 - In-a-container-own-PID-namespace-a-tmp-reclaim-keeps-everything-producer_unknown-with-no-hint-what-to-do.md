---
id: TASK-221
title: >-
  In a container (own PID namespace) a /tmp reclaim keeps everything
  producer_unknown with no hint what to do
status: To Do
assignee: []
created_date: '2026-10-25 13:04'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk, release-01-candidate 80c858c, clean Debian container (podman --init, own PID ns), /tmp sessions over 1 h old: ensure --select 'profile:pytest-tmp || profile:nix-shell-rc' /tmp reclaims nothing, exit 1, '9 kept: producer_unknown; ... liveness_contract pytest row 8 (named): the process list is not complete: diskgc is not in the initial PID namespace'. This is the documented 0.1 limit, but a CI operator running ensure in a job container only learns it from the release notes. Add one summary line, like the in_use_unknown busy line: diskgc runs in its own PID namespace (a container); /tmp sources need the host's process list; run ensure on the host (or --pid=host). Also 'row 8 (named)' is contract-table jargon in a user line. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/14-dry-select-tmp.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/15-ensure-config-tmp.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/16-json-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->
