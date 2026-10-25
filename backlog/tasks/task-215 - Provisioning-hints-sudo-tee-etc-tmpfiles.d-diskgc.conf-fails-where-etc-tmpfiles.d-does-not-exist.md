---
id: TASK-215
title: >-
  Provisioning hint's 'sudo tee /etc/tmpfiles.d/diskgc.conf' fails where
  /etc/tmpfiles.d does not exist
status: To Do
assignee: []
created_date: '2026-10-25 12:10'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: the clean Debian container has no /etc/tmpfiles.d; `cp lib/tmpfiles.d/diskgc.conf /etc/tmpfiles.d/` failed (release notes now say install -Dm0644). The missing-lock error prints the same tee line, which fails there too. Prefix `sudo mkdir -p /etc/tmpfiles.d` or use install -D. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/02-root-install.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/13-no-lock.txt.
<!-- SECTION:DESCRIPTION:END -->
