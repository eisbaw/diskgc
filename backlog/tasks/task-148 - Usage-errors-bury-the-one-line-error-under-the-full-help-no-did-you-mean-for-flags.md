---
id: TASK-148
title: >-
  Usage errors bury the one-line error under the full help; no did-you-mean for
  flags
status: Done
assignee: []
created_date: '2026-10-24 21:10'
updated_date: '2026-10-25 02:42'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch cli-msg e10a808 + ad6d98c (not merged). Usage error = 'diskgc: error: MSG' + the command's usage lines + one 'Run `diskgc --help` ...' line (exit 2); did-you-mean for flags of ensure/profiles/check, profiles subcommands and the first word (help -> --help) via new diskgc_core::suggest. Walk: tmp/walks/2026-10-25-cli-msg/before|after/02-07. Hint names `diskgc --help` (help is one text everywhere), not `diskgc ensure --help`.

Merged to master as 06de344 on 2026-10-25T02:42Z.
<!-- SECTION:NOTES:END -->
