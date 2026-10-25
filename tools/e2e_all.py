#!/usr/bin/env python3
"""Run every stage of a justfile recipe without stopping at the first
failure, then print and save a per-stage exit table (TASK-199).

`just e2e` stops at its first failing line, so one red stage hides every
later one: async QA then knows nothing about release, Docker or the VMs.
The stages are read from `just --dump --dump-format json` rather than
listed here, so this cannot drift from the recipe it runs: each dependency
is one stage (`just <dep>`), each body line is one stage, run as just runs
it (`sh -cu`, from the justfile's directory). Recipes with parameters,
arguments to dependencies, interpolation or a shebang are refused, since
running their lines one by one would not mean what just means.

Usage: python3 tools/e2e_all.py RECIPE   (run from the justfile directory)
Exit: 0 when every stage passed, 1 when any failed, 2 on a usage or recipe
error, 130 when interrupted (the table then marks unrun stages)."""

import json
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

NOT_RUN = "-"


def stages(dump, recipe):
    """The commands that make up `recipe`, in just's order."""
    recipes = dump["recipes"]
    if recipe not in recipes:
        raise ValueError(f"no recipe {recipe!r} in the justfile")
    r = recipes[recipe]
    if r["parameters"] or r["shebang"]:
        raise ValueError(f"recipe {recipe!r} has parameters or a shebang")
    commands = []
    for dep in r["dependencies"]:
        if dep["arguments"]:
            raise ValueError(f"dependency {dep['recipe']!r} of {recipe!r} takes arguments")
        commands.append(f"just {dep['recipe']}")
    for line in r["body"]:
        if len(line) != 1 or not isinstance(line[0], str):
            raise ValueError(f"recipe {recipe!r} has an interpolated line: {line!r}")
        text = line[0]
        if text.startswith(("@", "-")) or text.endswith("\\"):
            raise ValueError(f"recipe {recipe!r} has a line just treats specially: {text!r}")
        commands.append(text)
    if not commands:
        raise ValueError(f"recipe {recipe!r} has no stages")
    return commands


def run(commands, cwd, out=sys.stdout):
    """Run each command; a list of (command, exit or NOT_RUN, seconds).
    Stops early only on Ctrl-C, so the table says which stages never ran."""
    results = []
    try:
        for i, command in enumerate(commands, 1):
            print(f"e2e-all: stage {i}/{len(commands)}: {command}", file=out, flush=True)
            started = time.monotonic()
            code = subprocess.run(["sh", "-cu", command], cwd=cwd, check=False).returncode
            results.append((command, code, time.monotonic() - started))
            if code in (-2, 130):
                raise KeyboardInterrupt
    except KeyboardInterrupt:
        results += [(c, NOT_RUN, 0.0) for c in commands[len(results) :]]
    return results


def table(results):
    """One tab-separated row per stage: verdict, exit, seconds, command."""
    rows = ["verdict\texit\tseconds\tcommand"]
    for command, code, seconds in results:
        verdict = "NOT-RUN" if code == NOT_RUN else "PASS" if code == 0 else "FAIL"
        rows.append(f"{verdict}\t{code}\t{seconds:.0f}\t{command}")
    failed = sum(1 for _, code, _ in results if code not in (0, NOT_RUN))
    not_run = sum(1 for _, code, _ in results if code == NOT_RUN)
    rows.append(f"# {len(results)} stages, {failed} failed, {not_run} not run")
    return "\n".join(rows) + "\n"


def exit_code(results):
    if any(code == NOT_RUN for _, code, _ in results):
        return 130
    return 0 if all(code == 0 for _, code, _ in results) else 1


def revision(cwd):
    """The commit the stages ran on, `+dirty` when the tree had changes."""
    sha = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], cwd=cwd, capture_output=True, text=True, check=False
    ).stdout.strip()
    if not sha:
        return "nogit"
    dirty = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=cwd, check=False).returncode != 0
    return sha + ("+dirty" if dirty else "")


def main(argv):
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    cwd = Path.cwd()
    dump = subprocess.run(
        ["just", "--dump", "--dump-format", "json"], cwd=cwd, capture_output=True, text=True, check=True
    ).stdout
    try:
        commands = stages(json.loads(dump), argv[1])
    except ValueError as e:
        print(f"e2e-all: {e}", file=sys.stderr)
        return 2
    rev = revision(cwd)
    started = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    results = run(commands, cwd)
    text = f"# just {argv[1]}, every stage, on {rev}, started {started}\n" + table(results)
    out_dir = cwd / "tmp" / "e2e-all"
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"{argv[1]}-{rev}-{started}.tsv"
    path.write_text(text)
    print(text + f"e2e-all: table saved to {path}", flush=True)
    return exit_code(results)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
