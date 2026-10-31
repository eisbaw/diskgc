#!/usr/bin/env python3
"""`just release-verify`: a release version is only ever built from its tag.

docs/architecture.md, Release procedure: a plain version (no pre-release
suffix) is a release, and its one commit is tagged `v$VERSION`. This fails
(exit 1) when Cargo.toml's version is plain and HEAD is not the commit of
tag v$VERSION, or the tree is dirty; it passes a development version
(`0.2.0-dev`), which names its commit instead. Read-only: it runs git and
reads Cargo.toml. Exit 2: the facts could not be read.
"""

import subprocess
import sys
import tomllib
from pathlib import Path


def verdict(version, head, tag_commit, dirty):
    """(ok, message) for a workspace version, the HEAD commit, the commit
    tag v<version> points at (None: no such tag) and whether the tree has
    uncommitted changes."""
    if "-" in version:
        return True, f"{version} is a development version: builds name their commit; nothing to verify"
    tag = f"v{version}"
    if tag_commit is None:
        return False, f"{version} is a release version, but there is no tag {tag}"
    if tag_commit != head:
        return False, f"{version} is a release version, but HEAD {head[:12]} is not {tag} ({tag_commit[:12]})"
    if dirty:
        return False, f"{version} is a release version at {tag}, but the tree has uncommitted changes"
    return True, f"{version} is a release version built from {tag} ({head[:12]})"


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True,
                          capture_output=True, text=True).stdout.strip()


def main():
    repo = Path(__file__).resolve().parent.parent
    try:
        version = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        head = git(repo, "rev-parse", "HEAD")
        tag = subprocess.run(["git", "-C", str(repo), "rev-parse", "-q", "--verify",
                              f"refs/tags/v{version}^{{commit}}"],
                             capture_output=True, text=True)
        tag_commit = tag.stdout.strip() if tag.returncode == 0 else None
        dirty = git(repo, "status", "--porcelain", "--untracked-files=no") != ""
    except (OSError, KeyError, subprocess.CalledProcessError, tomllib.TOMLDecodeError) as e:
        print(f"release-verify: cannot read the facts: {e}", file=sys.stderr)
        return 2
    ok, message = verdict(version, head, tag_commit, dirty)
    print(f"release-verify: {message}", file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
