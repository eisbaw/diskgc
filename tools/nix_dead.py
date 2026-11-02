#!/usr/bin/env python3
"""Print how much a Nix garbage collection would free, deleting nothing
(docs/testing.md, Commands: Cleanup; TASK-311).

Usage: python3 tools/nix_dead.py   (from the checkout; the last line of `just e2e`)

Nix GC is host-wide and stays the machine owner's call; this only
reports. `nix-store --gc --print-dead` lists the dead store paths, and
their NAR sizes are summed with `nix path-info --json` (an upper bound on
the bytes freed: paths hard-linked by store optimisation free less;
entries that are not valid store paths, such as stray `.lock` files, are
counted but have no size).

Listing dead paths is not free of side effects: the daemon's root scan
unlinks every registration in /nix/var/nix/gcroots/auto whose out-link is
gone, and stale temporary-root files of dead processes. Registrations of
this repository's own out-links (its .gcroots/ links in any worktree) are
the harness's artefacts; when any other registration is dangling, the
estimate is skipped, naming them, so this never removes another
program's state. A registration whose target cannot be read is reported
and does not stop the estimate (its target is usually root's, e.g.
/root/.cache/nix/flake-registry.json).
Exit: 0 (also when skipped), 2 when Nix failed."""

import json
import os
import subprocess
import sys
from pathlib import Path

AUTO_ROOTS = Path("/nix/var/nix/gcroots/auto")
BATCH = 2000


def dangling_registrations(auto_dir=AUTO_ROOTS):
    """(dangling out-link targets, unreadable targets) of the auto roots."""
    dangling, unreadable = [], []
    for entry in sorted(os.listdir(auto_dir)):
        target = os.path.join(auto_dir, os.readlink(os.path.join(auto_dir, entry)))
        try:
            os.lstat(target)
        except FileNotFoundError:
            dangling.append(target)
        except PermissionError:
            unreadable.append(target)
    return dangling, unreadable


def foreign(targets, repo_root):
    """The targets outside `repo_root` (this repository and its worktrees)."""
    root = str(repo_root).rstrip("/") + "/"
    return [t for t in targets if not os.path.normpath(t).startswith(root)]


def nar_total(path_infos):
    """(bytes, valid paths, invalid paths) of `nix path-info --json`
    objects ({path: info or null}, or a list of infos)."""
    total = valid = invalid = 0
    for batch in path_infos:
        infos = batch.values() if isinstance(batch, dict) else batch
        for info in infos:
            if info:
                total += info.get("narSize", 0)
                valid += 1
            else:
                invalid += 1
    return total, valid, invalid


def run(argv, **kwargs):
    done = subprocess.run(argv, capture_output=True, text=True, check=False, **kwargs)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(argv[:3])} ... failed ({done.returncode}): {done.stderr.strip()[-2000:]}")
    return done.stdout


def repo_root():
    common = run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], timeout=60).strip()
    return Path(common).parent


def main(argv):
    if argv[1:]:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        dangling, unreadable = dangling_registrations()
        others = foreign(dangling, repo_root())
        for target in unreadable:
            print(f"nix-dead: note: cannot check the gcroots/auto registration of {target}")
        if others:
            for target in others:
                print(f"nix-dead: dangling gcroots/auto registration, not this repository's: {target}")
            print(
                "nix-dead: SKIPPED: listing dead paths would make the Nix daemon unlink the "
                f"{len(others)} registration(s) above; that is the machine owner's call"
            )
            return 0
        dead = run(["nix-store", "--gc", "--print-dead"], timeout=1800).split()
        infos = []
        for start in range(0, len(dead), BATCH):
            out = run(["nix", "path-info", "--json", "--json-format", "1", *dead[start : start + BATCH]], timeout=600)
            infos.append(json.loads(out))
        total, valid, invalid = nar_total(infos)
    except (RuntimeError, OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"nix-dead: {error}", file=sys.stderr)
        return 2
    ours = len(dangling)
    print(
        f"nix-dead: a `nix-store --gc` would free at most {total / 2**30:.2f} GiB "
        f"({valid} dead store paths by NAR size, {invalid} other dead entries); not run, "
        f"the machine owner's call"
        + (f"; the root scan dropped {ours} dangling registration(s) of this repository" if ours else "")
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
