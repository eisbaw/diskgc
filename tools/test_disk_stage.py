"""Tests for tools/disk_stage.py: the budget table matches docs/testing.md,
the mutation budget is never below run_mutants.py's own guard, a stage
below its budget does not run, and a run reports free space and keeps
its command's exit status."""

import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import disk_stage  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
TOOL = Path(__file__).resolve().parent / "disk_stage.py"


def documented_budgets():
    """{stage: GiB} from the Disk budgets table of docs/testing.md."""
    text = (REPO / "docs" / "testing.md").read_text()
    section = text.split("\n### Disk budgets\n", 1)[1].split("\n### ", 1)[0]
    return {
        stage: float(gib)
        for stage, gib in re.findall(r"^\| `([a-z-]+)` \| ([0-9.]+) GiB", section, re.MULTILINE)
    }


class Budgets(unittest.TestCase):
    def test_docs_state_every_budget_and_only_those(self):
        self.assertEqual(
            documented_budgets(),
            {stage: round(budget / disk_stage.GIB, 2) for stage, (budget, _) in disk_stage.STAGES.items()},
        )

    def test_mutants_budget_is_not_weaker_than_the_runner_guard(self):
        # The shell runner's start guard: disk_floor plus 64 MiB.
        source = (REPO / "tools" / "mutants_container_smoke.sh").read_text()
        floor = re.search(r'^disk_floor="\$\(\((\d+) \* 1024 \* 1024 \* 1024\)\)"$', source, re.MULTILINE)
        extra = re.search(r'^minimum="\$\(\(disk_floor \+ (\d+) \* 1024 \* 1024\)\)"$', source, re.MULTILINE)
        self.assertIsNotNone(floor)
        self.assertIsNotNone(extra)
        guard = int(floor.group(1)) * disk_stage.GIB + int(extra.group(1)) * disk_stage.MIB
        self.assertEqual(guard, 16 * disk_stage.GIB + 64 * disk_stage.MIB)
        self.assertGreaterEqual(disk_stage.STAGES["mutants"][0], guard)

    def test_shortfalls(self):
        self.assertEqual(disk_stage.shortfalls({"/a": 5, "/b": 10}, 6), [("/a", 5)])
        self.assertEqual(disk_stage.shortfalls({"/a": 6}, 6), [])


class Run(unittest.TestCase):
    def test_a_stage_reports_free_space_and_keeps_the_exit_status(self):
        with tempfile.TemporaryDirectory() as d:
            done = subprocess.run(
                [sys.executable, str(TOOL), "check", "sh", "-c", "exit 3"],
                cwd=d,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(done.returncode, 3, done.stderr)
        self.assertRegex(done.stdout, r"disk: check: budget [0-9.]+ GiB; checkout")
        self.assertRegex(done.stdout, r"before [0-9.]+ GiB free, after [0-9.]+ GiB, delta [+-][0-9.]+ GiB, peak use")

    def test_a_stage_below_its_budget_does_not_run(self):
        with tempfile.TemporaryDirectory() as d:
            marker = Path(d) / "ran"
            code = (
                "import sys; sys.path.insert(0, %r); import disk_stage as s; "
                "s.STAGES['check'] = (2**62, False); sys.exit(s.main(['x', 'check', 'touch', %r]))"
            ) % (str(TOOL.parent), str(marker))
            done = subprocess.run([sys.executable, "-c", code], cwd=d, capture_output=True, text=True, check=False)
            self.assertEqual(done.returncode, 2)
            self.assertFalse(marker.exists())
        self.assertIn("disk: check: REFUSED:", done.stderr)
        self.assertIn("do not lower the budget", done.stderr)

    def test_unknown_stage_or_missing_command_exits_2(self):
        for args in (["nope", "true"], ["check"]):
            done = subprocess.run([sys.executable, str(TOOL), *args], capture_output=True, text=True, check=False)
            self.assertEqual(done.returncode, 2, args)


if __name__ == "__main__":
    unittest.main()
