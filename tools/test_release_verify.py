"""Tests for tools/release_verify.py's decision (the git facts are inputs)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from release_verify import verdict  # noqa: E402

HEAD = "080389d" + "0" * 33
OTHER = "af3d503" + "0" * 33


class Verdict(unittest.TestCase):
    def test_a_development_version_passes_anywhere(self):
        self.assertTrue(verdict("0.2.0-dev", HEAD, None, True)[0])

    def test_a_release_version_on_its_clean_tag_passes(self):
        self.assertTrue(verdict("0.2.0", HEAD, HEAD, False)[0])

    def test_a_release_version_without_its_tag_fails(self):
        ok, message = verdict("0.2.0", HEAD, None, False)
        self.assertFalse(ok)
        self.assertIn("no tag v0.2.0", message)

    def test_a_release_version_off_its_tag_fails(self):
        # A backport commit after the tag that kept the plain version.
        ok, message = verdict("0.1.0", HEAD, OTHER, False)
        self.assertFalse(ok)
        self.assertIn("is not v0.1.0", message)

    def test_a_release_version_on_a_dirty_tag_fails(self):
        ok, message = verdict("0.2.0", HEAD, HEAD, True)
        self.assertFalse(ok)
        self.assertIn("uncommitted", message)


if __name__ == "__main__":
    unittest.main()
