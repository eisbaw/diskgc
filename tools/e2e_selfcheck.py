#!/usr/bin/env python3
"""Check that an e2e run left nothing of its own behind (docs/testing.md,
Commands: Cleanup; TASK-311).

Usage (from the checkout's root, as `just` runs it):
  python3 tools/e2e_selfcheck.py snapshot   (first line of `just e2e`)
  python3 tools/e2e_selfcheck.py verify     (after its cleanup lines)

`snapshot` records what exists before the run in SNAPSHOT; `verify`
compares the host with it after the run's own cleanup (`e2e_kept.py
--prune`, `e2e_images.py --apply`, `e2e_gcroots.py --apply`) and fails,
listing each, on anything new that is not in the retained set:
  containers  a container labelled with this checkout (runner.rs
              CHECKOUT_LABEL); `just e2e` keeps none;
  volumes     a Podman volume (an anonymous volume left by a removed
              container);
  images      a harness image (localhost/diskgc-e2e*) that is neither one a
              runner of this checkout records (.gcroots/*/share/diskgc-e2e/
              image-ref) nor stamped as used within the grace period
              (tools/e2e_images.py);
  scratch     an entry in this checkout's tmp/system ($TMPDIR of the dev
              shell's host tests), or in /tmp or $XDG_RUNTIME_DIR with a
              name a test, VM driver or Nix build would leave (SCRATCH_NAMES;
              other programs' entries are not ours and not checked);
  worktree    a new untracked file in the source tree (`git status
              --porcelain`), which the run wrote there.
Other worktrees run concurrently: only what can be attributed to this
checkout fails the check, so a leak by a parallel run elsewhere is not
caught here. Exit: 0 clean, 1 leftovers, 2 on a usage or environment error."""

import fnmatch
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import e2e_images
from e2e_kept import CHECKOUT_LABEL

SNAPSHOT = Path("tmp/e2e-selfcheck/before.json")
SCRATCH_NAMES = ["diskgc*", "nix-build-*", "vm-state-*", "*.qcow2"]
# Lock files the harness and diskgc keep by design (runner.rs host_pid_lock,
# the host lock under $XDG_RUNTIME_DIR/diskgc).
RETAINED_RUNTIME = {"diskgc", "diskgc-e2e-host-pid.lock"}


def run(argv):
    done = subprocess.run(argv, capture_output=True, text=True, timeout=120, check=False)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(argv)} failed ({done.returncode}): {done.stderr.strip()}")
    return done.stdout


def scratch_names(directory, patterns, retained=()):
    try:
        names = os.listdir(directory)
    except FileNotFoundError:
        return []
    return sorted(
        n for n in names if n not in retained and any(fnmatch.fnmatch(n, p) for p in patterns)
    )


def state(checkout):
    """Everything `verify` compares, as JSON-friendly lists."""
    containers = json.loads(
        run(["podman", "ps", "--all", "--filter", f"label={CHECKOUT_LABEL}={checkout}", "--format", "json"])
    )
    images = json.loads(run(["podman", "images", "--format", "json"]))
    runtime = os.environ.get("XDG_RUNTIME_DIR", "")
    return {
        "containers": sorted(c["Names"][0] for c in containers),
        "volumes": sorted(run(["podman", "volume", "ls", "--quiet"]).split()),
        "images": sorted(
            {ref for image in images for ref in image.get("Names") or [] if e2e_images.is_harness_ref(ref)}
        ),
        "tmp_system": sorted(os.listdir(checkout / "tmp" / "system"))
        if (checkout / "tmp" / "system").is_dir()
        else [],
        "tmp": scratch_names("/tmp", SCRATCH_NAMES),
        "runtime": scratch_names(runtime, SCRATCH_NAMES, RETAINED_RUNTIME) if runtime else [],
        "worktree": sorted(
            line
            for line in run(
                ["git", "-C", str(checkout), "status", "--porcelain", "--untracked-files=all"]
            ).splitlines()
            if line.startswith("??")
        ),
    }


def runner_refs(checkout):
    """The image tags this checkout's runners record."""
    refs = set()
    for runner in (checkout / ".gcroots").glob("*"):
        path = runner / e2e_images.RUNNER_IMAGE_REF
        if path.is_file():
            refs.add(path.read_text().strip())
    return refs


def leftovers(before, after, retained_images):
    """{kind: [new entries]} of what `after` has beyond `before`, images
    in `retained_images` excepted."""
    found = {}
    for kind, entries in after.items():
        new = sorted(set(entries) - set(before.get(kind, [])))
        if kind == "images":
            new = [ref for ref in new if ref not in retained_images]
        if new:
            found[kind] = new
    return found


def retained_images(checkout, now):
    stamps = e2e_images.stamp_ages(e2e_images.stamp_dir(), now)
    grace = e2e_images.DEFAULT_GRACE_HOURS * 3600
    recent = {ref for ref, age in stamps.items() if age < grace}
    return runner_refs(checkout) | recent


def main(argv):
    if argv[1:] not in (["snapshot"], ["verify"]):
        print(__doc__, file=sys.stderr)
        return 2
    checkout = Path.cwd().resolve()
    try:
        now_state = state(checkout)
        if argv[1] == "snapshot":
            SNAPSHOT.parent.mkdir(parents=True, exist_ok=True)
            SNAPSHOT.write_text(json.dumps(now_state, indent=1) + "\n")
            print(f"e2e-selfcheck: snapshot of {checkout} in {SNAPSHOT}")
            return 0
        before = json.loads(SNAPSHOT.read_text())
        found = leftovers(before, now_state, retained_images(checkout, time.time()))
    except (RuntimeError, OSError, KeyError, IndexError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"e2e-selfcheck: {error}", file=sys.stderr)
        return 2
    if found:
        for kind, entries in found.items():
            for entry in entries:
                print(f"e2e-selfcheck: LEFT BEHIND {kind}: {entry}", file=sys.stderr)
        print("e2e-selfcheck: FAILED: the run left the above behind", file=sys.stderr)
        return 1
    print(
        f"e2e-selfcheck: OK: nothing new beyond the retained set "
        f"({len(now_state['containers'])} labelled container(s) of this checkout, "
        f"{len(now_state['images'])} harness image tag(s), as before or retained)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
