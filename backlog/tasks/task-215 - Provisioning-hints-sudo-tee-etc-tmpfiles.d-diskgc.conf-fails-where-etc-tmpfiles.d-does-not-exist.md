---
id: TASK-215
title: >-
  Provisioning hint's 'sudo tee /etc/tmpfiles.d/diskgc.conf' fails where
  /etc/tmpfiles.d does not exist
status: Done
assignee: []
created_date: '2026-10-25 12:10'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: the clean Debian container has no /etc/tmpfiles.d; `cp lib/tmpfiles.d/diskgc.conf /etc/tmpfiles.d/` failed (release notes now say install -Dm0644). The missing-lock error prints the same tee line, which fails there too. Prefix `sudo mkdir -p /etc/tmpfiles.d` or use install -D. Dumps: tmp/walks/2026-10-25-rc-walk-debian/dumps/02-root-install.txt, tmp/walks/2026-10-25-rc-walk-debian/dumps/13-no-lock.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): hint uses 'printf ... | sudo install -Dm0644 /dev/stdin /etc/tmpfiles.d/diskgc.conf'. Walk: tmp/walks/2026-10-25-polish/dumps/b13r-root-provision-hint.txt (tee fails) -> tmp/walks/2026-10-25-polish/dumps/a13r-root-provision-hint.txt (creates dir, file == package's, lock made). Only GNU coreutils install checked (Debian); busybox install with /dev/stdin not verified.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
