#!/usr/bin/env python3
"""Run and verify the complete mutation scope in bounded Podman shards."""

from pathlib import Path
import re
import subprocess
import sys

if __package__:
    from .mutation_scope import SOURCES
    from .verify_mutant_sweep import (
        MAX_SHARD_MUTANTS, MAX_SHARDS, mutant_names, verify_partition, verify_scope,
    )
else:
    from mutation_scope import SOURCES
    from verify_mutant_sweep import (
        MAX_SHARD_MUTANTS, MAX_SHARDS, mutant_names, verify_partition, verify_scope,
    )


def shard_count(source: str) -> int:
    full = mutant_names(SOURCES[source])
    if not full:
        raise ValueError(f"{source} has no mutants")
    first = (len(full) + MAX_SHARD_MUTANTS - 1) // MAX_SHARD_MUTANTS
    for total in range(first, MAX_SHARDS + 1):
        shards = [mutant_names(SOURCES[source], f"{index}/{total}")
                  for index in range(total)]
        try:
            verify_partition(full, shards)
        except ValueError:
            continue
        return total
    raise ValueError(f"{source} cannot be partitioned into nonempty shards of at most "
                     f"{MAX_SHARD_MUTANTS} mutants (maximum {MAX_SHARDS} shards)")


def checked_command(args: list[str]) -> str:
    print("+", " ".join(args), flush=True)
    result = subprocess.run(args, text=True, capture_output=True, check=False)
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n", flush=True)
    if result.stderr:
        print(result.stderr, end="" if result.stderr.endswith("\n") else "\n",
              file=sys.stderr, flush=True)
    if result.returncode:
        raise ValueError(f"command exited {result.returncode}: {' '.join(args)}")
    return result.stdout


def run_source(source: str) -> None:
    total = shard_count(source)
    checker = [sys.executable, "tools/verify_mutant_sweep.py"]
    checked_command([*checker, "plan", source, str(total)])
    reports: list[str] = []
    for index in range(total):
        output = checked_command(["bash", "tools/mutants_container_smoke.sh",
                                  "--run-shard", source, f"{index}/{total}"])
        matches = re.findall(r"^Mutation exit=0; logs=(.+)$", output, re.MULTILINE)
        if len(matches) != 1:
            raise ValueError(f"{source} shard {index}/{total} has no unique successful report path")
        report = Path(matches[0])
        expected = Path("tmp/mutants-container-smoke").resolve()
        if report.parent != expected or not report.name.startswith(f"{source}-{index}-of-{total}-"):
            raise ValueError(f"{source} shard {index}/{total} reported an unexpected path: {report}")
        reports.append(str(report))
    checked_command([*checker, "verify", source, str(total), *reports])


def main() -> int:
    try:
        verify_scope()
        for source in SOURCES:
            run_source(source)
    except (OSError, ValueError) as error:
        print(f"bounded mutation sweep failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
