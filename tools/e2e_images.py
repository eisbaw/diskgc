#!/usr/bin/env python3
"""List, and with --apply remove, the e2e harness's old Podman image tags
(docs/testing.md, Commands; TASK-60).

Usage: python3 tools/e2e_images.py [--apply]   (from the repository root)

The runners load each test image under `localhost/diskgc-e2e-NAME:TAG`, the
tag being the image's Nix hash, and never remove one; every input change
leaves the previous multi-gigabyte image behind. Which tags are current is
the flake's answer: every package with an `imageTag` (`nix eval`). Only
tags in the harness's own namespace (`localhost/diskgc-e2e` and
`localhost/diskgc-e2e-*`) are ever considered; every other image is left
alone and not listed.

Each harness tag is one of:
  current  the flake at this checkout uses it: kept;
  in-use   a container (running, stopped, or one `e2e-one` kept) uses it:
           kept; `podman rmi` without --force would refuse anyway;
  old      removed by --apply (`podman rmi TAG`, never --force).

Without --apply nothing is removed: the plan is printed. Layers shared with
a kept image are not freed. Another checkout (worktree) on other inputs has
other current tags: removing one costs that checkout a reload, and an e2e
run there that is starting a container from it at that moment fails; run
--apply when no other e2e run is starting.
Exit: 0 (with --apply, when every removal succeeded), 1 when a removal
failed, 2 on a usage or environment error."""

import json
import subprocess
import sys

HARNESS_REPOSITORY = "localhost/diskgc-e2e"

CURRENT_REFS_EXPR = (
    "ps: builtins.concatMap (n: let p = ps.${n}; in "
    'if p ? imageTag then [ "${p.imageName}:${p.imageTag}" ] else [ ]) '
    "(builtins.attrNames ps)"
)


def is_harness_ref(ref):
    repository = ref.rsplit(":", 1)[0]
    return repository == HARNESS_REPOSITORY or repository.startswith(HARNESS_REPOSITORY + "-")


def plan(images, current):
    """(ref, verdict, image) for every harness tag in `podman images
    --format json` output `images`, given the flake's `current` refs."""
    current = set(current)
    rows = []
    for image in images:
        for ref in image.get("Names") or []:
            if not is_harness_ref(ref):
                continue
            if ref in current:
                verdict = "current"
            elif image.get("Containers", 0) > 0:
                verdict = "in-use"
            else:
                verdict = "old"
            rows.append((ref, verdict, image))
    return sorted(rows, key=lambda row: row[0])


def describe(rows):
    lines = []
    for ref, verdict, image in rows:
        size = image.get("Size", 0) / 2**30
        note = f", {image['Containers']} container(s)" if verdict == "in-use" else ""
        lines.append(f"{verdict:<8} {ref}  ({size:.2f} GiB{note})")
    old = [row for row in rows if row[1] == "old"]
    lines.append(
        f"{len(old)} old tag(s) of {len(rows)} harness tag(s); "
        f"at most {sum(row[2].get('Size', 0) for row in old) / 2**30:.2f} GiB "
        "(shared layers stay while a kept image uses them)"
    )
    return "\n".join(lines)


def apply(rows, rmi):
    """Remove each old tag with `rmi(ref)` (True on success); the number
    of failures."""
    failed = 0
    for ref, verdict, _ in rows:
        if verdict != "old":
            continue
        if rmi(ref):
            print(f"removed  {ref}")
        else:
            failed += 1
    return failed


def run(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, check=False, **kwargs)


def podman_rmi(ref):
    removed = run(["podman", "rmi", ref], timeout=300)
    if removed.returncode != 0:
        print(f"failed   {ref}: {removed.stderr.strip()}", file=sys.stderr)
    return removed.returncode == 0


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


def main(argv):
    if argv[1:] not in ([], ["--apply"]):
        print(__doc__, file=sys.stderr)
        return 2
    try:
        rows = plan(podman_images(), current_refs())
    except (RuntimeError, OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"e2e-images: {error}", file=sys.stderr)
        return 2
    print(describe(rows))
    if argv[1:] != ["--apply"]:
        print("dry run: nothing removed; `just e2e-images --apply` removes the old tags")
        return 0
    return 1 if apply(rows, podman_rmi) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
