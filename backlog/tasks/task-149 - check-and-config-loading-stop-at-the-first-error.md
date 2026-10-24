---
id: TASK-149
title: check and config loading stop at the first error
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
Found by TASK-143 walk (tmp/walks/2026-10-24/21-laptop-config-iterate.txt, tmp/walks/2026-10-24/31-config-errors.txt). A config with three problems needs three edit-run rounds; bad2.yaml had a single '&' and a '~' root, and only the first was reported. PRD: 'the config is validated completely at load'. Expected: check reports every static error it can find in one run (parse errors aside), each with file and line. Also: serde errors read 'invalid config /home/tester/bad1.yaml: error: line 7 ...' (double 'error') and point at '--> <input>:7:9' instead of the file path.
<!-- SECTION:DESCRIPTION:END -->
