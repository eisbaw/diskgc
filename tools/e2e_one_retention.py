#!/usr/bin/env python3
"""Check, through the real runner and rootless Podman, that `e2e-one` keeps
a failed test's container for inspection and removes a passing test's
(docs/testing.md, Commands; TASK-60).

Usage: python3 tools/e2e_one_retention.py RUNNER
RUNNER is an e2e runner wrapper, e.g. .gcroots/e2e-runner/bin/diskgc-e2e-runner.

1. `RUNNER one PROBE`: PROBE (suite.rs, RUNNER_FAILURE_PROBE) fails on
   purpose. The runner must exit 1 and name the container it kept; that
   container must exist, have exited, and its log must hold PROBE's message.
2. `RUNNER one PASSING`: the runner must exit 0 and leave no container.

Only the containers these two runs created are removed (the kept one at the
end, whatever happened). No image is touched. Logs go to
tmp/e2e-logs-retention/. Exit 0 when both checks pass, 1 when one fails,
2 on a usage error."""

import re
import subprocess
import sys
from pathlib import Path

# The Rust constants these mirror are checked by tools/test_e2e_one_retention.py.
PROBE = "runner_failure_probe_fails_on_purpose"
PROBE_MESSAGE = "runner failure probe: failing on purpose"
PASSING = "sandbox_trivial"
LOG_DIR = "tmp/e2e-logs-retention"
# Covers the runner's own setup limit for `one` (ONE_SETUP_LIMIT, 15 min)
# plus the test; the base image is normally loaded already.
RUN_TIMEOUT_SECS = 20 * 60
PODMAN_TIMEOUT_SECS = 60

KEPT = re.compile(r"^container (\S+) kept for inspection:", re.MULTILINE)


def container_name(runner_pid, test):
    """runner.rs container_name: `diskgc-e2e-PID-` and the test name with
    every character but ASCII alphanumerics and `.` turned into `-`."""
    safe = "".join(c if (c.isascii() and c.isalnum()) or c == "." else "-" for c in test)
    return f"diskgc-e2e-{runner_pid}-{safe}"


def kept_container(stderr):
    """The container the runner says it kept, or None."""
    found = KEPT.findall(stderr)
    return found[-1] if found else None


def podman(*args):
    return subprocess.run(
        ["podman", *args],
        capture_output=True,
        text=True,
        timeout=PODMAN_TIMEOUT_SECS,
        check=False,
    )


def run_one(runner, test):
    """Run `RUNNER one TEST`; (exit code, stderr, runner pid). The wrapper
    execs the runner, so the pid is the runner's, which names containers."""
    child = subprocess.Popen(
        [str(runner), "--log-dir", LOG_DIR, "one", test],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        _, stderr = child.communicate(timeout=RUN_TIMEOUT_SECS)
    except subprocess.TimeoutExpired:
        child.kill()
        _, stderr = child.communicate()
        raise AssertionError(f"`one {test}` ran over {RUN_TIMEOUT_SECS}s:\n{stderr}")
    return child.returncode, stderr, child.pid


def check_failed_is_kept(runner, created):
    """Step 1. Appends the container's name to `created` before checking."""
    code, stderr, pid = run_one(runner, PROBE)
    kept = container_name(pid, PROBE)
    created.append(kept)
    said = kept_container(stderr)
    if said is not None and said != kept:
        raise AssertionError(f"runner says it kept {said}, expected {kept}:\n{stderr}")
    if code != 1:
        raise AssertionError(f"`one {PROBE}` exited {code}, expected 1:\n{stderr}")
    if KEPT.search(stderr) is None:
        raise AssertionError(f"runner did not say it kept the container:\n{stderr}")
    state = podman("container", "inspect", "--format", "{{.State.Status}}", kept)
    if state.returncode != 0:
        raise AssertionError(f"failed test's container {kept} was not kept: {state.stderr}")
    if state.stdout.strip() != "exited":
        raise AssertionError(f"kept container {kept} is {state.stdout.strip()!r}, not exited")
    logs = podman("logs", kept)
    if PROBE_MESSAGE not in logs.stdout + logs.stderr:
        raise AssertionError(f"kept container {kept} did not run the probe:\n{logs.stderr}")
    print(f"e2e-one kept the failed test's container {kept} (exited, probe message in its log)")


def check_passed_is_removed(runner, created):
    """Step 2. Appends the container's name to `created` before checking."""
    code, stderr, pid = run_one(runner, PASSING)
    name = container_name(pid, PASSING)
    created.append(name)
    if code != 0:
        raise AssertionError(f"`one {PASSING}` exited {code}, expected 0:\n{stderr}")
    exists = podman("container", "exists", name)
    if exists.returncode != 1:
        raise AssertionError(
            f"passing test's container {name} remains (podman container exists: "
            f"{exists.returncode}) {exists.stderr}"
        )
    print(f"e2e-one removed the passing test's container {name}")


def main(argv):
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    runner = Path(argv[1])
    created = []
    verdict = 0
    try:
        check_failed_is_kept(runner, created)
        check_passed_is_removed(runner, created)
    except AssertionError as failure:
        print(f"e2e-one retention: FAILED: {failure}", file=sys.stderr)
        verdict = 1
    for name in created:
        # Only containers these runs created; --ignore: the passing one is
        # normally gone already.
        removed = podman("rm", "--force", "--ignore", "--time", "0", name)
        if removed.returncode != 0:
            print(f"e2e-one retention: could not remove {name}: {removed.stderr}", file=sys.stderr)
            verdict = 1
    print(f"e2e-one retention: {'OK' if verdict == 0 else 'FAILED'}")
    return verdict


if __name__ == "__main__":
    sys.exit(main(sys.argv))
