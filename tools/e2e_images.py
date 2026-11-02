#!/usr/bin/env python3
"""List, and with --apply remove, the e2e harness's old Podman image tags
(docs/testing.md, Commands; TASK-60, TASK-311).

Usage (from the repository root):
  python3 tools/e2e_images.py [--apply] [--grace-hours H]
  python3 tools/e2e_images.py stamp RUNNER_DIR...

The runners load each test image under `localhost/diskgc-e2e-NAME:TAG`, the
tag being the image's Nix hash, and never remove one; every input change
leaves the previous multi-gigabyte image behind. Which tags are current is
the flake's answer: every package with an `imageTag` (`nix eval`). Only
tags in the harness's own namespace (`localhost/diskgc-e2e` and
`localhost/diskgc-e2e-*`) are ever considered; every other image is left
alone and not listed.

Worktrees on other inputs have other current tags, so "not current here"
does not mean unused. Each tag therefore has a last-used stamp: an empty
file per tag in the per-user state directory (`stamp_dir()`: Podman's
image store is per user too), its mtime the last use. `just _runner`
stamps its runner's image (`stamp RUNNER_DIR`, from the runner package's
share/diskgc-e2e/image-ref) before every run in any worktree, and --apply
stamps this checkout's current tags. A tag never stamped gets its first
stamp from --apply, which starts its grace period.

Each harness tag is one of:
  current  the flake at this checkout uses it: kept;
  in-use   a container (running, stopped, or one `e2e-one` kept) uses it:
           kept; `podman rmi` without --force would refuse anyway;
  recent   used (or first seen) less than the grace period ago (default
           24 h), possibly by another worktree: kept;
  old      removed by --apply (`podman rmi TAG`, never --force), and its
           stamp with it.

Without --apply nothing is removed or stamped: the plan is printed. Layers
shared with a kept image are not freed. A worktree idle for longer than
the grace period loses its images and loads them again on its next run.
Exit: 0 (with --apply, when every removal succeeded), 1 when a removal
failed, 2 on a usage or environment error."""

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import quote, unquote

HARNESS_REPOSITORY = "localhost/diskgc-e2e"
DEFAULT_GRACE_HOURS = 24
# Where a runner package records its image (flake.nix, mkE2eRunner).
RUNNER_IMAGE_REF = Path("share/diskgc-e2e/image-ref")

CURRENT_REFS_EXPR = (
    "ps: builtins.concatMap (n: let p = ps.${n}; in "
    'if p ? imageTag then [ "${p.imageName}:${p.imageTag}" ] else [ ]) '
    "(builtins.attrNames ps)"
)


def stamp_dir():
    """The per-user directory of last-used stamps: $DISKGC_E2E_STATE_DIR,
    else $XDG_STATE_HOME/diskgc-e2e, else ~/.local/state/diskgc-e2e."""
    base = os.environ.get("DISKGC_E2E_STATE_DIR")
    if not base:
        xdg = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
        base = str(Path(xdg) / "diskgc-e2e")
    return Path(base) / "image-stamps"


def is_harness_ref(ref):
    repository = ref.rsplit(":", 1)[0]
    return repository == HARNESS_REPOSITORY or repository.startswith(HARNESS_REPOSITORY + "-")


def stamp_path(directory, ref):
    return Path(directory) / quote(ref, safe="")


def touch(directory, ref, now=None):
    """Record `ref` as used at `now` (default: the current time)."""
    if not is_harness_ref(ref):
        raise ValueError(f"not a harness image tag: {ref!r}")
    Path(directory).mkdir(parents=True, exist_ok=True)
    path = stamp_path(directory, ref)
    path.touch()
    if now is not None:
        os.utime(path, (now, now))


def stamp_ages(directory, now):
    """{ref: seconds since its last use} for every stamp in `directory`."""
    directory = Path(directory)
    if not directory.is_dir():
        return {}
    ages = {}
    for path in directory.iterdir():
        ref = unquote(path.name)
        if is_harness_ref(ref):
            ages[ref] = max(0.0, now - path.stat().st_mtime)
    return ages


def plan(images, current, ages=None, grace_secs=0):
    """(ref, verdict, image, age) for every harness tag in `podman images
    --format json` output `images`, given the flake's `current` refs and
    the stamps' `ages` ({ref: seconds since last use}). With no grace, a
    tag not current and not in use is old whatever its stamp says."""
    current = set(current)
    ages = ages or {}
    rows = []
    for image in images:
        for ref in image.get("Names") or []:
            if not is_harness_ref(ref):
                continue
            age = ages.get(ref)
            if ref in current:
                verdict = "current"
            elif image.get("Containers", 0) > 0:
                verdict = "in-use"
            elif grace_secs > 0 and (age is None or age < grace_secs):
                verdict = "recent"
            else:
                verdict = "old"
            rows.append((ref, verdict, image, age))
    return sorted(rows, key=lambda row: row[0])


