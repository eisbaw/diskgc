---
id: TASK-149
title: check and config loading stop at the first error
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
Found by TASK-143 walk (tmp/walks/2026-10-24/21-laptop-config-iterate.txt, tmp/walks/2026-10-24/31-config-errors.txt). A config with three problems needs three edit-run rounds; bad2.yaml had a single '&' and a '~' root, and only the first was reported. PRD: 'the config is validated completely at load'. Expected: check reports every static error it can find in one run (parse errors aside), each with file and line. Also: serde errors read 'invalid config /home/tester/bad1.yaml: error: line 7 ...' (double 'error') and point at '--> <input>:7:9' instead of the file path.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch cli-msg 5f63996 + ad6d98c (not merged). config::parse collects every independent problem (bounded MAX_PROBLEMS=20, rest counted), each as PATH:LINE: message in file order; YAML syntax/schema errors stay single but lose the double 'error' and show the file path instead of <input>. Walk: tmp/walks/2026-10-25-cli-msg/before|after/08-three-errors.txt, 09-yaml-error.txt.

Merged to master as 06de344 on 2026-10-25T02:42Z.
<!-- SECTION:NOTES:END -->
