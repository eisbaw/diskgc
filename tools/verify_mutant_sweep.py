#!/usr/bin/env python3
"""Prove that bounded mutation shards cover one source file exactly once.

Run from the repository root in the Nix development shell. ``plan`` checks
the current cargo-mutants inventory without building and saves the partition
under ignored tmp/. ``check`` compares one selection read from stdin to that
plan and the live unsharded inventory. ``verify`` also checks one completed
report directory for every shard. Source edits require a new plan.
"""

import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

if __package__:
    from .mutation_report import validate_report
    from .mutation_scope import CONTAINER_IMAGE, SOURCES, command as scope_command
else:
    from mutation_report import validate_report
    from mutation_scope import CONTAINER_IMAGE, SOURCES, command as scope_command


MAX_SHARD_MUTANTS = 16
MAX_SHARDS = 64
PLAN_ROOT = Path("tmp/mutants-container-smoke/plans")
SELECTION_DIR = Path("crates/diskgc-core/src/select")
PROCEDURE_FILES = (
    "justfile", "tools/mutants_container_smoke.sh", "tools/copy_mutant_reports.py",
    "tools/bounded_host_log.py", "tools/mutation_report.py", "tools/mutation_scope.py",
    "tools/run_mutants.py", "tools/verify_mutant_sweep.py",
)


def verify_recipe() -> None:
    lines = Path("justfile").read_text().splitlines()
    try:
        start = lines.index("mutants:") + 1
    except ValueError as error:
        raise ValueError("Justfile has no mutants recipe") from error
    recipe = []
    for line in lines[start:]:
        if line.lstrip().startswith("#"):
            continue
        if not line.startswith((" ", "\t")) and line.strip():
            break
        if line.strip():
            recipe.append(line.strip())
    expected = [
        "git check-ignore -q tmp/system || { echo 'tmp/system must be Git-ignored before cargo-mutants copies the source tree' >&2; exit 2; }",
        "python3 tools/mutation_scope.py run",
    ]
    if recipe != expected:
        raise ValueError("mutants recipe differs from the canonical scoped command")


def mutant_names(source_file: Path, shard: str | None = None) -> list[str]:
    command = ["cargo", "mutants", "--list", "--gitignore", "true", "--file", str(source_file)]
    if shard is not None:
        command.extend(["--shard", shard])
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    if result.returncode:
        raise ValueError(
            f"cargo-mutants inventory failed for {source_file} {shard or 'all'} "
            f"(exit {result.returncode}): {result.stderr.strip()}"
        )
    names = result.stdout.splitlines()
    if len(names) != len(set(names)):
        raise ValueError(f"duplicate mutant name in {source_file} {shard or 'all'}")
    return names


def verify_scope() -> None:
    """Check that the named sources equal the Justfile's selection scope."""
    verify_recipe()
    actual_files = set(SELECTION_DIR.glob("*.rs")) - {SELECTION_DIR / "tests.rs"}
    expected_files = {path for name, path in SOURCES.items() if name != "executor"}
    if actual_files != expected_files:
        raise ValueError(
            f"selection source set changed: missing {sorted(expected_files - actual_files)}, "
            f"new {sorted(actual_files - expected_files)}"
        )
    result = subprocess.run(scope_command(list_only=True), text=True, capture_output=True, check=False)
    if result.returncode:
        raise ValueError(f"complete mutation listing failed: {result.stderr.strip()}")
    expected = [name for path in SOURCES.values() for name in mutant_names(path)]
    actual = result.stdout.splitlines()
    if len(expected) != len(set(expected)) or sorted(actual) != sorted(expected):
        raise ValueError("named source inventories differ from complete Justfile selection")
    print(f"complete mutation scope: {len(actual)} mutants across {len(SOURCES)} files")


def verify_partition(full: list[str], shards: list[list[str]]) -> None:
    """Reject omitted, duplicated, extra or oversized shard selections."""
    if not full:
        raise ValueError("source has no mutants; do not schedule empty shards")
    seen: set[str] = set()
    for index, names in enumerate(shards):
        if not names or len(names) > MAX_SHARD_MUTANTS:
            raise ValueError(f"shard {index} has {len(names)} mutants; required range is 1-16")
        if len(names) != len(set(names)):
            raise ValueError(f"shard {index} repeats a mutant within the shard")
        overlap = seen.intersection(names)
        if overlap:
            raise ValueError(f"shard {index} repeats {len(overlap)} mutant(s): {sorted(overlap)[0]}")
        seen.update(names)
    missing = set(full) - seen
    extra = seen - set(full)
    if missing or extra:
        raise ValueError(
            f"shards differ from full inventory: {len(missing)} missing, {len(extra)} extra"
        )


