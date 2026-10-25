"""Guest-only measurement of the background tally's pressure brake (TASK-29,
docs/testing.md `background_tally_backs_off_under_io_pressure`).

A container cannot prove this: /proc/pressure there is the host's. In this
disposable guest it is the guest's own. Two private disks, throttled by QEMU:

- /dev/vdb (moderate IOPS limit) holds the fixture the tally sizes; its
  caches are dropped after it is built, so the tally stays busy (each batch
  reads cold metadata) for the whole run;
- /dev/vdc (tight IOPS and bandwidth limit) takes a direct-I/O writer that
  saturates it, which drives the guest's io `some avg10` above 10 %.

The tally runs as the e2e driver's `tally-background` operation inside a
test container (the interlock), reading the fixture through a read-only
bind mount, and prints one line per batch or brake, stamped with
CLOCK_BOOTTIME (what /proc/uptime counts). Phases: unloaded, then the
writer until the pressure has been above 10 % for a while, then recovery.

Asserted: the back-off is logged; batches per minute while the pressure is
high fall at least 4 times below the unloaded rate; within 2 minutes after
the pressure falls below every threshold the tally runs batches again at
least at half its unloaded rate. Measured and printed: the rates, the
pressure samples, the batch times. The unloaded phase must not push the
pressure above 10 % itself (the test would prove nothing): asserted too.
"""

import os
import pwd
import subprocess
import sys
import threading
import time
from pathlib import Path

TOKEN = "diskgc.e2e.tally_pressure=1"
FIXTURE_DEV = "/dev/vdb"
LOAD_DEV = "/dev/vdc"
FIXTURE_MNT = Path("/mnt/fixture")
LOAD_MNT = Path("/mnt/load")
TREE = FIXTURE_MNT / "tree"
INSIDE = "/tmp/fixture"
CONTAINER = "diskgc-tally-pressure"
DIRS = 4000
FILES_PER_DIR = 50
RUN_SECONDS = 720
UNLOADED_SECONDS = 210
PRESSURE_HIGH = 10.0  # percent, io some avg10 (the brake's threshold)
HIGH_HOLD_SECONDS = 150
LOAD_MAX_SECONDS = 330
RECOVERY_SECONDS = 120


def require_private_guest() -> None:
    if TOKEN not in Path("/proc/cmdline").read_text().split():
        raise RuntimeError(f"refusing to format disks outside the disposable guest: {TOKEN} absent")
    if os.geteuid() != 0:
        raise RuntimeError("guest root required")
    for dev in (FIXTURE_DEV, LOAD_DEV):
        if subprocess.run(["findmnt", "-n", "-S", dev], capture_output=True).returncode == 0:
            raise RuntimeError(f"{dev} is already mounted")


def uptime() -> float:
    return float(Path("/proc/uptime").read_text().split()[0])


def io_some_avg10() -> float:
    line = Path("/proc/pressure/io").read_text().splitlines()[0]
    assert line.startswith("some "), line
    return float(line.split()[1].split("=")[1])


def prepare_disks(uid: int, gid: int) -> None:
    for dev, mnt in ((FIXTURE_DEV, FIXTURE_MNT), (LOAD_DEV, LOAD_MNT)):
        subprocess.check_call(["mkfs.ext4", "-q", "-F", dev])
        mnt.mkdir(parents=True, exist_ok=True)
        subprocess.check_call(["mount", dev, str(mnt)])
    started = time.monotonic()
    TREE.mkdir()
    for d in range(DIRS):
        sub = TREE / f"d{d:05}"
        sub.mkdir()
        for f in range(FILES_PER_DIR):
            (sub / f"f{f:03}").write_bytes(b"x" * 100)
    subprocess.check_call(["chown", "-R", f"{uid}:{gid}", str(TREE)])
    os.sync()
    Path("/proc/sys/vm/drop_caches").write_text("3\n")
    print(f"fixture dirs={DIRS} files={DIRS * FILES_PER_DIR} built_s={time.monotonic() - started:.0f}", flush=True)


def start_container(image: str, under_test: str) -> None:
    subprocess.check_call([
        "podman", "run", "--detach", "--rm", "--pull=never", "--network=none",
        "--name", CONTAINER,
        "--volume", "/nix/store:/nix/store:ro",
        "--volume", f"{under_test}/bin:/opt/diskgc-e2e/bin:ro",
        "--volume", f"{TREE}:{INSIDE}:ro",
        "--entrypoint", "/bin/sh", image, "-c", f"exec sleep {RUN_SECONDS + 300}",
    ])
    subprocess.check_call(["podman", "exec", CONTAINER, "/opt/diskgc-e2e/bin/diskgc-e2e-canary"])


