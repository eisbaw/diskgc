#!/usr/bin/env python3
"""Read-only, path-level inventory of legacy diskgc development scratch."""

import argparse
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

from scratch_tally import GIB, REPO, same_fs_entries, sizes


def registered_worktrees():
    output = subprocess.check_output(
        ["git", "worktree", "list", "--porcelain"], cwd=REPO, text=True
    )
    result = {}
    for block in output.strip().split("\n\n"):
        fields = dict(line.split(" ", 1) for line in block.splitlines() if " " in line)
        path = fields.get("worktree")
        if path:
            result[path] = {
                "branch": fields.get("branch"),
                "head": fields.get("HEAD"),
            }
    return result


def top_level_tmp_path(link):
    link = link.removesuffix(" (deleted)")
    if not link.startswith("/tmp/diskgc"):
        return None
    name = link.removeprefix("/tmp/").split("/", 1)[0]
    return "/tmp/" + name if name.startswith("diskgc") else None


def process_references():
    refs = {}
    unreadable = 0
    for process in Path("/proc").iterdir():
        if not process.name.isdigit():
            continue
        links = [process / "cwd"]
        try:
            links.extend((process / "fd").iterdir())
        except (OSError, PermissionError):
            unreadable += 1
        for link in links:
            try:
                target = os.readlink(link)
            except (OSError, PermissionError):
                continue
            path = top_level_tmp_path(target)
            if path:
                refs.setdefault(path, set()).add(int(process.name))
        try:
            maps = (process / "maps").read_text(errors="replace")
        except (OSError, PermissionError):
            continue
        for line in maps.splitlines():
            fields = line.split(maxsplit=5)
            path = top_level_tmp_path(fields[5]) if len(fields) == 6 else None
            if path:
                refs.setdefault(path, set()).add(int(process.name))
    return refs, unreadable


def dirty_entries(path):
    environment = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
    result = subprocess.run(
        ["git", "-C", path, "status", "--porcelain", "--untracked-files=normal"],
        env=environment,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        return None, result.stderr.strip()
    return len(result.stdout.splitlines()), None


def inventory():
    tmp = Path("/tmp")
    entries, skipped = same_fs_entries(tmp, "diskgc", tmp.stat().st_dev)
    worktrees = registered_worktrees()
    refs, unreadable = process_references()
    entry_sizes = sizes(entries)
    targets = []
    for path in entries:
        if not stat.S_ISDIR(path.lstat().st_mode):
            continue  # A symlink here must not redirect the nested target probe.
        target = path / "target"
        try:
            target_stat = target.lstat()
        except (FileNotFoundError, NotADirectoryError):
            continue
        if stat.S_ISDIR(target_stat.st_mode) and target_stat.st_dev == tmp.stat().st_dev:
            targets.append(target)
    target_sizes = sizes(targets)
    rows = []
    for path in entries:
        registered = worktrees.get(str(path))
        dirty, error = dirty_entries(str(path)) if registered else (None, None)
        rows.append(
            {
                "path": str(path),
                "bytes": entry_sizes[str(path)],
                "kind": "worktree"
                if registered
                else ("directory" if stat.S_ISDIR(path.lstat().st_mode) else "file"),
                "branch": registered["branch"] if registered else None,
                "head": registered["head"] if registered else None,
                "dirty_entries": dirty,
                "status_error": error,
                "target_bytes": target_sizes.get(str(path / "target"), 0),
                "observed_pids": sorted(refs.get(str(path), set())),
            }
        )
    rows.sort(key=lambda row: (-row["bytes"], row["path"]))
    summary = {
        "entries": len(rows),
        "bytes": sum(row["bytes"] for row in rows),
        "registered_worktrees": sum(row["kind"] == "worktree" for row in rows),
        "dirty_worktrees": sum(
            row["kind"] == "worktree" and row["dirty_entries"] not in (None, 0)
            for row in rows
        ),
        "status_errors": sum(row["status_error"] is not None for row in rows),
        "observed_processes": len({pid for row in rows for pid in row["observed_pids"]}),
        "unreadable_process_fd_tables": unreadable,
        "skipped_other_filesystem": skipped,
    }
    return summary, rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit every path as JSON lines")
    args = parser.parse_args()
    summary, rows = inventory()
    if args.json:
        print(json.dumps({"summary": summary}, sort_keys=True))
        for row in rows:
            print(json.dumps(row, sort_keys=True))
    else:
        print(
            f"{summary['entries']} entries, {summary['registered_worktrees']} registered "
            f"worktrees ({summary['dirty_worktrees']} dirty), "
            f"{summary['observed_processes']} observed referencing processes"
        )
        print(
            f"{summary['status_errors']} Git status errors; "
            f"{summary['unreadable_process_fd_tables']} process fd tables unreadable"
        )
        for row in rows[:20]:
            state = (
                "n/a"
                if row["dirty_entries"] is None
                else "dirty" if row["dirty_entries"] else "clean"
            )
            print(f"{row['bytes'] / GIB:6.2f} GiB  {row['kind']:9} {state:5} {row['path']}")
        print("Use --json to see every path and its branch, build bytes and process refs.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"scratch inventory failed: {error}", file=sys.stderr)
        sys.exit(1)
