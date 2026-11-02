#!/usr/bin/env python3
"""Run one heavy e2e stage behind a free-space guard, printing free space
before and after it (docs/testing.md, Commands: Disk budgets; TASK-311).

Usage: python3 tools/disk_stage.py STAGE COMMAND [ARG...]

Before COMMAND starts, every filesystem it writes to (the checkout, the
Nix store, and for container stages Podman's graph root) must have at
least STAGE's budget free; below it, COMMAND does not run and the exit is
2 with a message naming the stage, the filesystem and both numbers. A
budget is the stage's measured peak use, rounded up to whole GiB, plus
4 GiB left for the rest of the host (docs/testing.md has the
measurements). Lowering one to make a run start
defeats it: free space instead (`just e2e-kept --drop`,
`just e2e-images --apply`, and with Mark's OK `nix-store --gc`).

While COMMAND runs, free space is sampled every second; afterwards the
stage prints free space before and after, the difference, and the peak
use (the lowest free space seen). Other activity on the host shows up in
these numbers too. Exit: COMMAND's exit status, 2 when refused, 130 when
interrupted."""

import os
import subprocess
import sys
import threading
import time
from pathlib import Path

GIB = 2**30
MIB = 2**20

# STAGE: (bytes that must be free to start, whether Podman storage is
# written). The table in docs/testing.md (Disk budgets) states the same
# numbers with their measurements; tools/test_disk_stage.py keeps the two
# equal.
STAGES = {
    "check": (10 * GIB, False),
    "flake-check": (6 * GIB, False),
    "containers": (17 * GIB, True),
    "vm": (8 * GIB, False),
    "vm-real-kernel": (8 * GIB, False),
    "vm-cold-scale": (8 * GIB, False),
    "fuzz": (10 * GIB, False),
    # Never below mutants_container_smoke.sh's own guard (16 GiB plus 64 MiB).
    "mutants": (16 * GIB + 64 * MIB, True),
}

SAMPLE_SECS = 1.0


def free_bytes(path):
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize


def filesystems(roles):
    """{representative path: "role+role"} with one path per filesystem
    (by st_dev) among `roles` ({role: path})."""
    by_device = {}
    for role, path in roles.items():
        by_device.setdefault(os.stat(path).st_dev, []).append((role, str(path)))
    return {group[0][1]: "+".join(role for role, _ in group) for group in by_device.values()}


def shortfalls(free, budget):
    """[(path, free bytes)] of the filesystems below `budget`."""
    return [(path, have) for path, have in free.items() if have < budget]


def gib(n):
    return f"{n / GIB:.2f} GiB"


def podman_graph_root():
    found = subprocess.run(
        ["podman", "info", "--format", "{{.Store.GraphRoot}}"],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    if found.returncode != 0 or not found.stdout.strip():
        raise RuntimeError(f"podman info (graph root) failed: {found.stderr.strip()}")
    return found.stdout.strip()


class LowestFree:
    """Samples free space on `paths` every SAMPLE_SECS until stopped."""

    def __init__(self, paths):
        self.lowest = {path: free_bytes(path) for path in paths}
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stopped.wait(SAMPLE_SECS):
            for path in self.lowest:
                self.lowest[path] = min(self.lowest[path], free_bytes(path))

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stopped.set()
        self.thread.join()


def report(stage, before, after, lowest, seconds, names):
    lines = []
    for path, was in before.items():
        now = after[path]
        lines.append(
            f"disk: {stage}: {names[path]}: before {gib(was)} free, after {gib(now)}, "
            f"delta {(now - was) / GIB:+.2f} GiB, peak use {gib(was - lowest[path])} "
            f"(lowest free {gib(lowest[path])}), {seconds:.0f} s"
        )
    return "\n".join(lines)


def main(argv):
    if len(argv) < 3 or argv[1] not in STAGES:
        print(f"{__doc__}\nstages: {', '.join(STAGES)}", file=sys.stderr)
        return 2
    stage, command = argv[1], argv[2:]
    budget, uses_podman = STAGES[stage]
    roles = {"checkout": Path.cwd(), "nix-store": Path("/nix/store")}
    try:
        if uses_podman:
            roles["podman"] = Path(podman_graph_root())
        names = filesystems(roles)
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"disk: {stage}: {error}", file=sys.stderr)
        return 2
    before = {path: free_bytes(path) for path in names}
    short = shortfalls(before, budget)
    if short:
        for path, have in short:
            print(
                f"disk: {stage}: REFUSED: the filesystem of {names[path]} ({path}) has {gib(have)} free, the stage's "
                f"budget is {gib(budget)} (docs/testing.md, Disk budgets). Free space "
                "(`just e2e-kept --drop`, `just e2e-images --apply`; `nix-store --gc` is the "
                "machine owner's call) and rerun; do not lower the budget.",
                file=sys.stderr,
            )
        return 2
    print(f"disk: {stage}: budget {gib(budget)}; " + ", ".join(
        f"{names[p]} {gib(b)} free" for p, b in before.items()), flush=True)
    started = time.monotonic()
    with LowestFree(list(before)) as sampler:
        try:
            child = subprocess.Popen(command)
        except OSError as error:
            print(f"disk: {stage}: cannot run {command[0]}: {error}", file=sys.stderr)
            return 127
        try:
            code = child.wait()
        except KeyboardInterrupt:
            code = child.wait()
            code = 130 if code >= 0 else code
    after = {path: free_bytes(path) for path in before}
    lowest = {path: min(sampler.lowest[path], after[path]) for path in before}
    print(report(stage, before, after, lowest, time.monotonic() - started, names), flush=True)
    return 128 - code if code < 0 else code


if __name__ == "__main__":
    sys.exit(main(sys.argv))
