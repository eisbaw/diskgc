"""Tests for tools/e2e_kept.py: which containers are the harness's, which
--prune and --drop remove, and that the label mirrors runner.rs (Podman is
a fake executable on PATH in the end-to-end cases)."""

import json
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import e2e_kept  # noqa: E402

RUNNER_RS = Path(__file__).resolve().parent.parent / "diskgc-e2e" / "src" / "runner.rs"
HERE = "/work/t311"
NOW = 10_000_000
DAY = 24 * 3600


def container(name, created, label=HERE, state="exited", image="localhost/diskgc-e2e-base:x"):
    labels = None if label is None else {e2e_kept.CHECKOUT_LABEL: label}
    return {"Names": [name], "Created": created, "Labels": labels, "State": state, "Image": image}


CONTAINERS = [
    *(container(f"diskgc-e2e-{i}-mine", NOW - i) for i in range(1, 8)),
    container("diskgc-e2e-50-running", NOW, state="running"),
    container("diskgc-e2e-60-other", NOW - 5, label="/work/t295"),
    container("diskgc-e2e-61-orphan", NOW - 5, label="/work/removed"),
    container("diskgc-e2e-70-legacy", NOW - 2 * DAY, label=None),
    container("diskgc-e2e-71-legacy-young", NOW - 3600, label=None),
    container("diskgc-e2e-72-legacy-other-image", NOW - 2 * DAY, label=None, image="docker.io/x:y"),
    container("walk-diskgc-podman", NOW - 2 * DAY, label=None),
    container("diskgc-e2e-builder", NOW - 2 * DAY, label=None),
    container("mydb", NOW - 2 * DAY, label=HERE),
]


def verdicts(keep=5):
    rows = e2e_kept.classify(CONTAINERS, HERE, keep, lambda p: p != "/work/removed", NOW)
    return {e2e_kept.name_of(c): v for c, v in rows}


