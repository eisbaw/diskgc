"""Guest-only 40k-candidate ext4 dry-plan measurement for TASK-70.

The kernel-command-line token prevents accidentally running the cache drop on
a developer host. The NixOS VM test supplies this script and the product
binary through the read-only Nix store; the fixture lives on its private disk.
"""

import json
import os
import pwd
import re
import subprocess
import sys
import tempfile
from pathlib import Path


TOKEN = "diskgc.e2e.cold_scale=1"
ROOT = Path("/var/lib/diskgc-cold-scale")
CONFIG = Path("/var/lib/diskgc-cold-scale.yaml")
INSIDE_ROOT = Path("/tmp/cold-scale")
INSIDE_CONFIG = Path("/home/tester/cold-scale.yaml")
CONTAINER = "diskgc-cold-scale"
COUNT = 40_000
PROFILE = "cold-scale-small-dirs"
OUTER_LIMIT_SECONDS = 630


def require_private_guest() -> None:
    cmdline = Path("/proc/cmdline").read_text().split()
    if TOKEN not in cmdline:
        raise RuntimeError(f"refusing cache drop outside the disposable guest: {TOKEN} absent")
    if os.geteuid() != 0:
        raise RuntimeError("guest-only cache drop requires guest root")
    if ROOT.is_symlink():
        raise RuntimeError(f"fixture root must not be a symlink: {ROOT}")
    ROOT.mkdir(mode=0o700, exist_ok=True)
    mount = subprocess.check_output(
        ["findmnt", "-n", "-o", "TARGET,FSTYPE,SOURCE", "-T", str(ROOT)], text=True
    ).split()
    if len(mount) != 3 or mount[:2] != ["/", "ext4"]:
        raise RuntimeError(f"fixture must be on guest ext4 root: {mount}")
    if mount[2].startswith("/nix/store"):
        raise RuntimeError(f"fixture must not be on Nix store: {mount}")
    print(f"fixture_mount={','.join(mount)} guest_kernel_token=present", flush=True)


def candidate(number: int, root: Path = ROOT) -> Path:
    return root / f"cold-scale-{number:05}"


def build_fixture(uid: int, gid: int) -> None:
    os.chown(ROOT, uid, gid)
    CONFIG.write_text(
        "version: 1\n"
        "profiles:\n"
        f"  - name: {PROFILE}\n"
        f"    find: {{roots: [{INSIDE_ROOT}], paths: ['cold-scale-*'], type: dir}}\n"
        "    action: delete\n"
        f"    examples: {{match: [{candidate(0, INSIDE_ROOT)}]}}\n"
    )
    CONFIG.chmod(0o644)
    os.setegid(gid)
    os.seteuid(uid)
    try:
        for number in range(COUNT):
            path = candidate(number)
            path.mkdir(mode=0o700)
            (path / "payload").write_bytes(b"x")
    finally:
        os.seteuid(0)
        os.setegid(0)
    assert sum(1 for _ in ROOT.iterdir()) == COUNT


def dentry_count() -> int:
    return int(Path("/proc/sys/fs/dentry-state").read_text().split()[0])


def cached_kib() -> int:
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("Cached:"):
            return int(line.split()[1])
    raise AssertionError("/proc/meminfo has no Cached field")


def cold_guest_caches() -> None:
    require_private_guest()
    # The completed builder holds no fixture descriptors. Read every inode
    # before the drop so the before/after dentry delta has a known fixture.
    for number in range(COUNT):
        path = candidate(number)
        os.stat(path)
        os.stat(path / "payload")
    os.sync()
    before_dentries = dentry_count()
    before_cached = cached_kib()
    require_private_guest()
    with open("/proc/sys/vm/drop_caches", "w", encoding="ascii") as drop:
        drop.write("3\n")
    after_dentries = dentry_count()
    after_cached = cached_kib()
    print(
        "guest_cache_drop "
        f"dentries_before={before_dentries} dentries_after={after_dentries} "
        f"cached_kib_before={before_cached} cached_kib_after={after_cached}",
        flush=True,
    )
    if before_dentries - after_dentries < COUNT:
        raise RuntimeError("guest dentry cache did not drop by the required count")
    if before_cached - after_cached < 32 * 1024:
        raise RuntimeError("guest page cache was not materially dropped")


