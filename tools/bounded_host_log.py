#!/usr/bin/env python3
"""Write a subprocess stream to a host log with size and free-space limits."""

import os
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) != 5:
        print("usage: bounded_host_log.py PATH FLOOR RESERVE LIMIT", file=sys.stderr)
        return 2
    path = Path(sys.argv[1])
    floor, reserve, limit = (int(value) for value in sys.argv[2:5])
    written = 0
    try:
        with path.open("xb") as output:
            while chunk := sys.stdin.buffer.read1(64 * 1024):
                remaining = limit - written
                if remaining <= 0:
                    raise ValueError(f"log exceeds {limit} byte limit")
                over_limit = len(chunk) > remaining
                chunk = chunk[:remaining]
                fs = os.statvfs(path)
                if fs.f_bavail * fs.f_frsize < floor + reserve + len(chunk):
                    raise OSError("host disk reserve reached while writing run log")
                output.write(chunk)
                output.flush()
                written += len(chunk)
                if over_limit:
                    raise ValueError(f"log exceeds {limit} byte limit")
    except (OSError, ValueError) as error:
        print(f"bounded run log failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
