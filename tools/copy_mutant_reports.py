#!/usr/bin/env python3
"""Copy a live container's tar stream without crossing the host disk reserve."""

import os
from pathlib import Path
import sys
import tarfile

if __package__:
    from .mutation_report import validate_report
else:
    from mutation_report import validate_report


def free_bytes(path: Path) -> int:
    fs = os.statvfs(path)
    return fs.f_bavail * fs.f_frsize


def copy_reports(destination: Path, floor: int, reserve: int, limit: int) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    written = 0
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as archive:
        for member in archive:
            if member.name in (".", "./") and member.isdir():
                continue
            parts = Path(member.name).parts
            if not parts or member.name.startswith("/") or ".." in parts:
                raise ValueError(f"unsafe report path: {member.name!r}")
            target = destination.joinpath(*parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(f"unexpected report entry: {member.name!r}")
            if written + member.size > limit:
                raise ValueError(f"report exceeds {limit} byte limit")
            target.parent.mkdir(parents=True, exist_ok=True)
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(f"missing report data: {member.name!r}")
            with target.open("xb") as output:
                while chunk := source.read(64 * 1024):
                    if free_bytes(destination) < floor + reserve + len(chunk):
                        raise OSError("host disk reserve reached during report copy")
                    output.write(chunk)
                    written += len(chunk)
    if free_bytes(destination) < floor + reserve:
        raise OSError("host disk reserve reached after report copy")


def verify_selection(destination: Path, selected_file: Path) -> None:
    expected = selected_file.read_text().splitlines()
    # Preserve valid reports with surviving mutants for diagnosis. The final
    # sweep verifier rejects them and names each survivor for follow-up.
    validate_report(destination / "mutants.out", expected, require_caught=False)


def main() -> int:
    if len(sys.argv) != 6:
        print("usage: copy_mutant_reports.py DEST SELECTED FLOOR RESERVE LIMIT", file=sys.stderr)
        return 2
    destination = Path(sys.argv[1])
    try:
        copy_reports(destination, *(int(value) for value in sys.argv[3:6]))
        verify_selection(destination, Path(sys.argv[2]))
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print(f"report copy/validation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
