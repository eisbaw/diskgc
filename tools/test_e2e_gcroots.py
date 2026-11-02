"""Tests for tools/e2e_gcroots.py: which .gcroots links are stale, against
this repository's real justfile and a scratch checkout."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import e2e_gcroots  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
TOOL = Path(__file__).resolve().parent / "e2e_gcroots.py"


class NamesInUse(unittest.TestCase):
    def test_the_real_justfile_roots_the_dev_shell_and_every_runner_it_calls(self):
        names = e2e_gcroots.names_in_use((REPO / "justfile").read_text())
        for name in ["dev", "e2e-runner", "e2e-runner-debian", "e2e-runner-docker", "e2e-runner-release"]:
            self.assertIn(name, names)
        self.assertNotIn("e2e-runner-release-01", names)

    def test_interpolated_names_are_not_names(self):
        self.assertEqual(e2e_gcroots.names_in_use("nix build -o .gcroots/{{NAME}} .#{{NAME}}"), set())


class Plan(unittest.TestCase):
    def test_only_store_links_no_recipe_builds_are_stale(self):
        rows = e2e_gcroots.plan(
            [
                ("dev", "/nix/store/a-dev"),
                ("e2e-runner-release-01", "/nix/store/b-old"),
                ("notes.txt", None),
                ("elsewhere", "/home/x/result"),
            ],
            {"dev"},
        )
        self.assertEqual(
            [(name, verdict) for name, verdict, _ in rows],
            [("dev", "in-use"), ("e2e-runner-release-01", "stale"), ("elsewhere", "foreign"), ("notes.txt", "foreign")],
        )


class EndToEnd(unittest.TestCase):
    def test_apply_drops_the_stale_link_only(self):
        with tempfile.TemporaryDirectory() as d:
            checkout = Path(d)
            (checkout / "justfile").write_text("_runner NAME:\n    just _runner e2e-runner x\n    nix build -o .gcroots/dev\n")
            roots = checkout / ".gcroots"
            roots.mkdir()
            os.symlink("/nix/store/x-runner", roots / "e2e-runner")
            os.symlink("/nix/store/x-dev", roots / "dev")
            os.symlink("/nix/store/x-old", roots / "e2e-runner-release-01")
            (roots / "keep-me").write_text("not a link")
            dry = subprocess.run([sys.executable, str(TOOL), d], capture_output=True, text=True, check=False)
            self.assertEqual(dry.returncode, 0, dry.stderr)
            self.assertIn("1 stale root(s)", dry.stdout)
            self.assertTrue((roots / "e2e-runner-release-01").is_symlink())
            done = subprocess.run(
                [sys.executable, str(TOOL), "--apply", d], capture_output=True, text=True, check=False
            )
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(sorted(os.listdir(roots)), ["dev", "e2e-runner", "keep-me"])

    def test_a_justfile_naming_no_root_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "justfile").write_text("check:\n    true\n")
            done = subprocess.run([sys.executable, str(TOOL), "--apply", d], capture_output=True, text=True, check=False)
            self.assertEqual(done.returncode, 2)


if __name__ == "__main__":
    unittest.main()
