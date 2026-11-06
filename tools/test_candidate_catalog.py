"""Tests for candidate_catalog.py (TASK-350's test-only candidate catalog)."""

import unittest
from pathlib import Path

import candidate_catalog

LIBRARY = """\
# header
- name: a
  description: d
  liveness_class: tolerant

- name: b
  find: {roots: required}
  liveness_class: delegated
  releases: ["0.1", "0.2"]

- name: c
  liveness_class: verified-heuristic
"""


class Admit(unittest.TestCase):
    def test_inserts_after_liveness_class_of_each_named_entry(self):
        out = candidate_catalog.admit(LIBRARY, "0.2", ["c", "a"])
        self.assertIn('  liveness_class: tolerant\n  releases: ["0.2"]\n\n- name: b', out)
        self.assertTrue(out.endswith('  liveness_class: verified-heuristic\n  releases: ["0.2"]\n'))
        # b is untouched, and nothing else moved.
        self.assertEqual(out.count("releases:"), 3)

    def test_refuses_an_entry_already_in_a_catalog(self):
        with self.assertRaisesRegex(ValueError, "b already has a releases line"):
            candidate_catalog.admit(LIBRARY, "0.2", ["b"])

    def test_refuses_an_unknown_or_repeated_name_and_a_bad_release(self):
        with self.assertRaisesRegex(ValueError, "zz is not a top-level entry"):
            candidate_catalog.admit(LIBRARY, "0.2", ["zz"])
        with self.assertRaisesRegex(ValueError, "named twice"):
            candidate_catalog.admit(LIBRARY, "0.2", ["a", "a"])
        with self.assertRaisesRegex(ValueError, "not MAJOR.MINOR"):
            candidate_catalog.admit(LIBRARY, "v0.2", ["a"])

    def test_an_empty_list_changes_nothing(self):
        self.assertEqual(candidate_catalog.admit(LIBRARY, "0.2", []), LIBRARY)
        with self.assertRaisesRegex(ValueError, "not MAJOR.MINOR"):
            candidate_catalog.admit(LIBRARY, "v0.2", [])

    def test_main_leaves_the_file_alone_for_an_empty_list(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            library, names = Path(tmp) / "builtin.yaml", Path(tmp) / "list"
            library.write_text(LIBRARY)
            names.write_text("# no candidates\n\n")
            self.assertEqual(candidate_catalog.main([str(library), "0.2", str(names)]), 0)
            self.assertEqual(library.read_text(), LIBRARY)

    def test_refuses_an_entry_without_one_liveness_class(self):
        text = "- name: x\n  description: d\n"
        with self.assertRaisesRegex(ValueError, "x has 0 liveness_class"):
            candidate_catalog.admit(text, "0.2", ["x"])

    def test_refuses_an_entry_start_it_cannot_read(self):
        text = "- name: a\n  liveness_class: tolerant\n- name: b  # note\n  liveness_class: tolerant\n"
        with self.assertRaisesRegex(ValueError, "line 3 starts an entry"):
            candidate_catalog.admit(text, "0.2", ["a"])

    def test_the_candidate_list_patches_the_real_library(self):
        """The real files: every listed candidate can be patched into the real library."""
        real = Path(__file__).resolve().parent.parent / "crates/diskgc-core/src/profile/builtin.yaml"
        family = candidate_catalog.read_list(real.parents[4] / "diskgc-e2e/candidate-catalog")
        out = candidate_catalog.admit(real.read_text(), "0.2", family)
        self.assertEqual(out.count('releases: ["0.2"]\n'), real.read_text().count('releases: ["0.2"]\n') + len(family))


if __name__ == "__main__":
    unittest.main()
