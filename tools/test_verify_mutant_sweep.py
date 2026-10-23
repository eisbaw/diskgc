"""Behavioral checks for mutation inventory and report reconciliation."""

from contextlib import redirect_stdout
from io import StringIO
import json
from pathlib import Path
from unittest import TestCase, mock

from tools.copy_mutant_reports import verify_selection
from tools.mutation_report import validate_report
from tools.mutation_scope import SOURCES, command as scope_command
from tools.run_mutants import run_source, shard_count
from tools.verify_mutant_sweep import (
    container_image_id, identity_digest, inventory, main, plan_identity,
    procedure_snapshot, verify_partition, verify_recipe, verify_reports,
    workspace_snapshot,
)


class PartitionTests(TestCase):
    def test_complete_disjoint_partition(self):
        verify_partition(["a", "b", "c"], [["a", "b"], ["c"]])

    def test_missing_duplicate_and_oversized_shards_fail(self):
        cases = [
            (["a", "b"], [["a"]]),
            (["a", "b"], [["a", "a"], ["b"]]),
            (["a", "b"], [["a"], ["a", "b"]]),
            (["a"], [["a", "extra"]]),
            (["a"], [[]]),
            (["a"], [["a"] * 17]),
        ]
        for full, shards in cases:
            with self.subTest(full=full, shards=shards), self.assertRaises(ValueError):
                verify_partition(full, shards)


class ReportTests(TestCase):
    def setUp(self):
        self.directories = [Path(f"tmp/parse-{index}-of-2-run") for index in range(2)]
        self.shards = [["a"], ["b"]]
        self.files = {}
        for index, directory in enumerate(self.directories):
            name = self.shards[index][0]
            self.files[directory / "exit-status"] = "0\n"
            self.files[directory / "cargo-exit-status"] = "0\n"
            self.files[directory / "selected.txt"] = name + "\n"
            self.files[directory / "plan.sha256"] = "plan-1\n"
            reports = directory / "out" / "mutants.out"
            self.files[reports / "mutants.json"] = json.dumps([{"name": name}])
            self.files[reports / "outcomes.json"] = json.dumps({
                "total_mutants": 1,
                "caught": 1,
                "missed": 0,
                "timeout": 0,
                "unviable": 0,
                "outcomes": [
                    {"scenario": "Baseline", "summary": "Success"},
                    {"scenario": {"Mutant": {"name": name}}, "summary": "CaughtMutant"},
                ],
            })

    def read_text(self, path):
        return self.files[path]

    def test_complete_reports_pass(self):
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            with redirect_stdout(StringIO()) as output:
                verify_reports("parse", self.shards, self.directories, "plan-1")
        self.assertIn("validated 2 outcomes", output.getvalue())

    def test_missing_duplicate_and_incomplete_reports_fail(self):
        cases = [
            self.directories[:1],
            [self.directories[0], self.directories[0]],
        ]
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            for directories in cases:
                with self.subTest(directories=directories), self.assertRaises(ValueError):
                    verify_reports("parse", self.shards, directories, "plan-1")
        report = self.directories[1] / "out" / "mutants.out" / "outcomes.json"
        self.files[report] = self.files[report].replace('"name": "b"', '"name": "wrong"')
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            with self.assertRaisesRegex(ValueError, "do not cover every selected mutant"):
                verify_reports("parse", self.shards, self.directories, "plan-1")

    def test_false_passes_fail(self):
        report = self.directories[0] / "out" / "mutants.out" / "outcomes.json"
        original = json.loads(self.files[report])
        cases = [
            (lambda data: data["outcomes"][0].update(summary="Failed"), "baseline"),
            (lambda data: data["outcomes"][1].update(summary="MissedMutant"), "caught count"),
            (lambda data: (data["outcomes"][1].update(summary="MissedMutant"),
                           data.update(caught=0, missed=1)), "non-caught"),
            (lambda data: data.update(caught=0), "caught count"),
            (lambda data: data["outcomes"].append(data["outcomes"][1]), "outcome count"),
        ]
        for mutate, message in cases:
            data = json.loads(json.dumps(original))
            mutate(data)
            self.files[report] = json.dumps(data)
            with self.subTest(message=message), mock.patch.object(
                Path, "read_text", autospec=True, side_effect=self.read_text
            ), self.assertRaisesRegex(ValueError, message):
                verify_reports("parse", self.shards, self.directories, "plan-1")

    def test_mixed_plan_identities_fail(self):
        self.files[self.directories[1] / "plan.sha256"] = "older-plan\n"
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            with self.assertRaisesRegex(ValueError, "identity differs"):
                verify_reports("parse", self.shards, self.directories, "plan-1")

    def test_report_copy_validation_uses_same_outcome_gate(self):
        report = self.directories[0] / "out" / "mutants.out" / "outcomes.json"
        data = json.loads(self.files[report])
        data["outcomes"][0]["summary"] = "Failed"
        self.files[report] = json.dumps(data)
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            with self.assertRaisesRegex(ValueError, "baseline"):
                verify_selection(self.directories[0] / "out", self.directories[0] / "selected.txt")

    def test_valid_survivor_report_is_preserved_but_sweep_fails(self):
        report = self.directories[0] / "out" / "mutants.out" / "outcomes.json"
        data = json.loads(self.files[report])
        data["outcomes"][1]["summary"] = "MissedMutant"
        data.update(caught=0, missed=1)
        self.files[report] = json.dumps(data)
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=self.read_text):
            verify_selection(self.directories[0] / "out", self.directories[0] / "selected.txt")
            with self.assertRaisesRegex(ValueError, "MissedMutant: a"):
                verify_reports("parse", self.shards, self.directories, "plan-1")


