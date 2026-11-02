#!/usr/bin/env python3
"""List, and with --apply drop, a checkout's stale e2e GC roots
(docs/testing.md, Container setup: GC roots; TASK-311).

Usage: python3 tools/e2e_gcroots.py [--apply] [CHECKOUT]   (default: .)

The container recipes link each runner (`just _runner NAME`) and the dev
shell (`_gcroot-dev`) under CHECKOUT/.gcroots/NAME; each link is a Nix GC
root holding a multi-gigabyte closure. A link whose name no recipe of
CHECKOUT's justfile builds any more (a renamed or retired runner, e.g.
e2e-runner-release-01) roots its closure forever. The names in use are
read from the justfile's text: every literal `.gcroots/NAME` and every
`_runner NAME`. Only symlinks into /nix/store are ever dropped; anything
else under .gcroots/ is reported and left alone. Dropping a link frees
nothing by itself: Nix's GC (manual, `nix-store --gc`) does.

A removed worktree takes its .gcroots/ with it (`git worktree remove`
deletes ignored files too), so its roots need nothing more.
Exit: 0, 1 when a removal failed, 2 on a usage or environment error."""

import os
import re
import sys
from pathlib import Path

LITERAL_ROOT = re.compile(r"\.gcroots/([A-Za-z0-9][A-Za-z0-9_.-]*)")
RUNNER_CALL = re.compile(r"\b_runner\s+([A-Za-z0-9][A-Za-z0-9_.-]*)")
STORE = "/nix/store/"


def names_in_use(justfile_text):
    """The .gcroots names the justfile's recipes build."""
    return set(LITERAL_ROOT.findall(justfile_text)) | set(RUNNER_CALL.findall(justfile_text))


def plan(entries, in_use):
    """(name, verdict, target) for each (name, link target or None) entry
    of .gcroots/: `in-use`, `stale` (a store link no recipe builds) or
    `foreign` (not a store link: never touched)."""
    rows = []
    for name, target in sorted(entries):
        if target is None or not target.startswith(STORE):
            verdict = "foreign"
        elif name in in_use:
            verdict = "in-use"
        else:
            verdict = "stale"
        rows.append((name, verdict, target))
    return rows


def entries_of(roots_dir):
    if not roots_dir.is_dir():
        return []
    found = []
    for path in roots_dir.iterdir():
        target = os.readlink(path) if path.is_symlink() else None
        found.append((path.name, target))
    return found


def main(argv):
    args = argv[1:]
    apply = "--apply" in args
    rest = [a for a in args if a != "--apply"]
    if len(rest) > 1 or any(a.startswith("-") for a in rest):
        print(__doc__, file=sys.stderr)
        return 2
    checkout = Path(rest[0] if rest else ".").resolve()
    try:
        in_use = names_in_use((checkout / "justfile").read_text())
        rows = plan(entries_of(checkout / ".gcroots"), in_use)
    except OSError as error:
        print(f"e2e-gcroots: {error}", file=sys.stderr)
        return 2
    if not in_use:
        print(f"e2e-gcroots: {checkout}/justfile names no GC root; refusing", file=sys.stderr)
        return 2
    for name, verdict, target in rows:
        print(f"{verdict:<8} {checkout}/.gcroots/{name} -> {target}")
    stale = [name for name, verdict, _ in rows if verdict == "stale"]
    if not apply:
        print(f"{len(stale)} stale root(s); dry run: `just e2e-gcroots --apply` drops them")
        return 0
    failed = 0
    for name in stale:
        try:
            (checkout / ".gcroots" / name).unlink()
            print(f"dropped  {checkout}/.gcroots/{name}")
        except OSError as error:
            print(f"failed   {checkout}/.gcroots/{name}: {error}", file=sys.stderr)
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
