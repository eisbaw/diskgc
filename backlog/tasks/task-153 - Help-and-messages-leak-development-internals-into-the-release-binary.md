---
id: TASK-153
title: Help and messages leak development internals into the release binary
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 02:42'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/01-help.txt, tmp/walks/2026-10-24/02-help-sub.txt). In the 0.1 release binary: --help says 'watch and survey arrive with later tasks' and 'The full contract is docs/reference.md in diskgc's source tree' (the package ships no reference.md); 'diskgc survey' says 'not implemented in this build yet (task-38)'; help says 'User profiles from your config are listed by diskgc profiles list' though 0.1 refuses user profiles; help does not say where the config file is looked up (~/.config/diskgc/config.yaml, /etc/diskgc/config.yaml) or where the example config is installed. 'diskgc profiles test' says '8 profiles: examples pass' when none of the 8 has examples.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch cli-msg e10a808 + ad6d98c (not merged). --help: no 'later tasks'/source-tree path; says config lookup and example location; release build says it runs only its built-ins. watch/survey: no task numbers. executor/ipc refusals and release_scope drop task numbers/docs paths. profiles test counts examples of source profiles. Walk: tmp/walks/2026-10-25-cli-msg/before|after/01, 02, 11, 12.

Merged to master as 06de344 on 2026-10-25T02:42Z.
<!-- SECTION:NOTES:END -->
