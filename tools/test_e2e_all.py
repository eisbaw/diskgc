import io
import subprocess
import tempfile
import unittest
from pathlib import Path

import e2e_all


def recipe(body, dependencies=(), parameters=(), shebang=False):
    return {
        "recipes": {
            "e2e": {
                "dependencies": [{"recipe": d, "arguments": [], "star": None} for d in dependencies],
                "body": [[line] for line in body],
                "parameters": list(parameters),
                "shebang": shebang,
            }
        }
    }


class StagesTest(unittest.TestCase):
    def test_dependencies_then_body_lines_in_order(self):
        dump = recipe(["nix build a", "just _runner x"], dependencies=["check", "_gcroot-dev"])
        self.assertEqual(
            e2e_all.stages(dump, "e2e"),
            ["just check", "just _gcroot-dev", "nix build a", "just _runner x"],
        )

    def test_recipes_whose_lines_do_not_stand_alone_are_refused(self):
        interpolated = recipe([])
        interpolated["recipes"]["e2e"]["body"] = [["just one ", [["variable", "NAME"]]]]
        for dump in (
            interpolated,
            recipe(["x"], parameters=[{"name": "NAME"}]),
            recipe(["x"], shebang=True),
            recipe(["@quiet"]),
            recipe(["-ignored"]),
            recipe(["continued \\"]),
            recipe([]),
        ):
            with self.subTest(dump=dump), self.assertRaises(ValueError):
                e2e_all.stages(dump, "e2e")
        with self.assertRaises(ValueError):
            e2e_all.stages(recipe(["x"]), "absent")


class RunTest(unittest.TestCase):
    def test_a_failing_stage_does_not_stop_later_stages(self):
        with tempfile.TemporaryDirectory() as d:
            results = e2e_all.run(
                ["true", "exit 3", "touch later"], d, out=io.StringIO()
            )
            self.assertTrue((Path(d) / "later").exists())
        self.assertEqual([code for _, code, _ in results], [0, 3, 0])
        self.assertEqual(e2e_all.exit_code(results), 1)
        text = e2e_all.table(results)
        self.assertIn("FAIL\t3\t", text)
        self.assertIn("# 3 stages, 1 failed, 0 not run", text)

    def test_all_passing_stages_exit_zero_and_unrun_stages_exit_130(self):
        self.assertEqual(e2e_all.exit_code([("a", 0, 1.0), ("b", 0, 2.0)]), 0)
        interrupted = [("a", 1, 1.0), ("b", e2e_all.NOT_RUN, 0.0)]
        self.assertEqual(e2e_all.exit_code(interrupted), 130)
        self.assertIn("NOT-RUN\t-\t0\tb", e2e_all.table(interrupted))

    def test_end_to_end_on_a_scratch_justfile(self):
        with tempfile.TemporaryDirectory() as d:
            Path(d, "justfile").write_text(
                "# one\nok:\n    true\n\n# all\ne2e: ok\n    false\n    echo after > after\n"
            )
            run = subprocess.run(
                ["python3", Path(e2e_all.__file__).resolve(), "e2e"],
                cwd=d,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertTrue(Path(d, "after").exists())
            [saved] = list(Path(d, "tmp", "e2e-all").glob("e2e-*.tsv"))
            rows = saved.read_text().splitlines()
            self.assertEqual([r.split("\t")[0] for r in rows[2:5]], ["PASS", "FAIL", "PASS"])
            self.assertIn(rows[-1], run.stdout)


if __name__ == "__main__":
    unittest.main()
