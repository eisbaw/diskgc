---
id: TASK-148
title: >-
  Usage errors bury the one-line error under the full help; no did-you-mean for
  flags
status: To Do
assignee: []
created_date: '2026-10-24 21:10'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/30-errors-flags.txt, tmp/walks/2026-10-24/02-help-sub.txt). 'diskgc ensure --min-fre 1G /home/tester' prints 'diskgc: ensure: unknown flag --min-fre' followed by ~30 lines of the full help, so the error scrolls off; the same for a bad unit, a relative PATH, a missing PATH, a bare 'diskgc' and 'diskgc help'. Profiles already suggest close names ('close names: cargo-target'); flags do not. Under --json the same error reads 'diskgc: error: ensure: unknown flag' vs 'diskgc: ensure: unknown flag' without --json.
Expected: the error, one usage line for the command, and 'see diskgc --help'; suggest the closest flag.
<!-- SECTION:DESCRIPTION:END -->
