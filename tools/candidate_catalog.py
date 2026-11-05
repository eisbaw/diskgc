#!/usr/bin/env python3
"""Put named built-ins into a release catalog in a COPY of builtin.yaml.

Test-only (TASK-350; why: docs/testing.md, Admission evidence). The flake
runs it on the candidate-catalog build's own copy of the source, never on
the checked-in file, whose `releases:` lines stay the one home of what a
release ships.

usage: candidate_catalog.py FILE RELEASE LIST

LIST is a file of profile names, one per line (`#` and blank lines
ignored): diskgc-e2e/candidate-catalog, the one home of the candidates.
Each NAME must be a top-level `- name: NAME` entry with a
`  liveness_class:` line and no `  releases:` line yet (a built-in already
in some catalog is a mistake in the candidate list, not something to
merge). The line `  releases: ["RELEASE"]` is inserted after its
`liveness_class`. Any violation exits 1 naming the profile; nothing is
written then.
"""

import re
import sys
from pathlib import Path

ENTRY = re.compile(r"^- name: (\S+)\s*$")
RELEASE = re.compile(r"^\d+\.\d+$")


def entry_spans(lines):
    """Map each top-level profile name to its [start, end) line span.

    Every top-level list item (`- ` at column 0) starts an entry, and each
    must be exactly `- name: NAME`: anything else would otherwise fall into
    the previous entry's span and could be patched as that profile.
    """
    starts = []
    for i, line in enumerate(lines):
        if not line.startswith("- "):
            continue
        match = ENTRY.match(line)
        if not match:
            raise ValueError(f"line {i + 1} starts an entry that is not `- name: NAME`: {line.rstrip()!r}")
        starts.append((i, match.group(1)))
    spans = {}
    for n, (start, name) in enumerate(starts):
        end = starts[n + 1][0] if n + 1 < len(starts) else len(lines)
        if name in spans:
            raise ValueError(f"profile {name} is defined twice")
        spans[name] = (start, end)
    return spans


def read_list(path):
    """The profile names in a candidate list file."""
    names = []
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.append(line)
    return names


def admit(text, release, names):
    """Return text with `releases: [release]` added to each named entry."""
    if not RELEASE.match(release):
        raise ValueError(f"release {release!r} is not MAJOR.MINOR")
    if not names:
        raise ValueError("no profile named")
    if len(set(names)) != len(names):
        raise ValueError(f"a profile is named twice: {names}")
    lines = text.splitlines(keepends=True)
    spans = entry_spans(lines)
    inserts = []
    for name in names:
        if name not in spans:
            raise ValueError(f"profile {name} is not a top-level entry")
        start, end = spans[name]
        body = lines[start:end]
        if any(line.startswith("  releases:") for line in body):
            raise ValueError(f"profile {name} already has a releases line")
        classes = [i for i, line in enumerate(body) if line.startswith("  liveness_class:")]
        if len(classes) != 1:
            raise ValueError(f"profile {name} has {len(classes)} liveness_class lines, not one")
        inserts.append(start + classes[0] + 1)
    for at in sorted(inserts, reverse=True):
        lines.insert(at, f'  releases: ["{release}"]\n')
    return "".join(lines)


def main(argv):
    if len(argv) != 3:
        print("usage: candidate_catalog.py FILE RELEASE LIST", file=sys.stderr)
        return 2
    path, release = Path(argv[0]), argv[1]
    try:
        names = read_list(argv[2])
        patched = admit(path.read_text(), release, names)
    except (OSError, ValueError) as error:
        print(f"candidate_catalog: {path}: {error}", file=sys.stderr)
        return 1
    path.write_text(patched)
    print(f"candidate_catalog: {path}: {', '.join(names)} added to catalog {release}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
