"""Tests for tools/e2e_images.py: which tags it would remove, that --apply
removes only those, and the last-used stamps and their grace period
(Podman and Nix are not run; the end-to-end path uses fake `podman` and
`nix` executables on PATH)."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import e2e_images  # noqa: E402

CURRENT = [
    "localhost/diskgc-e2e-base:new",
    "localhost/diskgc-e2e-debian:new",
    "localhost/diskgc-e2e-vm:new",
]


def image(names, containers=0, size=2**30):
    return {"Id": "x", "Names": names, "Containers": containers, "Size": size}


IMAGES = [
    image(["localhost/diskgc-e2e-base:new"], containers=3),
    image(["localhost/diskgc-e2e-base:old"]),
    image(["localhost/diskgc-e2e-base:pinned"], containers=1),
    image(["localhost/diskgc-e2e-debian:old", "localhost/diskgc-e2e-debian:new"]),
    image(["localhost/diskgc-e2e:renamed-long-ago"]),
    image(["docker.io/library/debian:trixie-slim"]),
    image(["localhost/diskgc-e2e-retired-image:x"]),
    image(["localhost/diskgc-e2ex:not-ours"]),
    image(["localhost/other/diskgc-e2e-base:not-ours"]),
    image(None),
]


class Plan(unittest.TestCase):
    def verdicts(self):
        return {ref: verdict for ref, verdict, _, _ in e2e_images.plan(IMAGES, CURRENT)}

    def test_current_in_use_and_old_harness_tags(self):
        self.assertEqual(
            self.verdicts(),
            {
                "localhost/diskgc-e2e-base:new": "current",
                "localhost/diskgc-e2e-base:old": "old",
                "localhost/diskgc-e2e-base:pinned": "in-use",
                "localhost/diskgc-e2e-debian:old": "old",
                "localhost/diskgc-e2e-debian:new": "current",
                "localhost/diskgc-e2e:renamed-long-ago": "old",
                "localhost/diskgc-e2e-retired-image:x": "old",
            },
        )

    def test_other_images_are_not_even_listed(self):
        listed = self.verdicts()
        for ref in [
            "docker.io/library/debian:trixie-slim",
            "localhost/diskgc-e2ex:not-ours",
            "localhost/other/diskgc-e2e-base:not-ours",
        ]:
            self.assertNotIn(ref, listed)

    def test_the_summary_counts_old_tags(self):
        text = e2e_images.describe(e2e_images.plan(IMAGES, CURRENT))
        self.assertIn("4 old tag(s) of 7 harness tag(s)", text)
        self.assertIn("in-use   localhost/diskgc-e2e-base:pinned  (1.00 GiB, 1 container(s))", text)


class Apply(unittest.TestCase):
    def test_removes_only_old_tags_and_counts_failures(self):
        rows = e2e_images.plan(IMAGES, CURRENT)
        asked = []

        def rmi(ref):
            asked.append(ref)
            return ref != "localhost/diskgc-e2e:renamed-long-ago"

        with contextlib.redirect_stdout(io.StringIO()) as out:
            failed = e2e_images.apply(rows, rmi)
        self.assertIn("removed  localhost/diskgc-e2e-base:old", out.getvalue())
        self.assertEqual(
            sorted(asked),
            sorted(ref for ref, verdict, _, _ in rows if verdict == "old"),
        )
        self.assertNotIn("localhost/diskgc-e2e-base:new", asked)
        self.assertNotIn("localhost/diskgc-e2e-base:pinned", asked)
        self.assertEqual(failed, 1)


HOUR = 3600
DAY = 24 * HOUR


class Grace(unittest.TestCase):
    def verdicts(self, ages):
        rows = e2e_images.plan(IMAGES, CURRENT, ages, grace_secs=DAY)
        return {ref: verdict for ref, verdict, _, _ in rows}

    def test_a_tag_used_within_the_grace_period_is_kept(self):
        verdicts = self.verdicts(
            {"localhost/diskgc-e2e-base:old": 23 * HOUR, "localhost/diskgc-e2e-debian:old": 25 * HOUR}
        )
        self.assertEqual(verdicts["localhost/diskgc-e2e-base:old"], "recent")
        self.assertEqual(verdicts["localhost/diskgc-e2e-debian:old"], "old")

    def test_a_never_stamped_tag_is_kept_under_a_grace_period_only(self):
        self.assertEqual(self.verdicts({})["localhost/diskgc-e2e:renamed-long-ago"], "recent")
        rows = e2e_images.plan(IMAGES, CURRENT, {}, grace_secs=0)
        self.assertIn(("localhost/diskgc-e2e:renamed-long-ago", "old"), [(r, v) for r, v, _, _ in rows])

    def test_current_and_in_use_win_over_any_stamp(self):
        verdicts = self.verdicts(
            {"localhost/diskgc-e2e-base:new": 99 * DAY, "localhost/diskgc-e2e-base:pinned": 99 * DAY}
        )
        self.assertEqual(verdicts["localhost/diskgc-e2e-base:new"], "current")
        self.assertEqual(verdicts["localhost/diskgc-e2e-base:pinned"], "in-use")


class Stamps(unittest.TestCase):
    def test_touch_and_ages_round_trip_through_file_names(self):
        with tempfile.TemporaryDirectory() as d:
            e2e_images.touch(d, "localhost/diskgc-e2e-base:t1", now=1000)
            e2e_images.touch(d, "localhost/diskgc-e2e:t2", now=500)
            self.assertEqual(
                e2e_images.stamp_ages(d, 2000),
                {"localhost/diskgc-e2e-base:t1": 1000, "localhost/diskgc-e2e:t2": 1500},
            )
            self.assertTrue(all("/" not in n for n in os.listdir(d)))

    def test_only_harness_tags_are_stamped(self):
        with tempfile.TemporaryDirectory() as d, self.assertRaises(ValueError):
            e2e_images.touch(d, "docker.io/library/debian:trixie-slim")

    def test_refresh_stamps_current_and_first_seen_and_forgets_absent_tags(self):
        with tempfile.TemporaryDirectory() as d:
            e2e_images.touch(d, "localhost/diskgc-e2e-base:new", now=0)
            e2e_images.touch(d, "localhost/diskgc-e2e-base:old", now=0)
            e2e_images.touch(d, "localhost/diskgc-e2e-base:gone-from-podman", now=0)
            e2e_images.refresh_stamps(d, IMAGES, CURRENT, now=DAY)
            ages = e2e_images.stamp_ages(d, DAY)
        self.assertEqual(ages["localhost/diskgc-e2e-base:new"], 0)  # current: used now
        self.assertEqual(ages["localhost/diskgc-e2e-base:old"], DAY)  # kept its age
        self.assertEqual(ages["localhost/diskgc-e2e:renamed-long-ago"], 0)  # first seen now
        self.assertNotIn("localhost/diskgc-e2e-base:gone-from-podman", ages)
        self.assertNotIn("docker.io/library/debian:trixie-slim", ages)

    def test_the_runner_package_records_its_image(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / e2e_images.RUNNER_IMAGE_REF
            path.parent.mkdir(parents=True)
            path.write_text("localhost/diskgc-e2e-base:abc\n")
            self.assertEqual(e2e_images.runner_image_ref(d), "localhost/diskgc-e2e-base:abc")
            path.write_text("docker.io/x:y\n")
            with self.assertRaises(RuntimeError):
                e2e_images.runner_image_ref(d)
        with tempfile.TemporaryDirectory() as d, self.assertRaises(RuntimeError):
            e2e_images.runner_image_ref(d)


FAKE_PODMAN = """#!/usr/bin/env python3
import json, os, sys
state = os.environ["FAKE_STATE"]
log = open(os.path.join(state, "calls"), "a")
log.write(" ".join(sys.argv[1:]) + "\\n")
images = json.load(open(os.path.join(state, "images.json")))
if sys.argv[1:3] == ["images", "--format"]:
    print(json.dumps(images))
