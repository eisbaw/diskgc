---
id: TASK-218
title: >-
  Help text profile lines use internal jargon (temp-prefix v1 roots, the pytest
  contract)
status: To Do
assignee: []
created_date: '2026-10-25 12:10'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk, reading diskgc --help as a new user: 'pytest-tmp ... pytest session directories under temp-prefix v1 roots in /tmp, once the pytest contract permits'. Neither term is explained in the help or the package. Say it plainly, e.g. 'ended pytest sessions under /tmp/pytest-of-USER (also inside ended nix-shell TMPDIRs); see profiles show pytest-tmp'. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/03-help.txt.
<!-- SECTION:DESCRIPTION:END -->
