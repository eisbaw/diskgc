#!/usr/bin/env python3
"""Run a command with a wall-clock deadline: a hang guard for the gate
recipes' `just check` stage (docs/testing.md, Commands; TASK-60).

Usage: python3 tools/deadline.py SECONDS COMMAND [ARGS...]

The command runs in its own process group. At the deadline the whole group
gets SIGTERM, then SIGKILL after GRACE_SECS, and this exits 124 (as
coreutils `timeout` does). Unlike `timeout`, this process stays in the
terminal's foreground process group and forwards SIGINT, SIGTERM and SIGHUP
to the command's group, so Ctrl-C still stops the command and its children
(`timeout` would leave them running) while a hung one is still killed
(`timeout --foreground` would kill only the command, not its children).

Exit: the command's exit code (128 + N when signal N ended it), 124 at the
deadline, 2 on a usage error."""

import os
import signal
import subprocess
import sys

GRACE_SECS = 10
DEADLINE_EXIT = 124
FORWARDED = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def exit_code(returncode):
    """Popen's returncode (negative for a signal) as a shell exit code."""
    return 128 - returncode if returncode < 0 else returncode


def signal_group(pgid, signum):
    try:
        os.killpg(pgid, signum)
    except ProcessLookupError:
        pass  # the group is gone already


def run(command, seconds, grace=GRACE_SECS):
    # Handlers first: a signal that arrives before the command exists is
    # held and forwarded once it does, instead of orphaning it.
    started = []
    pending = []

    def forward(signum, _frame):
        if started:
            signal_group(started[0].pid, signum)
        else:
            pending.append(signum)

    previous = {signum: signal.signal(signum, forward) for signum in FORWARDED}
    try:
        child = subprocess.Popen(command, process_group=0)
        started.append(child)
        for signum in pending:
            signal_group(child.pid, signum)
        return wait_or_stop(child, command, seconds, grace)
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def wait_or_stop(child, command, seconds, grace):
    try:
        return exit_code(child.wait(timeout=seconds))
    except subprocess.TimeoutExpired:
        print(
            f"deadline: {' '.join(command)} still running after {seconds} s; "
            "stopping its process group",
            file=sys.stderr,
            flush=True,
        )
        signal_group(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            signal_group(child.pid, signal.SIGKILL)
            child.wait()
        # Children that ignored SIGTERM may outlive the leader. While any
        # member lives the group id cannot be reused, so this reaches only
        # them (or nothing).
        signal_group(child.pid, signal.SIGKILL)
        return DEADLINE_EXIT


def main(argv):
    if len(argv) < 3 or not argv[1].isdigit() or int(argv[1]) <= 0:
        print(__doc__, file=sys.stderr)
        return 2
    return run(argv[2:], int(argv[1]))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