class Sampler(threading.Thread):
    """io some avg10 every second, with its uptime."""

    def __init__(self) -> None:
        super().__init__(daemon=True)
        self.samples: list[tuple[float, float]] = []
        self.stop = threading.Event()

    def run(self) -> None:
        while not self.stop.is_set():
            self.samples.append((uptime(), io_some_avg10()))
            self.stop.wait(1.0)


def parse(lines: list[str]) -> tuple[list[float], list[float], list[str]]:
    batches, brakes, other = [], [], []
    for line in lines:
        fields = line.split()
        if len(fields) < 3 or not fields[1].isdigit():
            other.append(line)
            continue
        at = int(fields[1]) / 1000
        if fields[2] == "batch":
            batches.append(at)
        elif fields[2] == "braked":
            brakes.append(at)
        else:
            other.append(line)
    return batches, brakes, other


def rate(times: list[float], start: float, end: float) -> float:
    assert end > start, (start, end)
    return sum(1 for t in times if start <= t < end) * 60 / (end - start)


def main() -> None:
    assert len(sys.argv) == 3, "expected image and under-test directory"
    require_private_guest()
    account = pwd.getpwnam("diskgc_tally")
    prepare_disks(account.pw_uid, account.pw_gid)
    writer = None
    sampler = Sampler()
    try:
        start_container(sys.argv[1], sys.argv[2])
        sampler.start()
        out_path = Path("/root/tally.out")
        with open(out_path, "w") as out:
            driver = subprocess.Popen(
                ["podman", "exec", "--user", "1000", CONTAINER,
                 "/opt/diskgc-e2e/bin/diskgc-e2e-driver", "tally-background", str(RUN_SECONDS), INSIDE],
                stdout=out, stderr=subprocess.STDOUT,
            )
            t0 = uptime()
            time.sleep(UNLOADED_SECONDS)
            load_start = uptime()
            writer = subprocess.Popen(
                ["sh", "-c", f"while :; do dd if=/dev/zero of={LOAD_MNT}/fill bs=1M count=256 oflag=direct status=none; done"]
            )
            high_since = None
            while uptime() - load_start < LOAD_MAX_SECONDS:
                time.sleep(1)
                if io_some_avg10() > PRESSURE_HIGH:
                    high_since = high_since or uptime()
                    if uptime() - high_since >= HIGH_HOLD_SECONDS:
                        break
                else:
                    high_since = None
            writer.terminate()
            writer.wait()
            writer = None
            load_end = uptime()
            assert high_since is not None, "the writer never pushed io some avg10 above 10 %"
            driver.wait(timeout=RUN_SECONDS + 120)
        sampler.stop.set()
        sampler.join()
        lines = out_path.read_text().splitlines()
        batches, brakes, other = parse(lines)
        for line in other:
            print("driver:", line, flush=True)
        # Unloaded: the minute and more before the load, after the first
        # listing settled; its own pressure must stay below the threshold.
        unloaded = (t0 + 60, load_start)
        unloaded_pressure = max(p for t, p in sampler.samples if unloaded[0] <= t < unloaded[1])
        r0 = rate(batches, *unloaded)
        # High: from 20 s after the pressure first stayed above 10 % to the
        # writer's end.
        high = (high_since + 20, load_end)
        r1 = rate(batches, *high)
        fall = next(t for t, p in sampler.samples if t >= load_end and p <= PRESSURE_HIGH)
        recovered = (fall + 60, fall + RECOVERY_SECONDS)
        r2 = rate(batches, *recovered)
        first_after = next((t for t in batches if t >= fall), None)
        print(
            f"unloaded batches/min={r0:.1f} (max io avg10 {unloaded_pressure:.2f}%) "
            f"high batches/min={r1:.1f} over {high[1] - high[0]:.0f}s brakes={len(brakes)} "
            f"recovery batches/min={r2:.1f} first batch {None if first_after is None else round(first_after - fall)}s after the fall",
            flush=True,
        )
        print("pressure samples (uptime, io some avg10):", [(round(t), p) for t, p in sampler.samples[::10]], flush=True)
        assert unloaded_pressure <= PRESSURE_HIGH, f"the unloaded tally pressed the disk itself: {unloaded_pressure}"
        assert r0 >= 8, f"the tally was not busy unloaded ({r0:.1f} batches/min): the measurement proves nothing"
        assert brakes, "no back-off logged"
        assert r1 * 4 <= r0, f"batches/min under pressure {r1:.1f} not 4x below unloaded {r0:.1f}"
        assert first_after is not None and first_after - fall <= RECOVERY_SECONDS, "no batch within 2 min of the fall"
        assert r2 * 2 >= r0, f"not recovered within 2 min: {r2:.1f} vs unloaded {r0:.1f}"
    finally:
        sampler.stop.set()
        if writer is not None:
            writer.kill()
        subprocess.run(["podman", "rm", "-f", CONTAINER], capture_output=True)


if __name__ == "__main__":
    main()
