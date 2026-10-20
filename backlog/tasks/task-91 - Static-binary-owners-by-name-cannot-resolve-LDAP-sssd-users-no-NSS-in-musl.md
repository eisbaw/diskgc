---
id: TASK-91
title: 'Static binary: owners: by name cannot resolve LDAP/sssd users (no NSS in musl)'
status: Done
assignee: []
created_date: '2026-10-19 04:49'
updated_date: '2026-10-20 10:38'
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
- [x] #1 The resolution rule for owners: names on a static binary is decided and written in reference.md (owners) with the static-binary limit stated
- [x] #2 A name that does not resolve is exit 2 with a message naming the cause; covered by a test
- [x] #3 Explicit {uid: N} owners are accepted only for non-system UIDs; the protected policy documents stable UID assignment and a disposable-container deletion oracle proves a UID absent from passwd is honored
- [x] #4 A mixed name and explicit UID resolving to the same owner is rejected before planning or deletion
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Compass domain review: explicit UID policy is coherent, but stable fleet UID assignment is an operator contract; test the numeric path and resolved-UID collision before closing.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Static musl owner policy accepts explicit non-system {uid: N} mappings for accounts absent from passwd, retains name resolution and fail-fast unknown names, and rejects resolved-UID aliases. Docs state the stable-UID contract. Focused ModelHost/unit and disposable-container positive/negative cases passed; exact-tree full E2E and QA/MPED commit gates are being run by integration.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
