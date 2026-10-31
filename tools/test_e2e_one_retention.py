"""Unit tests for tools/e2e_one_retention.py: its pure parts, and that the
names and messages it mirrors still match the Rust harness. The live check
itself runs in `just e2e` (it needs Podman and the runner)."""

import contextlib
import io
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import e2e_one_retention as retention  # noqa: E402

E2E_SRC = Path(__file__).resolve().parent.parent / "diskgc-e2e" / "src"


def rust_str_const(source, name):
    match = re.search(rf'pub const {name}: &str = "([^"]*)";', source)
    if match is None:
        raise AssertionError(f"no `pub const {name}: &str` in the Rust source")
    return match.group(1)


class MirrorsTheRustHarness(unittest.TestCase):
    def test_probe_name_and_message_match_suite_rs(self):
        suite = (E2E_SRC / "suite.rs").read_text()
        self.assertEqual(retention.PROBE, rust_str_const(suite, "RUNNER_FAILURE_PROBE"))
        self.assertEqual(
            retention.PROBE_MESSAGE, rust_str_const(suite, "RUNNER_FAILURE_PROBE_MESSAGE")
        )
        self.assertIn(f'name: "{retention.PASSING}"', suite)

    def test_kept_message_matches_runner_rs(self):
        runner = (E2E_SRC / "runner.rs").read_text()
        self.assertIn('"container {container} kept for inspection:', runner)

    def test_container_names_match_runner_rs_cases(self):
        # The cases of runner.rs container_names_carry_only_allowed_characters.
        self.assertEqual(
            retention.container_name(7, "cargo_smoke@nightly-2026-10-16"),
            "diskgc-e2e-7-cargo-smoke-nightly-2026-10-16",
        )
        self.assertEqual(
            retention.container_name(7, "cargo_smoke@1.90.0"), "diskgc-e2e-7-cargo-smoke-1.90.0"
        )
        self.assertEqual(
            retention.container_name(7, "sandbox_trivial"), "diskgc-e2e-7-sandbox-trivial"
        )


class KeptContainer(unittest.TestCase):
    def test_finds_the_kept_container_line(self):
        stderr = (
            "FAIL      runner_failure_probe_fails_on_purpose   1.2s\n"
            "container diskgc-e2e-42-x kept for inspection: podman logs diskgc-e2e-42-x; ...\n"
        )
        self.assertEqual(retention.kept_container(stderr), "diskgc-e2e-42-x")

    def test_none_without_the_line(self):
        self.assertIsNone(retention.kept_container("PASS sandbox_trivial\n"))
        self.assertIsNone(retention.kept_container("  container x kept for inspection:\n"))


class Usage(unittest.TestCase):
    def test_wrong_arguments_are_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(retention.main(["e2e_one_retention.py"]), 2)
        self.assertIn("Usage:", err.getvalue())


if __name__ == "__main__":
    unittest.main()
