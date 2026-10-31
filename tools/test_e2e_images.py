"""Tests for tools/e2e_images.py: which tags it would remove, and that
--apply removes only those (Podman and Nix are not run)."""

import contextlib
import io
import sys
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
        return {ref: verdict for ref, verdict, _ in e2e_images.plan(IMAGES, CURRENT)}

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
            sorted(ref for ref, verdict, _ in rows if verdict == "old"),
        )
        self.assertNotIn("localhost/diskgc-e2e-base:new", asked)
        self.assertNotIn("localhost/diskgc-e2e-base:pinned", asked)
        self.assertEqual(failed, 1)


class Refs(unittest.TestCase):
    def test_harness_namespace(self):
        self.assertTrue(e2e_images.is_harness_ref("localhost/diskgc-e2e:t"))
        self.assertTrue(e2e_images.is_harness_ref("localhost/diskgc-e2e-dind:t"))
        self.assertFalse(e2e_images.is_harness_ref("localhost/diskgc-e2ex:t"))
        self.assertFalse(e2e_images.is_harness_ref("docker.io/diskgc-e2e-base:t"))


if __name__ == "__main__":
    unittest.main()
