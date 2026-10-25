---
id: TASK-218
title: >-
  Help text profile lines use internal jargon (temp-prefix v1 roots, the pytest
  contract)
status: Done
assignee: []
created_date: '2026-10-25 12:10'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk, reading diskgc --help as a new user: 'pytest-tmp ... pytest session directories under temp-prefix v1 roots in /tmp, once the pytest contract permits'. Neither term is explained in the help or the package. Say it plainly, e.g. 'ended pytest sessions under /tmp/pytest-of-USER (also inside ended nix-shell TMPDIRs); see profiles show pytest-tmp'. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/03-help.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): pytest-tmp description now 'pytest session directories in /tmp/pytest-of-USER (also inside nix-shell and mktemp temp dirs in /tmp), once no pytest run is seen using them' (matching set unchanged); help points at shipped README/release notes. Walk: tmp/walks/2026-10-25-polish/dumps/b03-help.txt -> tmp/walks/2026-10-25-polish/dumps/a03-help.txt.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
