#!/usr/bin/env python3
"""The five-source mutation scope shared by the bounded sweep and verifier."""

from pathlib import Path
import subprocess
import sys


SOURCES = {
    "parse": Path("crates/diskgc-core/src/select/parse.rs"),
    "check": Path("crates/diskgc-core/src/select/check.rs"),
    "evaluate": Path("crates/diskgc-core/src/select/evaluate.rs"),
    "mod": Path("crates/diskgc-core/src/select/mod.rs"),
    "executor": Path("crates/diskgc-core/src/executor.rs"),
}
CONTAINER_IMAGE = "docker.io/library/debian:trixie-slim"


def command(*, list_only: bool = True) -> list[str]:
    if not list_only:
        raise ValueError("host mutation builds are unsupported; use the bounded container sweep")
    result = ["cargo", "mutants", "--list"]
    result.extend(["--gitignore", "true"])
    for source in SOURCES.values():
        result.extend(["--file", str(source)])
    return result


if __name__ == "__main__":
    if sys.argv[1:] == ["image"]:
        print(CONTAINER_IMAGE)
        sys.exit(0)
    if sys.argv[1:] != ["run"]:
        print("usage: mutation_scope.py run|image", file=sys.stderr)
        sys.exit(2)
    sys.exit(subprocess.run([sys.executable, str(Path(__file__).with_name("run_mutants.py"))],
                            check=False).returncode)