def start_container(image: str, under_test: str) -> None:
    def checked_podman(args: list[str]) -> None:
        result = subprocess.run(args, capture_output=True, text=True)
        if result.returncode != 0:
            raise RuntimeError(
                f"Podman command failed ({result.returncode}): {args!r}\n"
                f"stdout: {result.stdout}\nstderr: {result.stderr}"
            )

    checked_podman(
        [
            "podman", "run", "--detach", "--rm", "--pull=never", "--network=none",
            "--userns=host", "--cap-add=SYS_ADMIN", "--cap-add=SYS_PTRACE",
            "--name", CONTAINER,
            "--volume", "/nix/store:/nix/store:ro",
            "--volume", f"{under_test}/bin:/opt/diskgc-e2e/bin:ro",
            "--volume", f"{ROOT}:{INSIDE_ROOT}:ro",
            "--volume", f"{CONFIG}:{INSIDE_CONFIG}:ro",
            "--entrypoint", "/bin/sh", image,
            "-c", "exec sleep 2000",
        ],
    )
    checked_podman(
        ["podman", "exec", CONTAINER, "/opt/diskgc-e2e/bin/diskgc-e2e-canary"],
    )
    mountinfo = subprocess.check_output(
        ["podman", "exec", CONTAINER, "cat", "/proc/self/mountinfo"], text=True
    )
    matches = []
    for line in mountinfo.splitlines():
        left, right = line.split(" - ", 1)
        if left.split()[4] == str(INSIDE_ROOT):
            matches.append((right.split()[0], left.split()[5].split(",")))
    if len(matches) != 1 or matches[0][0] != "ext4" or "ro" not in matches[0][1]:
        raise RuntimeError(f"container fixture mount must be read-only ext4: {matches}")
    tester = subprocess.check_output(
        ["podman", "exec", "--user", "tester", CONTAINER, "/bin/sh", "-c",
         f"test -r {candidate(0, INSIDE_ROOT)}/payload && id -u"],
        text=True,
    ).strip()
    assert tester == "1000", f"container tester cannot read ext4 fixture: {tester!r}"
    print("container_ext4_mount=verified interlock_canary=passed fixture_bind=read_only", flush=True)


def run_plan(time_binary: str, label: str) -> None:
    args = [
        "podman", "exec", "--user", "tester", CONTAINER,
        time_binary, "-f", "DISKGC_SCALE_METRIC elapsed_seconds=%e max_rss_kib=%M exit=%x",
        "/opt/diskgc-e2e/bin/diskgc",
        "ensure",
        "--dry-run",
        "--json",
        "--config",
        str(INSIDE_CONFIG),
        "--min-free",
        "100%",
        "--select",
        f"profile:{PROFILE}",
        str(INSIDE_ROOT),
    ]
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(
            args,
            cwd="/",
            stdout=stdout,
            stderr=stderr,
        )
        try:
            process.wait(timeout=OUTER_LIMIT_SECONDS)
        except subprocess.TimeoutExpired as error:
            process.kill()
            process.wait()
            raise AssertionError(f"{label} exceeded the 10m product deadline plus 30s") from error

        stderr.seek(0)
        error_text = stderr.read().decode("utf-8", errors="replace")
        stdout.seek(0)
        output = stdout.read()
        metric_line = next(
            (line for line in error_text.splitlines() if line.startswith("DISKGC_SCALE_METRIC ")),
            None,
        )
        assert metric_line is not None, f"{label}: GNU time metric missing: {error_text[:4096]!r}"
        metric = re.fullmatch(
            r"DISKGC_SCALE_METRIC elapsed_seconds=([0-9.]+) max_rss_kib=(\d+) exit=(\d+)",
            metric_line,
        )
        assert metric is not None, f"{label}: invalid GNU time metric: {metric_line!r}"
        elapsed_ms = int(float(metric.group(1)) * 1000)
        peak_kib = int(metric.group(2))
        product_exit = int(metric.group(3))
        assert process.returncode == 0, (
            f"{label}: exit={process.returncode} elapsed_ms={elapsed_ms} "
            f"stderr={error_text[:4096]!r} stdout_prefix={output[:512]!r}"
        )
        assert product_exit == 0, f"{label}: product exit {product_exit}"
        assert elapsed_ms < 600_000, f"{label}: exceeded the default 10m deadline"
        report = json.loads(output)
        assert report["status"] == "dry_run", (label, report["status"], error_text[:4096])
        assert report["errors"] == [], (label, report["errors"])
        plan = report["plan"]
        assert len(plan) == COUNT, (label, len(plan), COUNT)
        expected = {str(candidate(number, INSIDE_ROOT)) for number in range(COUNT)}
        actual = {entry["candidate"] for entry in plan}
        assert actual == expected, (label, len(actual), len(expected))
        assert all(entry["reason"] is None for entry in plan), label
        assert peak_kib > 0, f"{label}: no product max RSS sample"
        print(
            f"{label} elapsed_ms={elapsed_ms} exit={product_exit} "
            f"status={report['status']} candidates={len(plan)} "
            f"peak_product_maxrss_kib={peak_kib}",
            flush=True,
        )


def main() -> None:
    assert len(sys.argv) == 4, "expected image, under-test directory, and GNU time binary"
    require_private_guest()
    account = pwd.getpwnam("diskgc_scale")
    build_fixture(account.pw_uid, account.pw_gid)
    try:
        start_container(sys.argv[1], sys.argv[2])
        cold_guest_caches()
        run_plan(sys.argv[3], "cold_ext4_plan")

        # The first plan itself warmed the tree. Read payloads as an explicit
        # warm control before repeating the same real-binary dry plan.
        for number in range(COUNT):
            with open(candidate(number) / "payload", "rb") as payload:
                assert payload.read() == b"x"
        run_plan(sys.argv[3], "warm_ext4_plan")
        assert sum(1 for _ in ROOT.iterdir()) == COUNT, "dry plan changed fixture count"
        for number in range(COUNT):
            assert (candidate(number) / "payload").read_bytes() == b"x", number
    finally:
        subprocess.run(["podman", "rm", "-f", CONTAINER], capture_output=True)


if __name__ == "__main__":
    main()
