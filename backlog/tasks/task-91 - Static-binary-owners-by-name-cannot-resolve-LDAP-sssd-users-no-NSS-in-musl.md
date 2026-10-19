---
id: TASK-91
title: 'Static binary: owners: by name cannot resolve LDAP/sssd users (no NSS in musl)'
status: To Do
assignee: []
created_date: '2026-10-19 04:49'
labels:
  - wave-2
  - safety
  - config
dependencies: []
references:
  - docs/reference.md
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in review of task-63 (the Debian image). The shipped binary is static musl, and musl has no NSS modules: getpwnam reads /etc/passwd and, when that has no match, asks a running nscd; it never loads libnss_sss or libnss_ldap. On a CI fleet whose runner accounts come from LDAP or sssd (no nscd), `owners:` by user name (required with --allow-root, task-21 AC #4: "resolving to non-system users") cannot resolve them, and a name that does not resolve must fail loudly, never be skipped. The e2e images replace /etc/passwd with the test users, so no test sees this.

Options: accept numeric uids in `owners:` (a schema decision, before the freeze, task-26); resolve through `getent passwd` (a child process, honours NSS; trust and timeout questions); or document that `owners:` by name needs local accounts or nscd. Whatever is chosen, a name that does not resolve is exit 2 at run time (safety.md, Validation), and the message says why (static binary, no NSS).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The resolution rule for owners: names on a static binary is decided and written in reference.md (owners) with the static-binary limit stated
- [ ] #2 A name that does not resolve is exit 2 with a message naming the cause; covered by a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
