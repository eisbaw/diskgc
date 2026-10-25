---
id: TASK-198
title: 'check has no --json, so its JSON config-error branch is unreachable'
status: Done
assignee: []
created_date: '2026-10-25 07:31'
updated_date: '2026-10-25 09:55'
labels:
  - shipping
  - json
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in the TASK-192 walk (tmp/walks/2026-10-25-contract/03-*). 'diskgc check --config X --json' exits 2 with {status, exit_code, errors:[{cause: parse, message: 'check: unknown flag --json'}]}: check takes no --json (reference.md: 'diskgc check [--config FILE]'), yet main.rs Invocation::Check's Err arm prints error_json when json is set, which only an unparseable command line can set, so that arm is dead. Decide before the 0.1 freeze: give check --json (its tier lines as JSON plus the envelope on failure) or delete the dead arm. Not a regression of TASK-192.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark may veto): drop the unreachable JSON branch for 0.1; add 'check --json' after 0.1 if a harness needs it.

lane-c: the arm was NOT dead. 'diskgc check --config --json' parses with --json as the config path and json_requested asks for JSON, so the arm printed the JSON error object; with it removed (c9e0918) stdout was empty (reproduced, exit 2). Reverted in d65b97d, arm kept with a comment, pinned by json_anywhere_gives_one_object. check --json as a flag stays post-0.1 (mark-emulator decision unchanged in substance: no new flag for 0.1). Awaiting merge.

Merged to master as 9f317cb on 2026-10-25T09:54Z.
<!-- SECTION:NOTES:END -->