class ScopeTests(TestCase):
    def test_justfile_invokes_same_scope_as_verifier(self):
        justfile = Path("justfile").read_text()
        recipe = justfile.split("\nmutants:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("python3 tools/mutation_scope.py run", recipe)
        command = scope_command(list_only=True)
        self.assertEqual(command[:4], ["cargo", "mutants", "--list", "--gitignore"])
        self.assertEqual(command.count("--file"), 5)

    def test_host_mutation_build_command_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "host mutation builds are unsupported"):
            scope_command(list_only=False)

    def test_bounded_sweep_chooses_complete_nonempty_partition(self):
        def names(_source_file, shard=None):
            return {None: ["a", "b", "c"], "0/1": ["a", "b", "c"],
                    "0/2": ["a", "c"], "1/2": ["b"]}[shard]

        with mock.patch("tools.run_mutants.mutant_names", side_effect=names), \
             mock.patch("tools.run_mutants.MAX_SHARD_MUTANTS", 2):
            self.assertEqual(shard_count("parse"), 2)

    def test_bounded_sweep_verifies_each_shard_report(self):
        root = Path("tmp/mutants-container-smoke").resolve()
        reports = [str(root / f"parse-{index}-of-2-123") for index in range(2)]
        commands = []

        def run(args):
            commands.append(args)
            if "--run-shard" in args:
                index = int(args[-1].split("/", 1)[0])
                return f"Mutation exit=0; logs={reports[index]}\n"
            return ""

        with mock.patch("tools.run_mutants.shard_count", return_value=2), \
             mock.patch("tools.run_mutants.checked_command", side_effect=run):
            run_source("parse")
        self.assertEqual([command[-1] for command in commands[1:3]], ["0/2", "1/2"])
        self.assertEqual(commands[-1][-2:], reports)
        self.assertEqual(commands[-1][2:5], ["verify", "parse", "2"])

    def test_appended_justfile_command_is_rejected(self):
        justfile = Path("justfile").read_text()
        changed = justfile.replace(
            "    python3 tools/mutation_scope.py run\n",
            "    python3 tools/mutation_scope.py run\n    cargo mutants --list\n",
        )
        self.assertNotEqual(changed, justfile)
        with mock.patch.object(Path, "read_text", return_value=changed):
            with self.assertRaisesRegex(ValueError, "recipe differs"):
                verify_recipe()

    def test_workspace_snapshot_changes_when_test_source_changes(self):
        listing = mock.Mock(returncode=0, stdout=b"Cargo.toml\0crates/diskgc-core/src/lib.rs\0")
        with mock.patch("tools.verify_mutant_sweep.subprocess.run", return_value=listing), \
             mock.patch.object(Path, "is_file", return_value=True), \
             mock.patch.object(Path, "read_bytes", return_value=b"before"):
            before = workspace_snapshot()
        with mock.patch("tools.verify_mutant_sweep.subprocess.run", return_value=listing), \
             mock.patch.object(Path, "is_file", return_value=True), \
             mock.patch.object(Path, "read_bytes", return_value=b"after"):
            after = workspace_snapshot()
        self.assertNotEqual(before, after)

    def test_workspace_snapshot_includes_external_test_inputs(self):
        names = ("docs/reference.md", "docs/safety.md", "diskgc-e2e/Cargo.toml", "justfile", "PRD.md")
        for name in names:
            listing = mock.Mock(returncode=0, stdout=f"Cargo.toml\0{name}\0".encode())
            with self.subTest(name=name), \
                 mock.patch("tools.verify_mutant_sweep.subprocess.run", return_value=listing) as run, \
                 mock.patch.object(Path, "is_file", return_value=True), \
                 mock.patch.object(Path, "read_bytes", return_value=b"before"):
                before = workspace_snapshot()
                self.assertIn(name.split("/")[0], run.call_args.args[0])
                with mock.patch.object(Path, "read_bytes", return_value=b"after"):
                    self.assertNotEqual(before, workspace_snapshot())

    def test_plan_identity_changes_with_tool_version_and_runner(self):
        with mock.patch("tools.verify_mutant_sweep.workspace_snapshot", return_value="source"), \
             mock.patch("tools.verify_mutant_sweep.procedure_snapshot", return_value="runner"), \
             mock.patch("tools.verify_mutant_sweep.container_image_id", return_value="a" * 64), \
             mock.patch("tools.verify_mutant_sweep.cargo_mutants_version", return_value="1"):
            before = identity_digest(plan_identity("parse", 16)[1])
        with mock.patch("tools.verify_mutant_sweep.workspace_snapshot", return_value="source"), \
             mock.patch("tools.verify_mutant_sweep.procedure_snapshot", return_value="runner"), \
             mock.patch("tools.verify_mutant_sweep.container_image_id", return_value="a" * 64), \
             mock.patch("tools.verify_mutant_sweep.cargo_mutants_version", return_value="2"):
            self.assertNotEqual(before, identity_digest(plan_identity("parse", 16)[1]))
        with mock.patch("tools.verify_mutant_sweep.workspace_snapshot", return_value="source"), \
             mock.patch("tools.verify_mutant_sweep.procedure_snapshot", return_value="changed"), \
             mock.patch("tools.verify_mutant_sweep.container_image_id", return_value="a" * 64), \
             mock.patch("tools.verify_mutant_sweep.cargo_mutants_version", return_value="1"):
            self.assertNotEqual(before, identity_digest(plan_identity("parse", 16)[1]))
        with mock.patch("tools.verify_mutant_sweep.workspace_snapshot", return_value="source"), \
             mock.patch("tools.verify_mutant_sweep.procedure_snapshot", return_value="runner"), \
             mock.patch("tools.verify_mutant_sweep.container_image_id", return_value="b" * 64), \
             mock.patch("tools.verify_mutant_sweep.cargo_mutants_version", return_value="1"):
            self.assertNotEqual(before, identity_digest(plan_identity("parse", 16)[1]))

    def test_cached_image_id_is_validated_and_normalized(self):
        result = mock.Mock(returncode=0, stdout="sha256:" + "a" * 64 + "\n", stderr="")
        with mock.patch("tools.verify_mutant_sweep.subprocess.run", return_value=result):
            self.assertEqual(container_image_id(), "a" * 64)
        result.stdout = "tag-name\n"
        with mock.patch("tools.verify_mutant_sweep.subprocess.run", return_value=result):
            with self.assertRaisesRegex(ValueError, "cannot identify cached mutation image"):
                container_image_id()

    def test_runner_uses_resolved_image_id_for_both_containers(self):
        runner = Path("tools/mutants_container_smoke.sh").read_text()
        self.assertIn('preflight_name" "$image_id" sh -ec', runner)
        self.assertIn('container_name" "$image_id" \\', runner)
        self.assertIn('check "$2" "$total" "$index" "$image_id"', runner)
        self.assertIn('identity "$2" "$total" "$image_id"', runner)

    def test_pinned_image_a_cannot_use_plan_for_retargeted_tag_b(self):
        image_a, image_b = "a" * 64, "b" * 64
        with mock.patch("tools.verify_mutant_sweep.workspace_snapshot", return_value="source"), \
             mock.patch("tools.verify_mutant_sweep.procedure_snapshot", return_value="runner"), \
             mock.patch("tools.verify_mutant_sweep.container_image_id", return_value=image_b), \
             mock.patch("tools.verify_mutant_sweep.cargo_mutants_version", return_value="1"):
            b_path, b_identity = plan_identity("parse", 16)
            a_path, a_identity = plan_identity("parse", 16, image_a)
            self.assertEqual(a_identity["container_image_id"], image_a)
            self.assertNotEqual(a_path, b_path)
            self.assertNotEqual(identity_digest(a_identity), identity_digest(b_identity))
            with mock.patch.object(Path, "exists", autospec=True,
                                   side_effect=lambda path: path == b_path):
                with self.assertRaisesRegex(ValueError, "missing mutation plan"):
                    inventory("parse", 16, create=False, pinned_image_id=image_a)

    def test_check_and_identity_require_pinned_image_id(self):
        for argv in (("check", "parse", "16", "0"), ("identity", "parse", "16")):
            with self.subTest(argv=argv), mock.patch("sys.argv", ["verify_mutant_sweep.py", *argv]):
                self.assertEqual(main(), 2)

    def test_procedure_snapshot_includes_runner_command(self):
        def content(path):
            return b"changed" if str(path) == "tools/mutants_container_smoke.sh" else b"same"
        with mock.patch.object(Path, "read_bytes", autospec=True, side_effect=lambda path: b"same"):
            before = procedure_snapshot()
        with mock.patch.object(Path, "read_bytes", autospec=True, side_effect=content):
            self.assertNotEqual(before, procedure_snapshot())
