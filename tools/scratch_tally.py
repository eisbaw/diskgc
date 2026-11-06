#!/usr/bin/env python3
"""Filesystem-bounded disk tally with a small, crash-safe history."""

import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from collections import deque
from datetime import datetime, timezone


GIB = 1024**3
WORKTREE = Path(__file__).resolve().parent.parent
REPO = Path(
    subprocess.check_output(
        ["git", "-C", str(WORKTREE), "rev-parse", "--path-format=absolute", "--git-common-dir"],
        text=True,
    ).strip()
).parent
SCRATCH = REPO / "tmp"
HISTORY = SCRATCH / "scratch-tallies.jsonl"
HISTORY_LOCK = SCRATCH / "scratch-tallies.lock"
MAX_HISTORY_BYTES = 1024 * 1024
MAX_HISTORY_RECORDS = 1024


def sizes(paths, unreadable):
    """Bytes below each path, by `du`; what it could not read is appended to
    `unreadable` (its messages) and the sizes are then lower bounds.

    A rootless container run leaves overlay `work/work` directories that its
    user cannot read (TASK-330): `du` still prints a total for each path and
    exits 1, which is reported, not fatal. A path without a total is an
    error.
    """
    if not paths:
        return {}
    result = subprocess.run(
        ["du", "-x", "-B1", "-s", "-0", "--", *map(os.fspath, paths)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    values = {}
    for row in result.stdout.split(b"\0"):
        if row:
            size, name = row.split(b"\t", 1)
            values[os.fsdecode(name)] = int(size)
    missing = [os.fspath(p) for p in paths if os.fspath(p) not in values]
    if missing:
        raise ValueError(
            f"du printed no total for {missing} (exit {result.returncode}): "
            f"{os.fsdecode(result.stderr).strip()}"
        )
    if result.returncode != 0:
        unreadable.extend(line for line in os.fsdecode(result.stderr).splitlines() if line)
    return values


def same_fs_entries(directory, prefix, device):
    entries = []
    skipped = []
    with os.scandir(directory) as scan:
        for entry in scan:
            if not entry.name.startswith(prefix):
                continue
            if entry.stat(follow_symlinks=False).st_dev == device:
                entries.append(Path(entry.path))
            else:
                skipped.append(entry.name)
    return sorted(entries), sorted(skipped)


def gib(value):
    return f"{value / GIB:.2f} GiB"


def read_records(history):
    """Read at most the tail of the history, retaining at most 1,024 valid rows."""
    history.seek(0, os.SEEK_END)
    length = history.tell()
    history.seek(max(0, length - MAX_HISTORY_BYTES))
    if length > MAX_HISTORY_BYTES:
        history.readline()  # The first tail fragment may be a partial record.
    records = deque(maxlen=MAX_HISTORY_RECORDS)
    corrupt = 0
    keys = ("scratch_bytes", "legacy_tmp_bytes", "root_target_bytes", "available_bytes")
    for line in history:
        try:
            record = json.loads(line)
            if not all(isinstance(record.get(key), int) for key in keys):
                raise ValueError("incomplete tally record")
            normalized = {key: record[key] for key in keys}
            if isinstance(record.get("at"), str):
                normalized["at"] = record["at"][:40]
            records.append(normalized)
        except (ValueError, AttributeError):
            corrupt += 1
    return records, corrupt


def record_tally(current):
    """Keep the old history intact until its replacement is fully written."""
    lock_descriptor = os.open(HISTORY_LOCK, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_descriptor, "r+b") as lock:
        if not stat.S_ISREG(os.fstat(lock.fileno()).st_mode):
            raise ValueError(f"history lock is not a regular file: {HISTORY_LOCK}")
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            descriptor = os.open(HISTORY, os.O_RDONLY | os.O_NOFOLLOW)
        except FileNotFoundError:
            records, corrupt = deque(maxlen=MAX_HISTORY_RECORDS), 0
        else:
            with os.fdopen(descriptor, "rb") as history:
                if not stat.S_ISREG(os.fstat(history.fileno()).st_mode):
                    raise ValueError(f"history is not a regular file: {HISTORY}")
                records, corrupt = read_records(history)
        previous = records[-1] if records else None
        records.append(current)
        descriptor, replacement = tempfile.mkstemp(prefix=".scratch-tallies-", dir=SCRATCH)
        try:
            with os.fdopen(descriptor, "wb") as output:
                output.writelines(
                    (json.dumps(record, sort_keys=True) + "\n").encode("ascii")
                    for record in records
                )
                output.flush()
                os.fsync(output.fileno())
            os.replace(replacement, HISTORY)
            directory_fd = os.open(SCRATCH, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        finally:
            if os.path.exists(replacement):
                os.unlink(replacement)  # Only our incomplete metadata replacement.
        return previous, corrupt


def main():
    repo_device = REPO.stat().st_dev
    tmp_device = Path("/tmp").stat().st_dev
    if SCRATCH.is_symlink():
        raise ValueError(f"refusing symlink scratch root: {SCRATCH}")
    SCRATCH.mkdir(exist_ok=True)
    if SCRATCH.stat().st_dev != repo_device:
        raise ValueError(f"scratch root is on another filesystem: {SCRATCH}")

    children, skipped_children = same_fs_entries(SCRATCH, "", repo_device)
    legacy, skipped_legacy = same_fs_entries(Path("/tmp"), "diskgc", tmp_device)
    unreadable = []
    child_sizes = sizes(children, unreadable)
    legacy_sizes = sizes(legacy, unreadable)
    scratch_bytes = sizes([SCRATCH], unreadable)[str(SCRATCH)]
    root_target = REPO / "target"
    root_target_bytes = (
        sizes([root_target], unreadable)[str(root_target)] if root_target.exists() else 0
    )
    legacy_bytes = sum(legacy_sizes.values())
    filesystem = os.statvfs(REPO)
    capacity = filesystem.f_blocks * filesystem.f_frsize
    available = filesystem.f_bavail * filesystem.f_frsize
    current = {
        "at": datetime.now(timezone.utc).isoformat(),
        "scratch_bytes": scratch_bytes,
        "legacy_tmp_bytes": legacy_bytes,
        "root_target_bytes": root_target_bytes,
        "available_bytes": available,
    }

    previous, corrupt = record_tally(current)

    print(f"Project tmp: {gib(scratch_bytes)} ({SCRATCH})")
    for path, value in sorted(child_sizes.items(), key=lambda item: (-item[1], item[0])):
        print(f"  {gib(value):>12}  {json.dumps(Path(path).name)}")
    print(f"Legacy /tmp/diskgc*: {gib(legacy_bytes)} across {len(legacy)} entries")
    print(f"Legacy root target/: {gib(root_target_bytes)}")
    free_percent = available * 100 / capacity
    print(f"Available: {gib(available)} ({free_percent:.1f}% of filesystem)")
    if skipped_children or skipped_legacy:
        print(
            f"Skipped other-filesystem entries: tmp={len(skipped_children)}, "
            f"legacy={len(skipped_legacy)}"
        )
    if unreadable:
        shown = sorted(set(unreadable))
        print(
            f"WARNING: du reported {len(shown)} errors, so the sizes above are lower bounds "
            "(an unreadable rootless overlay work/work directory is one cause):"
        )
        for line in shown[:5]:
            print(f"  {line}")
        if len(shown) > 5:
            print(f"  ... and {len(shown) - 5} more")
    if corrupt:
        print(f"WARNING: ignored {corrupt} incomplete or corrupt tally history records")
    if previous is not None:
        for label, key in (
            ("Project tmp change", "scratch_bytes"),
            ("Legacy development scratch change", "legacy_tmp_bytes"),
            ("Root target change", "root_target_bytes"),
        ):
            change = current[key] - previous[key]
            print(f"{label} since previous tally: {change / GIB:+.2f} GiB")
        growth = sum(
            current[key] - previous[key]
            for key in ("scratch_bytes", "legacy_tmp_bytes", "root_target_bytes")
        )
        if growth >= 2 * GIB:
            print(f"WARNING: tracked scratch grew by {gib(growth)} since the previous tally")
    if free_percent < 10 or available < 20 * GIB:
        print("WARNING: free space is below 10% or 20 GiB")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"scratch tally failed: {error}", file=sys.stderr)
        sys.exit(1)
