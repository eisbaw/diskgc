#!/usr/bin/env python3
"""List, prune or drop the containers the e2e runner kept (docs/testing.md,
Commands; TASK-311).

Usage (from the checkout's root, as `just` runs it):
  python3 tools/e2e_kept.py [--prune | --drop] [--legacy] [--keep N]

`e2e-one` keeps a failed test's container for inspection. The harness's
containers are exactly those named `diskgc-e2e-...` that carry the label
`diskgc-e2e.checkout=<checkout>` (runner.rs CHECKOUT_LABEL, set when the
runner creates them); nothing else is ever listed as removable or removed.
Only exited containers are candidates: a running or created one belongs to
a run in progress.

Each exited harness container is one of:
  kept     this checkout's, among its newest N (default 5): kept by --prune;
  surplus  this checkout's, older than its newest N: removed by --prune;
  orphan   its checkout no longer exists (worktree removed): removed by
           --prune;
  other    another live checkout's: never removed from here;
  legacy   named `diskgc-e2e-<pid>-...` on a harness image but without the
           label (made before TASK-311, or by a checkout whose runner
           predates it), created over 24 h ago: removed only by
           --drop --legacy; younger ones are `legacy-recent` and stay, as
           someone may be inspecting them.
--drop removes this checkout's kept and surplus ones and the orphans (with
--legacy, the legacy ones too). Removal is `podman rm --volumes` (never
--force), so a container someone restarted meanwhile stays.
`_runner` runs --prune after every runner run, so a checkout keeps at most
N failed containers. Exit: 0, 1 when a removal failed, 2 on a usage or
environment error."""

import json
import re
import subprocess
import sys
import time
from pathlib import Path

from e2e_images import is_harness_ref

NAME_PREFIX = "diskgc-e2e-"
# runner.rs CHECKOUT_LABEL (tools/test_e2e_kept.py checks the mirror).
CHECKOUT_LABEL = "diskgc-e2e.checkout"
DEFAULT_KEEP = 5
LEGACY_NAME = re.compile(r"^diskgc-e2e-[0-9]+-")
LEGACY_MIN_AGE_SECS = 24 * 3600


def name_of(container):
    names = container.get("Names") or []
    return names[0] if names else container.get("Id", "?")[:12]


def classify(containers, checkout, keep, checkout_exists, now):
    """(container, verdict) for every harness container in `podman ps -a
    --format json` output, newest first; `checkout_exists(path)` says
    whether a labelled checkout still exists, `now` is the Unix time.
    Ones not exited are `active`."""
    checkout = str(checkout)
    rows, ours = [], 0
    for container in sorted(containers, key=lambda c: c.get("Created", 0), reverse=True):
        name = name_of(container)
        if not name.startswith(NAME_PREFIX):
            continue
        label = (container.get("Labels") or {}).get(CHECKOUT_LABEL)
        if label is None:
            if LEGACY_NAME.match(name) and is_harness_ref(container.get("Image", "")):
                if container.get("State") != "exited":
                    verdict = "active"
                elif now - container.get("Created", now) < LEGACY_MIN_AGE_SECS:
                    verdict = "legacy-recent"
                else:
                    verdict = "legacy"
                rows.append((container, verdict))
            continue
        if container.get("State") != "exited":
            verdict = "active"
        elif label == checkout:
            ours += 1
            verdict = "kept" if ours <= keep else "surplus"
        elif not checkout_exists(label):
            verdict = "orphan"
        else:
            verdict = "other"
        rows.append((container, verdict))
    return rows


def removable(rows, mode, legacy):
    """The containers `mode` (list, prune, drop) removes."""
    verdicts = {"list": set(), "prune": {"surplus", "orphan"}, "drop": {"kept", "surplus", "orphan"}}[mode]
    if mode == "drop" and legacy:
        verdicts = verdicts | {"legacy"}
    return [container for container, verdict in rows if verdict in verdicts]


def describe(rows, checkout):
    lines = []
    for container, verdict in rows:
        label = (container.get("Labels") or {}).get(CHECKOUT_LABEL, "-")
        where = "" if label == str(checkout) else f"  [{label}]"
        lines.append(
            f"{verdict:<8} {name_of(container)}  ({container.get('State')}, "
            f"{container.get('Status', '')}){where}"
        )
    counts = {}
    for _, verdict in rows:
        counts[verdict] = counts.get(verdict, 0) + 1
    summary = ", ".join(f"{n} {v}" for v, n in sorted(counts.items())) or "none"
    lines.append(f"harness containers: {summary}")
    return "\n".join(lines)


def remove(containers, rm):
    """`rm(name)` (True on success) for each; the number of failures."""
    failed = 0
    for container in containers:
        name = name_of(container)
        if rm(name):
            print(f"removed  {name}")
        else:
            failed += 1
    return failed


def podman_rm(name):
    removed = subprocess.run(
        ["podman", "rm", "--volumes", name], capture_output=True, text=True, timeout=120, check=False
    )
    if removed.returncode != 0:
        print(f"failed   {name}: {removed.stderr.strip()}", file=sys.stderr)
    return removed.returncode == 0


def podman_containers():
    listed = subprocess.run(
        ["podman", "ps", "--all", "--format", "json"],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    if listed.returncode != 0:
        raise RuntimeError(f"podman ps failed: {listed.stderr}")
    return json.loads(listed.stdout)


def parse(argv):
    """(mode, legacy, keep); a ValueError on a usage error."""
    mode, legacy, keep, rest = "list", False, DEFAULT_KEEP, list(argv)
    while rest:
        arg = rest.pop(0)
        if arg in ("--prune", "--drop") and mode == "list":
            mode = arg[2:]
        elif arg == "--legacy":
            legacy = True
        elif arg == "--keep" and rest:
            keep = int(rest.pop(0))
            if keep < 0:
                raise ValueError("--keep must be >= 0")
        else:
            raise ValueError(f"unexpected argument {arg!r}")
    return mode, legacy, keep


def main(argv):
    try:
        mode, legacy, keep = parse(argv[1:])
    except ValueError as error:
        print(f"e2e-kept: {error}\n{__doc__}", file=sys.stderr)
        return 2
    checkout = Path.cwd().resolve()
    try:
        rows = classify(podman_containers(), checkout, keep, lambda p: Path(p).is_dir(), time.time())
    except (RuntimeError, OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"e2e-kept: {error}", file=sys.stderr)
        return 2
    doomed = removable(rows, mode, legacy)
    if mode == "list":
        print(describe(rows, checkout))
        print("list only: `just e2e-kept --drop` removes this checkout's (`--legacy`: and unlabelled old ones)")
        return 0
    if mode == "drop":
        print(describe(rows, checkout))
    return 1 if remove(doomed, podman_rm) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
