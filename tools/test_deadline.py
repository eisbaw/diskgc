"""Tests for tools/deadline.py with real short-lived processes."""

import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
sys.path.insert(0, str(TOOLS))

import deadline  # noqa: E402


def gone(pid, wait=5.0):
    """Whether `pid` has ended (absent, or a zombie its new parent has not
    reaped yet), polling up to `wait` seconds."""
    stop = time.monotonic() + wait
    while True:
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1][0]
        except (FileNotFoundError, ProcessLookupError):
            return True
        if state == "Z":
            return True
        if time.monotonic() > stop:
            return False
        time.sleep(0.02)


class Deadline(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.pid_file = Path(self.dir.name) / "helper.pid"

    def tearDown(self):
        self.dir.cleanup()

    def helper_pid(self):
        stop = time.monotonic() + 5
        while not self.pid_file.exists() or not self.pid_file.read_text().strip():
            if time.monotonic() > stop:
                raise AssertionError("the helper never wrote its pid")
            time.sleep(0.02)
        return int(self.pid_file.read_text())

    def test_the_exit_code_passes_through(self):
        self.assertEqual(deadline.run(["sh", "-c", "exit 3"], 30), 3)
        self.assertEqual(deadline.run(["true"], 30), 0)

    def test_a_signal_death_is_128_plus_the_signal(self):
        self.assertEqual(deadline.run(["sh", "-c", "kill -TERM $$"], 30), 128 + signal.SIGTERM)

    def test_the_deadline_stops_the_whole_group_and_exits_124(self):
        started = time.monotonic()
        code = deadline.run(
            ["sh", "-c", f'sleep 60 & echo $! > "{self.pid_file}"; wait'], 1, grace=1
        )
        self.assertEqual(code, deadline.DEADLINE_EXIT)
        self.assertLess(time.monotonic() - started, 10)
        self.assertTrue(gone(self.helper_pid()), "the command's child survived the deadline")

    def test_sigterm_to_the_wrapper_reaches_the_commands_children(self):
        wrapper = subprocess.Popen(
            [
                sys.executable,
                str(TOOLS / "deadline.py"),
                "60",
                "sh",
                "-c",
                f'sleep 60 & echo $! > "{self.pid_file}"; wait',
            ],
            start_new_session=True,
        )
        helper = self.helper_pid()
        wrapper.send_signal(signal.SIGTERM)
        self.assertEqual(wrapper.wait(timeout=10), 128 + signal.SIGTERM)
        self.assertTrue(gone(helper), "SIGTERM was not forwarded to the command's group")

    def test_sigint_to_the_wrapper_stops_the_command(self):
        wrapper = subprocess.Popen(
            [
                sys.executable,
                str(TOOLS / "deadline.py"),
                "60",
                "sh",
                "-c",
                f'echo $$ > "{self.pid_file}"; exec sleep 60',
            ],
            start_new_session=True,
        )
        command = self.helper_pid()
        wrapper.send_signal(signal.SIGINT)
        self.assertEqual(wrapper.wait(timeout=10), 128 + signal.SIGINT)
        self.assertTrue(gone(command), "SIGINT was not forwarded to the command")

    def test_signal_handlers_are_restored(self):
        before = {signum: signal.getsignal(signum) for signum in deadline.FORWARDED}
        deadline.run(["true"], 30)
        self.assertEqual({signum: signal.getsignal(signum) for signum in deadline.FORWARDED}, before)

    def test_usage_errors(self):
        for argv in (["deadline.py"], ["deadline.py", "10"], ["deadline.py", "x", "true"],
                     ["deadline.py", "0", "true"]):
            with self.subTest(argv=argv):
                with open(os.devnull, "w") as null:
                    saved, sys.stderr = sys.stderr, null
                    try:
                        self.assertEqual(deadline.main(argv), 2)
                    finally:
                        sys.stderr = saved


if __name__ == "__main__":
    unittest.main()