def describe(rows):
    lines = []
    for ref, verdict, image, age in rows:
        size = image.get("Size", 0) / 2**30
        note = ""
        if verdict == "in-use":
            note = f", {image['Containers']} container(s)"
        if verdict in ("recent", "old"):
            note = ", never stamped" if age is None else f", last used {age / 3600:.1f} h ago"
        lines.append(f"{verdict:<8} {ref}  ({size:.2f} GiB{note})")
    old = [row for row in rows if row[1] == "old"]
    lines.append(
        f"{len(old)} old tag(s) of {len(rows)} harness tag(s); "
        f"at most {sum(row[2].get('Size', 0) for row in old) / 2**30:.2f} GiB "
        "(shared layers stay while a kept image uses them)"
    )
    return "\n".join(lines)


def apply(rows, rmi, forget=lambda ref: None):
    """Remove each old tag with `rmi(ref)` (True on success) and `forget`
    its stamp; the number of failures."""
    failed = 0
    for ref, verdict, _, _ in rows:
        if verdict != "old":
            continue
        if rmi(ref):
            print(f"removed  {ref}")
            forget(ref)
        else:
            failed += 1
    return failed


def run(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, check=False, **kwargs)


def podman_rmi(ref):
    """`podman rmi REF`; a tag another run removed meanwhile counts as removed."""
    removed = run(["podman", "rmi", ref], timeout=300)
    if removed.returncode == 0:
        return True
    if run(["podman", "image", "exists", ref], timeout=60).returncode == 1:
        print(f"gone     {ref} (removed meanwhile by another run)")
        return True
    print(f"failed   {ref}: {removed.stderr.strip()}", file=sys.stderr)
    return False


def current_refs():
    evaluated = run(
        ["nix", "eval", "--json", ".#packages.x86_64-linux", "--apply", CURRENT_REFS_EXPR],
        timeout=600,
    )
    if evaluated.returncode != 0:
        raise RuntimeError(f"nix eval of the current image tags failed: {evaluated.stderr}")
    refs = json.loads(evaluated.stdout)
    if not refs or not all(is_harness_ref(ref) for ref in refs):
        raise RuntimeError(f"unexpected current image tags from the flake: {refs}")
    return refs


def podman_images():
    listed = run(["podman", "images", "--format", "json"], timeout=120)
    if listed.returncode != 0:
        raise RuntimeError(f"podman images failed: {listed.stderr}")
    return json.loads(listed.stdout)


def runner_image_ref(runner_dir):
    """The image tag a runner package records (flake.nix, mkE2eRunner)."""
    path = Path(runner_dir) / RUNNER_IMAGE_REF
    try:
        ref = path.read_text().strip()
    except OSError as error:
        raise RuntimeError(f"no image tag recorded by runner {runner_dir}: {error}") from error
    if not is_harness_ref(ref):
        raise RuntimeError(f"runner {runner_dir} records a non-harness image tag {ref!r}")
    return ref


def refresh_stamps(directory, images, current, now):
    """--apply's stamping: this checkout's current tags are used now; a
    harness tag never stamped is first seen now (its grace starts); the
    stamps of tags no longer in Podman are dropped."""
    present = {ref for image in images for ref in image.get("Names") or [] if is_harness_ref(ref)}
    ages = stamp_ages(directory, now)
    for ref in present:
        if ref in current or ref not in ages:
            touch(directory, ref, now)
    for ref in set(ages) - present:
        stamp_path(directory, ref).unlink(missing_ok=True)


def parse(argv):
    """("stamp", [RUNNER_DIR...]) or ("plan", apply, grace_hours); a
    ValueError on a usage error."""
    if argv[:1] == ["stamp"]:
        if len(argv) < 2:
            raise ValueError("stamp needs at least one RUNNER_DIR")
        return ("stamp", argv[1:])
    apply_, grace_hours, rest = False, float(DEFAULT_GRACE_HOURS), list(argv)
    while rest:
        arg = rest.pop(0)
        if arg == "--apply":
            apply_ = True
        elif arg == "--grace-hours" and rest:
            grace_hours = float(rest.pop(0))
            if not grace_hours >= 0:
                raise ValueError("--grace-hours must be a number >= 0")
        else:
            raise ValueError(f"unexpected argument {arg!r}")
    return ("plan", apply_, grace_hours)


def main(argv):
    try:
        command, *args = parse(argv[1:])
    except ValueError as error:
        print(f"e2e-images: {error}\n{__doc__}", file=sys.stderr)
        return 2
    directory = stamp_dir()
    try:
        if command == "stamp":
            for runner_dir in args[0]:
                ref = runner_image_ref(runner_dir)
                touch(directory, ref)
                print(f"e2e-images: stamped {ref} as used now ({directory})")
            return 0
        apply_, grace_hours = args
        images, current = podman_images(), current_refs()
        now = time.time()
        if apply_:
            refresh_stamps(directory, images, current, now)
        rows = plan(images, current, stamp_ages(directory, now), grace_hours * 3600)
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"e2e-images: {error}", file=sys.stderr)
        return 2
    print(describe(rows))
    if not apply_:
        print(
            f"dry run: nothing removed; `just e2e-images --apply` removes the old tags "
            f"(grace {grace_hours:g} h; stamps in {directory})"
        )
        return 0
    return 1 if apply(rows, podman_rmi, lambda ref: stamp_path(directory, ref).unlink(missing_ok=True)) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