def cargo_mutants_version() -> str:
    result = subprocess.run(
        ["cargo", "mutants", "--version"], text=True, capture_output=True, check=False
    )
    if result.returncode:
        raise ValueError(f"cannot identify cargo-mutants version: {result.stderr.strip()}")
    return result.stdout.strip()


def container_image_id() -> str:
    result = subprocess.run(
        ["podman", "image", "inspect", "--format", "{{.Id}}", CONTAINER_IMAGE],
        text=True, capture_output=True, check=False,
    )
    image_id = result.stdout.strip()
    if result.returncode or not re.fullmatch(r"(?:sha256:)?[0-9a-f]{64}", image_id):
        raise ValueError(f"cannot identify cached mutation image {CONTAINER_IMAGE}: {result.stderr.strip()}")
    return image_id.removeprefix("sha256:")


def workspace_snapshot() -> str:
    """Hash source, tests, fixtures, documentation and build inputs."""
    paths = ("Cargo.toml", "Cargo.lock", "PRD.md", "rust-toolchain.toml", "flake.nix",
             "flake.lock", ".cargo", "build.rs", "justfile", "crates",
             "diskgc-e2e", "docs", "tools", "packaging", "fuzz")
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", *paths],
        capture_output=True, check=False,
    )
    if result.returncode:
        raise ValueError(f"cannot list Cargo workspace files: {result.stderr.decode().strip()}")
    names = sorted(set(name for name in result.stdout.split(b"\0") if name))
    if not names:
        raise ValueError("Cargo workspace snapshot is empty")
    digest = hashlib.sha256()
    for raw_name in names:
        path = Path(raw_name.decode("utf-8", "surrogateescape"))
        if not path.is_file():
            raise ValueError(f"workspace file missing: {path}")
        content = path.read_bytes()
        digest.update(len(raw_name).to_bytes(8, "big"))
        digest.update(raw_name)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def procedure_snapshot() -> str:
    """Bind evidence to the actual runner, report copy and scope procedure."""
    digest = hashlib.sha256()
    for name in PROCEDURE_FILES:
        data = Path(name).read_bytes()
        digest.update(name.encode() + b"\0")
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(data)
    return digest.hexdigest()


def identity_digest(identity: dict) -> str:
    return hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()


def plan_identity(source: str, total: int, pinned_image_id: str | None = None) -> tuple[Path, dict]:
    source_hash = hashlib.sha256(SOURCES[source].read_bytes()).hexdigest()
    version = cargo_mutants_version()
    snapshot = workspace_snapshot()
    image_id = pinned_image_id if pinned_image_id is not None else container_image_id()
    if not re.fullmatch(r"[0-9a-f]{64}", image_id):
        raise ValueError("pinned container image ID must be 64 lowercase hex digits")
    identity = {
        "source": source,
        "total": total,
        "source_sha256": source_hash,
        "cargo_mutants_version": version,
        "container_image_id": image_id,
        "workspace_sha256": snapshot,
        "procedure_sha256": procedure_snapshot(),
    }
    return PLAN_ROOT / f"{source}-{total}-{identity_digest(identity)[:16]}.json", identity


def load_plan(path: Path, identity: dict) -> dict:
    plan = json.loads(path.read_text())
    if any(plan.get(key) != value for key, value in identity.items()):
        raise ValueError(f"stale mutation plan: {path}")
    full, shards = plan["full"], plan["shards"]
    if not isinstance(full, list) or not isinstance(shards, list) or len(shards) != identity["total"]:
        raise ValueError(f"invalid mutation plan shape: {path}")
    if len(full) != len(set(full)):
        raise ValueError(f"duplicate mutant in mutation plan: {path}")
    verify_partition(full, shards)
    if mutant_names(SOURCES[identity["source"]]) != full:
        raise ValueError(f"live mutant inventory differs from saved plan: {path}")
    return plan


def inventory(source: str, total: int, create: bool,
              pinned_image_id: str | None = None) -> list[list[str]]:
    path, identity = plan_identity(source, total, pinned_image_id)
    if path.exists():
        plan = load_plan(path, identity)
    elif create:
        full = mutant_names(SOURCES[source])
        shards = [mutant_names(SOURCES[source], f"{index}/{total}") for index in range(total)]
        verify_partition(full, shards)
        plan = {**identity, "full": full, "shards": shards}
        PLAN_ROOT.mkdir(parents=True, exist_ok=True)
        with path.open("x") as output:
            json.dump(plan, output, indent=2)
            output.write("\n")
    else:
        raise ValueError(f"missing mutation plan {path}; run plan {source} {total} first")
    print(f"{source}: {len(plan['full'])} mutants in {total} complete, disjoint shards; {path}")
    return plan["shards"]