class Classify(unittest.TestCase):
    def test_newest_n_of_this_checkout_are_kept_the_rest_surplus(self):
        v = verdicts()
        self.assertEqual([v[f"diskgc-e2e-{i}-mine"] for i in range(1, 8)], ["kept"] * 5 + ["surplus"] * 2)
        self.assertEqual(verdicts(keep=0)["diskgc-e2e-1-mine"], "surplus")

    def test_other_checkouts_orphans_running_and_legacy(self):
        v = verdicts()
        self.assertEqual(v["diskgc-e2e-50-running"], "active")
        self.assertEqual(v["diskgc-e2e-60-other"], "other")
        self.assertEqual(v["diskgc-e2e-61-orphan"], "orphan")
        self.assertEqual(v["diskgc-e2e-70-legacy"], "legacy")
        self.assertEqual(v["diskgc-e2e-71-legacy-young"], "legacy-recent")

    def test_never_another_programs_container(self):
        v = verdicts()
        for name in ["walk-diskgc-podman", "diskgc-e2e-builder", "mydb", "diskgc-e2e-72-legacy-other-image"]:
            self.assertNotIn(name, v)

    def test_what_each_mode_removes(self):
        rows = e2e_kept.classify(CONTAINERS, HERE, 5, lambda p: p != "/work/removed", NOW)
        names = lambda mode, legacy=False: sorted(  # noqa: E731
            e2e_kept.name_of(c) for c in e2e_kept.removable(rows, mode, legacy)
        )
        self.assertEqual(names("list"), [])
        self.assertEqual(names("prune"), ["diskgc-e2e-6-mine", "diskgc-e2e-61-orphan", "diskgc-e2e-7-mine"])
        self.assertEqual(len(names("drop")), 8)
        self.assertNotIn("diskgc-e2e-70-legacy", names("drop"))
        self.assertIn("diskgc-e2e-70-legacy", names("drop", legacy=True))
        self.assertNotIn("diskgc-e2e-71-legacy-young", names("drop", legacy=True))
        self.assertNotIn("diskgc-e2e-60-other", names("drop", legacy=True))

    def test_usage(self):
        self.assertEqual(e2e_kept.parse([]), ("list", False, 5))
        self.assertEqual(e2e_kept.parse(["--drop", "--legacy"]), ("drop", True, 5))
        self.assertEqual(e2e_kept.parse(["--prune", "--keep", "2"]), ("prune", False, 2))
        for bad in (["--prune", "--drop"], ["--keep"], ["--keep", "-1"], ["--keep", "x"], ["x"]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                e2e_kept.parse(bad)


class MirrorsTheRunner(unittest.TestCase):
    def test_label_and_name_prefix_match_runner_rs(self):
        source = RUNNER_RS.read_text()
        label = re.search(r'pub const CHECKOUT_LABEL: &str = "([^"]*)";', source)
        self.assertIsNotNone(label)
        self.assertEqual(label.group(1), e2e_kept.CHECKOUT_LABEL)
        self.assertIn('format!("diskgc-e2e-{runner_pid}-{safe}")', source)


FAKE_PODMAN = r"""#!/usr/bin/env python3
import json, os, sys
state = os.environ["FAKE_STATE"]
with open(os.path.join(state, "calls"), "a") as log:
    log.write(" ".join(sys.argv[1:]) + "\n")
containers = json.load(open(os.path.join(state, "containers.json")))
if sys.argv[1:] == ["ps", "--all", "--format", "json"]:
    print(json.dumps(containers))
elif sys.argv[1:3] == ["rm", "--volumes"]:
    sys.exit(1 if sys.argv[3].endswith("-busy") else 0)
else:
    sys.exit(125)
"""


class EndToEndWithFakePodman(unittest.TestCase):
    def run_tool(self, *args):
        with tempfile.TemporaryDirectory() as d:
            state = Path(d)
            (state / "bin").mkdir()
            (state / "bin" / "podman").write_text(FAKE_PODMAN)
            (state / "bin" / "podman").chmod(0o755)
            checkout = (state / "checkout").resolve()
            checkout.mkdir()
            now = time.time()
            containers = [
                *(container(f"diskgc-e2e-{i}-mine", now - i, label=str(checkout)) for i in range(1, 8)),
                container("diskgc-e2e-9-busy", now - 100, label=str(checkout)),
                container("diskgc-e2e-60-other", now, label=str(state)),
                container("diskgc-e2e-70-legacy", now - 2 * DAY, label=None),
                container("walk-diskgc-podman", now - 2 * DAY, label=None),
            ]
            (state / "containers.json").write_text(json.dumps(containers))
            env = dict(os.environ, PATH=f"{state / 'bin'}:{os.environ['PATH']}", FAKE_STATE=str(state))
            done = subprocess.run(
                [sys.executable, str(Path(__file__).resolve().parent / "e2e_kept.py"), *args],
                cwd=checkout,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            calls = (state / "calls").read_text().splitlines()
        return done, sorted(c.split()[-1] for c in calls if c.startswith("rm"))

    def test_list_removes_nothing(self):
        done, removed = self.run_tool()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(removed, [])
        self.assertIn("legacy   diskgc-e2e-70-legacy", done.stdout)

    def test_prune_keeps_the_newest_five_and_reports_a_failed_removal(self):
        done, removed = self.run_tool("--prune")
        self.assertEqual(removed, ["diskgc-e2e-6-mine", "diskgc-e2e-7-mine", "diskgc-e2e-9-busy"])
        self.assertEqual(done.returncode, 1)
        self.assertIn("failed   diskgc-e2e-9-busy", done.stderr)

    def test_drop_legacy_removes_this_checkouts_and_old_unlabelled_ones_only(self):
        done, removed = self.run_tool("--drop", "--legacy")
        self.assertIn("diskgc-e2e-70-legacy", removed)
        self.assertEqual(len(removed), 9)
        self.assertNotIn("diskgc-e2e-60-other", removed)
        self.assertNotIn("walk-diskgc-podman", removed)


if __name__ == "__main__":
    unittest.main()
