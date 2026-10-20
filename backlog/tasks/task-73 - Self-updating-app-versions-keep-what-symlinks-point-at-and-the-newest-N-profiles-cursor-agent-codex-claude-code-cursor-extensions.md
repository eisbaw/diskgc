---
id: TASK-73
title: >-
  Self-updating app versions: keep what symlinks point at and the newest N;
  profiles cursor-agent, codex, claude-code, cursor extensions
status: Done
assignee: []
created_date: '2026-10-18 20:53'
updated_date: '2026-10-20 02:26'
labels:
  - home
  - wave-4
  - irreversible
dependencies:
  - TASK-62
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: self-updating CLIs keep every release under a versions dir and point a symlink at the one in use (~/.local/share/cursor-agent/versions/* with ~/.local/bin/{agent,cursor-agent}; ~/.codex/packages/*/releases/* with standalone/current). The version in use can be months old, so idle_for alone would delete it. Extends task-62 (keep newest N): add a predicate that keeps any candidate that is (or contains) the resolved target of a named symlink, then profiles cursor-agent-versions and codex-releases (~8.3 GiB net reclaimable here). Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A candidate that is the resolved target of the named symlink is never eligible (model test, incl. a dangling and a relative symlink)
- [x] #2 cursor-agent-versions and codex-releases written with examples and verified by a read-only dry run
- [x] #3 docs/reference.md documents the predicate (single home)
- [x] #4 Profiles cursor-agent-versions, codex-releases, claude-code-versions and cursor-extensions with examples; a dry run here keeps both live cursor-agent versions plus the newest N and lists the rest with sizes matching du
- [x] #5 A version a running process executes from is in_use (container test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Widened 2026-10-19 (user: self-updating software leaves old versions accruing, e.g. cursor under ~/.local). Measured read-only here: ~/.local/share/cursor-agent/versions 31 dirs 5.2 GB (TWO live versions: ~/.local/bin/agent -> 2026.06.24, ~/.local/bin/cursor-agent -> 2026.07.16), ~/.codex/packages/standalone/releases 10 dirs 3.4 GB + app-server-daemon 0.4 GB (current link), ~/.local/share/claude/versions 3 dirs 0.8 GB (~/.local/bin/claude), ~/.cursor/extensions 33 dirs 0.45 GB (old extension versions beside new), ~/.local/share/uv/python 0.4 GB (uv's own 'uv python uninstall' instead). Safe rule: a version dir is eligible only if (1) no symlink in the configured link dirs (~/.local/bin, ~/bin) or the app's own current link resolves into it - several can be live at once; (2) not among the newest N (default 2) by version/mtime; (3) idle; (4) not in use (a running binary from it is in_use via /proc exe). The ~/bin/claude-2.1.* binaries (14) look hand-managed: out of scope unless Mark says otherwise.

2026-10-19 user: ignore ~/bin/claude-* entirely (not a target). The ~/.local/share Cursor case is ~/.local/share/cursor-agent/versions (the only cursor dir there).

Implementation candidate in isolated /tmp/diskgc-version-profiles, not yet integrated or committed: root-relative protect_links and package-parent current link, readlinkat port, fail-closed missing/dangling/outside-root targets, four built-ins; model dry-run/reclaim tests and focused kernel container E2E pass. Read-only host dry runs: cursor-agent 32 found/29 eligible/2 linked_current/1 newest_version, 5,036,937,216 B eligible; Codex 12/9/2/1, 3,095,842,816 B; Claude native version files 3/1/1/1, 267,964,416 B; Cursor extensions 32 distinct IDs with one directory each, all kept by N=2 and zero eligible. Seven du -sx -B1 samples match estimated bytes exactly. The older note claiming extension versions beside newer ones is stale on this host. No live deletion.

Cursor extensions safety follow-up: the implementation now checks the native .cursor/extensions/extensions.json registry before ranking by mtime. Every listed relativeLocation is retained, including an old installed version outside newest N; missing/malformed/oversized/unreadable/changed manifests fail closed. The host registry has 32 distinct relativeLocation entries matching all 32 extension directories, with 0 mismatched location.path fields. Model tests cover installed-old, unlisted-old eligible, and missing/malformed/unsafe/unreadable registry cases. A new post-change host dry run is pending while integrated E2E runs.

2026-10-20 QA safety follow-up in isolated integration worktree: protect_link_dirs now scans every symlink in both .local/bin and bin for all four version profiles, including aliases. Unsafe or unreadable launcher directories retain candidates. Symlink targets are resolved component by component; internal dotdot after a named component is ambiguous and retains candidates. Model alias mutation bite: an old v0 linked by bin/renamed-launcher is linked_current; clearing protect_link_dirs in the otherwise identical compiled tier makes v0 eligible. Model missing, unsafe, unreadable directory and internal-dotdot regressions pass. Read-only host surveys after this change: Cursor agent 32 found, 29 eligible, 2 linked_current, 1 newest; Codex 12 found, 5 eligible, 7 linked_current including historical bin aliases; Claude native 3 found, 1 eligible, 1 linked_current, 1 newest; Cursor extensions 32 found, 0 eligible, all 32 installed_extension from the registry. Eligible estimated bytes: 5,036,937,216 + 1,722,105,856 + 267,964,416 respectively. One du -sx -B1 sample per eligible profile exactly matched estimated bytes. Exact installed versions recorded in built-ins: Cursor agent link targets 2026.06.24-00-45-58-9f61de7 and 2026.07.16-899851b; Codex current 0.163.0 standalone and 0.158.0 daemon; Claude native 2.1.304; Cursor editor 3.1.15. No live deletion. Full integrated gates pending.

2026-10-20 final QA: directory-wide launcher protection and fail-closed symlink resolution passed nix develop -c just check; focused real-kernel E2E codex_current_internal_dotdot_is_ambiguous and codex_version_running_binary_is_in_use both passed. Model fs_dry_run 39/39 passed. Read-only host survey after launcher-directory protection: Cursor agent 32/29 eligible/2 linked current; Codex 12/5 eligible/7 linked current; Claude Code 3/1 eligible/1 linked current; Cursor extensions 32/0 eligible, all 32 installed in extensions.json. No live deletion. Integrated full E2E remains for parent integration gate.

2026-10-20 integrated guard mutation bites in fs_dry_run: cursor-agent-versions clears protect_link_dirs and exposes a linked old v0; codex-releases removes protect_parent_link and exposes current release v1; claude-code-versions clears protect_link_dirs and exposes linked binary 1; cursor-extensions removes protect_cursor_extensions and exposes installed old acme.tool-1.0.0. Each unmutated built-in retains the same candidate. Optional absent ~/bin now means empty, but unsafe/unreadable directories still retain. Explicit YAML null cannot disable any new version guard. Cursor extension grouping parses an anchored three-number version and known platform suffix; two numeric publisher IDs keep their own newest two, ambiguous names retain. Focused model/parser tests pass; full final integrated E2E pending.

2026-10-20 final integrated gate: nix develop -c just check passed; nix develop -c just e2e passed 107/107. Independent QA and MPED architecture reviews returned GO. Earlier notes describing a pending gate are superseded by this result. No host deletion.
<!-- SECTION:NOTES:END -->