def verify_reports(source: str, shards: list[list[str]], directories: list[Path],
                   expected_identity: str) -> None:
    total = len(shards)
    if len(directories) != total:
        raise ValueError(f"expected {total} report directories; received {len(directories)}")
    pattern = re.compile(rf"^{re.escape(source)}-(\d+)-of-{total}-")
    seen: set[int] = set()
    counts = {"caught": 0, "missed": 0, "timeout": 0, "unviable": 0}
    for directory in directories:
        match = pattern.match(directory.name)
        if match is None:
            raise ValueError(f"report directory has wrong source/total: {directory}")
        index = int(match.group(1))
        if index >= total or index in seen:
            raise ValueError(f"duplicate or out-of-range shard index {index}: {directory}")
        seen.add(index)
        if (directory / "exit-status").read_text().strip() != "0" or (
            directory / "cargo-exit-status"
        ).read_text().strip() != "0":
            raise ValueError(f"shard did not exit successfully: {directory}")
        selected = (directory / "selected.txt").read_text().splitlines()
        if selected != shards[index]:
            raise ValueError(f"shard selection differs from current inventory: {directory}")
        if (directory / "plan.sha256").read_text().strip() != expected_identity:
            raise ValueError(f"shard plan/procedure identity differs from current plan: {directory}")
        reports = directory / "out" / "mutants.out"
        report_counts = validate_report(reports, selected)
        for category in counts:
            counts[category] += report_counts[category]
    if seen != set(range(total)):
        raise ValueError(f"missing shard indexes: {sorted(set(range(total)) - seen)}")
    print(f"validated {sum(map(len, shards))} outcomes: {counts}")


def main() -> int:
    if sys.argv[1:] == ["snapshot"]:
        try:
            print(workspace_snapshot())
        except (OSError, ValueError) as error:
            print(f"mutation snapshot failed: {error}", file=sys.stderr)
            return 1
        return 0
    if sys.argv[1:] == ["scope"]:
        try:
            verify_scope()
        except (OSError, ValueError) as error:
            print(f"mutation sweep verification failed: {error}", file=sys.stderr)
            return 1
        return 0
    if len(sys.argv) < 4 or sys.argv[1] not in ("plan", "check", "verify", "identity"):
        print("usage: verify_mutant_sweep.py scope|snapshot", file=sys.stderr)
        print("usage: verify_mutant_sweep.py plan SOURCE TOTAL", file=sys.stderr)
        print("       verify_mutant_sweep.py check SOURCE TOTAL INDEX IMAGE_ID < selected.txt", file=sys.stderr)
        print("       verify_mutant_sweep.py verify SOURCE TOTAL REPORT_DIR ...", file=sys.stderr)
        print("       verify_mutant_sweep.py identity SOURCE TOTAL IMAGE_ID", file=sys.stderr)
        return 2
    command, source = sys.argv[1:3]
    if source not in SOURCES:
        print(f"unknown source: {source}; choose {', '.join(SOURCES)}", file=sys.stderr)
        return 2
    try:
        total = int(sys.argv[3])
    except ValueError:
        print("TOTAL must be an integer", file=sys.stderr)
        return 2
    if not 1 <= total <= MAX_SHARDS:
        print(f"TOTAL must be 1-{MAX_SHARDS}", file=sys.stderr)
        return 2
    if command == "plan" and len(sys.argv) != 4:
        print(f"{command} takes no report directories", file=sys.stderr)
        return 2
    if command == "identity" and len(sys.argv) != 5:
        print("identity requires the runner's pinned IMAGE_ID", file=sys.stderr)
        return 2
    if command == "check" and len(sys.argv) != 6:
        print("check requires one zero-based shard INDEX and pinned IMAGE_ID", file=sys.stderr)
        return 2
    try:
        if command == "identity":
            verify_recipe()
            path, identity = plan_identity(source, total, sys.argv[4])
            load_plan(path, identity)
            print(identity_digest(identity))
            return 0
        if command == "verify":
            verify_scope()
        shards = inventory(source, total, create=command == "plan",
                           pinned_image_id=sys.argv[5] if command == "check" else None)
        if command == "check":
            index = int(sys.argv[4])
            if index not in range(total) or sys.stdin.read().splitlines() != shards[index]:
                raise ValueError(f"selected mutants differ from inventory shard {index}/{total}")
        if command == "verify":
            expected_identity = identity_digest(plan_identity(source, total)[1])
            verify_reports(source, shards, [Path(value) for value in sys.argv[4:]], expected_identity)
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f"mutation sweep verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
