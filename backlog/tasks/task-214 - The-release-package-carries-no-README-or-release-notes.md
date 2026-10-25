---
id: TASK-214
title: The release package carries no README or release notes
status: Done
assignee: []
created_date: '2026-10-25 12:09'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: the package is bin/diskgc, lib/tmpfiles.d/diskgc.conf and share/doc/diskgc/examples/example.yaml. A user with only the package has --help and the provisioning error; the install steps, honest limits and JSON contract (reference.md, 0.1 release notes) are only in the source tree. Ship the release notes (and a pointer to reference.md) under share/doc/diskgc. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/01-root-look.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): package ships share/doc/diskgc/{README.md,RELEASE-NOTES.md,reference.md,safety.md}; RELEASE-NOTES cut from reference.md at build time (fails closed); README = packaging/README.md. Walk: tmp/walks/2026-10-25-polish/dumps/a01-root-look.txt, tmp/walks/2026-10-25-polish/dumps/a02-root-readme-install.txt, tmp/walks/2026-10-25-polish/dumps/a02b-readme-first-run.txt. release-01-package check extended.

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