elif sys.argv[1] == "rmi":
    ref = sys.argv[2]
    if ref.endswith(":stuck"):
        sys.stderr.write("image is in use\\n")
        sys.exit(2)
    for image in images:
        image["Names"] = [n for n in image["Names"] if n != ref]
    json.dump([i for i in images if i["Names"]], open(os.path.join(state, "images.json"), "w"))
elif sys.argv[1:3] == ["image", "exists"]:
    sys.exit(0 if any(sys.argv[3] in i["Names"] for i in images) else 1)
else:
    sys.exit(125)
"""

FAKE_NIX = """#!/bin/sh
echo '["localhost/diskgc-e2e-base:new"]'
"""


class EndToEndWithFakes(unittest.TestCase):
    """main() against fake podman and nix on PATH and a private stamp dir."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.state = Path(self.dir.name)
        bin_dir = self.state / "bin"
        bin_dir.mkdir()
        for name, text in (("podman", FAKE_PODMAN), ("nix", FAKE_NIX)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        images = [
            {"Names": ["localhost/diskgc-e2e-base:new"], "Containers": 0, "Size": 1},
            {"Names": ["localhost/diskgc-e2e-base:old"], "Containers": 0, "Size": 1},
            {"Names": ["localhost/diskgc-e2e-base:yesterday"], "Containers": 0, "Size": 1},
            {"Names": ["localhost/diskgc-e2e-base:stuck"], "Containers": 0, "Size": 1},
            {"Names": ["localhost/diskgc-e2e-base:kept"], "Containers": 1, "Size": 1},
            {"Names": ["docker.io/library/debian:old"], "Containers": 0, "Size": 1},
        ]
        (self.state / "images.json").write_text(json.dumps(images))
        self.env = dict(
            os.environ,
            PATH=f"{bin_dir}:{os.environ['PATH']}",
            FAKE_STATE=str(self.state),
            DISKGC_E2E_STATE_DIR=str(self.state / "state"),
        )
        stamps = self.state / "state" / "image-stamps"
        old = time.time() - 2 * DAY
        for ref, when in (
            ("localhost/diskgc-e2e-base:old", old),
            ("localhost/diskgc-e2e-base:stuck", old),
            ("localhost/diskgc-e2e-base:yesterday", time.time() - 2 * HOUR),
        ):
            e2e_images.touch(stamps, ref, now=when)

    def tearDown(self):
        self.dir.cleanup()

    def run_tool(self, *args):
        tool = Path(__file__).resolve().parent / "e2e_images.py"
        return subprocess.run(
            [sys.executable, str(tool), *args], env=self.env, capture_output=True, text=True, check=False
        )

    def calls(self):
        path = self.state / "calls"
        return path.read_text().splitlines() if path.exists() else []

    def test_a_dry_run_removes_and_stamps_nothing(self):
        before = sorted(os.listdir(self.state / "state" / "image-stamps"))
        done = self.run_tool()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertNotIn("rmi", " ".join(self.calls()))
        self.assertEqual(sorted(os.listdir(self.state / "state" / "image-stamps")), before)
        self.assertIn("old      localhost/diskgc-e2e-base:old", done.stdout)
        self.assertIn("recent   localhost/diskgc-e2e-base:yesterday", done.stdout)

    def test_apply_removes_only_old_harness_tags_past_the_grace_period(self):
        done = self.run_tool("--apply")
        self.assertEqual(done.returncode, 1, done.stderr)  # :stuck failed
        rmis = [c for c in self.calls() if c.startswith("rmi")]
        self.assertEqual(sorted(rmis), ["rmi localhost/diskgc-e2e-base:old", "rmi localhost/diskgc-e2e-base:stuck"])
        self.assertIn("failed   localhost/diskgc-e2e-base:stuck", done.stderr)
        stamps = e2e_images.stamp_ages(self.state / "state" / "image-stamps", time.time())
        self.assertNotIn("localhost/diskgc-e2e-base:old", stamps)  # forgotten with the tag
        self.assertIn("localhost/diskgc-e2e-base:stuck", stamps)
        self.assertLess(stamps["localhost/diskgc-e2e-base:kept"], HOUR)  # first seen now

    def test_no_grace_removes_recent_tags_too_but_never_current_or_in_use(self):
        done = self.run_tool("--apply", "--grace-hours", "0")
        rmis = sorted(c.split()[1] for c in self.calls() if c.startswith("rmi"))
        self.assertEqual(
            rmis,
            ["localhost/diskgc-e2e-base:old", "localhost/diskgc-e2e-base:stuck", "localhost/diskgc-e2e-base:yesterday"],
        )
        self.assertEqual(done.returncode, 1)

    def test_stamp_records_the_runner_image_and_bad_usage_exits_2(self):
        runner = self.state / "runner"
        (runner / "share" / "diskgc-e2e").mkdir(parents=True)
        (runner / "share" / "diskgc-e2e" / "image-ref").write_text("localhost/diskgc-e2e-base:new\n")
        done = self.run_tool("stamp", str(runner))
        self.assertEqual(done.returncode, 0, done.stderr)
        stamps = e2e_images.stamp_ages(self.state / "state" / "image-stamps", time.time())
        self.assertLess(stamps["localhost/diskgc-e2e-base:new"], HOUR)
        self.assertEqual(self.run_tool("stamp", str(self.state / "absent")).returncode, 2)
        self.assertEqual(self.run_tool("--bogus").returncode, 2)
        self.assertEqual(self.run_tool("--grace-hours", "-1").returncode, 2)


class Refs(unittest.TestCase):
    def test_harness_namespace(self):
        self.assertTrue(e2e_images.is_harness_ref("localhost/diskgc-e2e:t"))
        self.assertTrue(e2e_images.is_harness_ref("localhost/diskgc-e2e-dind:t"))
        self.assertFalse(e2e_images.is_harness_ref("localhost/diskgc-e2ex:t"))
        self.assertFalse(e2e_images.is_harness_ref("docker.io/diskgc-e2e-base:t"))


if __name__ == "__main__":
    unittest.main()
