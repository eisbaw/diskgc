#!/usr/bin/env python3
"""The retro table: one row per commit whose verification note carries the
phase3.1 retro lines (the phase3.1-shipping skill, "Retro lines"; it lives
in the user's home skills, not in this repository). Notes without a `task:` line are not tasks and are left out; a
missing key prints as `-`. Cycle time is merged - started when both parse."""

import subprocess
import sys
from datetime import datetime

KEYS = ["task", "started", "merged", "review_rounds", "walked", "found_by", "qa", "refix_of"]
RECORD_END = "\x1e"


def parse_note(text):
    """The retro keys of one note; the first value of a repeated key wins."""
    fields = {}
    for line in text.splitlines():
        key, sep, value = line.partition(":")
        key = key.strip()
        if sep and key in KEYS and key not in fields:
            fields[key] = value.split("#", 1)[0].strip()
    return fields


def parse_utc(text):
    for fmt in ("%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%MZ"):
        try:
            return datetime.strptime(text, fmt)
        except ValueError:
            pass
    return None


def cycle_hours(fields):
    started, merged = parse_utc(fields.get("started", "")), parse_utc(fields.get("merged", ""))
    return f"{(merged - started).total_seconds() / 3600:.1f}" if started and merged else "-"


def rows(log_text):
    for record in log_text.split(RECORD_END):
        sha, _, note = record.strip("\n").partition("\n")
        fields = parse_note(note)
        if sha and "task" in fields:
            yield [sha] + [fields.get(k, "-") or "-" for k in KEYS] + [cycle_hours(fields)]


def table(row_list):
    header = ["sha"] + KEYS + ["cycle_h"]
    widths = [max(len(str(c)) for c in col) for col in zip(header, *row_list)]
    return "\n".join("  ".join(c.ljust(w) for c, w in zip(r, widths)).rstrip() for r in [header] + row_list)


def main():
    log = subprocess.run(
        ["git", "log", "--notes=verification", f"--format=%h%n%N{RECORD_END}"],
        check=True, capture_output=True, text=True,
    ).stdout
    found = list(rows(log))
    if not found:
        print("retro: no verification note carries a `task:` retro line yet", file=sys.stderr)
        return
    print(table(found))


if __name__ == "__main__":
    main()
