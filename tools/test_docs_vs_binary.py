"""The docs-vs-binary parsers must read reference.md's shapes and catch drift.

These cover the pure layer; the probes themselves are exercised by running
`just docs-vs-binary BIN` against a real build (no fake binary here: a fake
would only test the fake).
"""

import json
from pathlib import Path
import unittest

from docs_vs_binary import (
    Report, Result, Runner, UnsafeProbe, code_blocks, config_examples, dev_only_refusals,
    CATALOGS_IN_VERDICT, DESCRIPTOR, RECLAIMING_COMMANDS, build_row, documented_catalog, enum_values, host_refusal,
    json_schema, not_in_build, observed_values, parse_usage,
    probe_ci_examples, profiles_test_keys, schema_diff, section, size_grammar, status_exit_codes,
    table_rows, tier_names,
)

REFERENCE = Path(__file__).resolve().parent.parent / "docs" / "reference.md"

# The two `watch` usage keys, spelled through the tool's own list so that
# no bare reclaiming word stands here (tools/host_reclaim_scan.py).
ENSURE_KEY = RECLAIMING_COMMANDS[0]
RECLAIM_KEY = RECLAIMING_COMMANDS[1]
TALLY_KEY = f"{RECLAIM_KEY} --tally-only"


class UsageTests(unittest.TestCase):
    def test_flags_optionality_and_positionals(self):
        usage = parse_usage([
            "diskgc ensure --min-free SIZE [--target-free SIZE]",
            "              [--roots DIR]... [--dry-run] PATH",
            "diskgc profiles show NAME [--json]",
            "diskgc --help | --version",
        ])
        flags, positionals = usage["ensure"]
        self.assertIn(("--min-free", False), flags)
        self.assertIn(("--target-free", True), flags)
        self.assertIn(("--roots", True), flags)
        self.assertEqual(positionals, ("PATH",))
        self.assertEqual(usage["profiles show"][1], ("NAME",))
        self.assertEqual(usage[""][0], {("--help", False), ("--version", False)})

    def test_help_prefix_and_separate_top_level_lines_merge(self):
        usage = parse_usage(["usage: diskgc check [--config FILE]", "       diskgc --version",
                             "       diskgc --help"])
        self.assertEqual(usage[""][0], {("--help", False), ("--version", False)})
        self.assertEqual(usage["check"], (frozenset({("--config", True)}), ()))

    def test_a_dropped_flag_shows_as_a_difference(self):
        doc = parse_usage(["diskgc check [--config FILE] [--json]"])
        binary = parse_usage(["diskgc check [--config FILE]"])
        self.assertEqual(doc["check"][0] - binary["check"][0], {("--json", True)})


    def test_a_mode_flag_names_its_own_usage(self):
        usage = parse_usage([
            "diskgc watch --min-free SIZE --target-free SIZE PATH",
            "diskgc watch --tally-only [--min-free SIZE] PATH",
        ])
        self.assertEqual(sorted(usage), [RECLAIM_KEY, TALLY_KEY])
        self.assertIn(("--min-free", False), usage[RECLAIM_KEY][0])
        self.assertEqual(usage[TALLY_KEY][0], {("--min-free", True)})

    def test_the_row_comes_from_the_version_and_the_narrowed_catalog(self):
        descriptor = json.loads(DESCRIPTOR.read_text())
        narrowed = {"profiles": [{"name": "cargo-target", "releases": ["0.1"]}]}
        full = {"profiles": [{"name": "cargo-target", "releases": ["0.1"]},
                             {"name": "pip-cache", "releases": []}]}
        name, row = build_row(descriptor, "diskgc 0.2.0-dev (abc1234)\n", narrowed)
        self.assertEqual((name, row), ("0.2", descriptor["releases"]["0.2"]))
        name, _ = build_row(descriptor, "diskgc 0.1.0\n", narrowed)
        self.assertEqual(name, "0.1")
        self.assertEqual(build_row(descriptor, "diskgc 0.2.0-dev (x)\n", full),
                         ("development", descriptor["development"]))
        with self.assertRaises(ValueError):
            build_row(descriptor, "diskgc 9.9.0-dev (x)\n", narrowed)

    def test_usages_a_row_does_not_run(self):
        usages = ["", ENSURE_KEY, RECLAIM_KEY, TALLY_KEY, "check", "profiles list"]
        tally_only = {"commands": [ENSURE_KEY, TALLY_KEY, "check", "profiles"]}
        self.assertEqual(not_in_build(tally_only, usages), {RECLAIM_KEY})
        v01 = {"commands": [ENSURE_KEY, "check", "profiles"]}
        self.assertEqual(not_in_build(v01, usages), {RECLAIM_KEY, TALLY_KEY})
        every = {"commands": [ENSURE_KEY, RECLAIM_KEY, TALLY_KEY, "check", "profiles"]}
        self.assertEqual(not_in_build(every, usages), set())


class JsonTests(unittest.TestCase):
    DOC = {"status": "met | error", "plan": [{"reason": "a | b", "n": 0}], "index": {"x": 0}}

    def test_absent_and_undocumented_fields(self):
        observed = {"status": "met", "plan": [{"reason": "a", "extra": 1}], "index": None, "new": 2}
        absent, extra = schema_diff(json_schema(self.DOC), observed)
        self.assertEqual(absent, ["plan[].n"])
        self.assertEqual(extra, ["new", "plan[].extra"])

    def test_empty_lists_and_null_objects_prove_nothing(self):
        absent, extra = schema_diff(json_schema(self.DOC), {"status": "met", "plan": [], "index": None})
        self.assertEqual((absent, extra), ([], []))

    def test_enums_and_observed_values(self):
        enums = enum_values(self.DOC)
        self.assertEqual(enums, {"status": {"met", "error"}, "plan[].reason": {"a", "b"}})
        observed = {"plan": [{"reason": "a"}, {"reason": None}, {"reason": "c"}]}
        self.assertEqual(observed_values(observed, "plan[].reason"), ["a", "c"])


class DocumentTests(unittest.TestCase):
    def test_section_stops_at_same_level_and_ignores_fenced_hashes(self):
        doc = "## A\ntext\n```yaml\n# comment\n```\n### Sub\nsub\n## B\nb\n"
        self.assertIn("sub", section(doc, "## A"))
        self.assertIn("# comment", section(doc, "## A"))
        self.assertNotIn("b\n", section(doc, "## A").split("sub")[1])
        with self.assertRaises(ValueError):
            section(doc, "## Gone")

    def test_code_blocks_by_language(self):
        text = "```\nplain\n```\n```json\n{}\n```\n"
        self.assertEqual(code_blocks(text, ""), ["plain"])
        self.assertEqual(code_blocks(text, "json"), ["{}"])

    def test_status_sentence(self):
        text = ("The JSON `status` of each: 2 `usage_error` (also for a refusal), "
                "130 / 143 `interrupted`, 0 `met` (or `dry_run`), 1 `exhausted`.")
        self.assertEqual(status_exit_codes(text), {"usage_error": {2}, "interrupted": {130, 143},
                                                   "met": {0}, "dry_run": {0}, "exhausted": {1}})

    def test_tier_names_need_a_select(self):
        yaml = "groups:\n  - name: g\n    tiers:\n      - name: t1\n        select: profile:x\n"
        self.assertEqual(tier_names(yaml), ["t1"])

    def test_table_rows_skip_the_rule(self):
        rows = table_rows("| Code | Meaning |\n|---|---|\n| 2 | usage |\n\nafter\n", "| Code |")
        self.assertEqual(rows, [["2", "usage"]])


class GrammarTests(unittest.TestCase):
    def test_size_units_follow_the_sentence(self):
        doc = ("## Sizes and durations\n- `K M` and `KiB MiB` are powers of 1024; `KB` are\n"
               "  powers of 1000. Any other unit (`g`, `1.5G`).\n- Durations: integer plus `s m`.\n")
        units, invalid, durations = size_grammar(doc.replace("Any", "and so is any"))
        self.assertEqual(units, {"K": 1024, "M": 1024**2, "KiB": 1024, "MiB": 1024**2, "KB": 1000})
        self.assertEqual(invalid, ["g", "1.5G"])
        self.assertEqual(durations, ["s", "m"])

    def test_profiles_test_keys_skip_enum_spans(self):
        doc = ("### `profiles` command\n`--json` prints `config`, `status` (`passed | failed`),\n"
               "`profiles` (count). Exit 0 when all pass.\n")
        self.assertEqual(profiles_test_keys(doc), ["config", "status", "profiles"])


class ReferenceShapeTests(unittest.TestCase):
    """The parsers still find what they need in the real reference.md: a
    restructured document fails here, not silently in a release check."""

    def setUp(self):
        self.doc = REFERENCE.read_text()

    def test_catalog_tables(self):
        catalogs, sources, filters, inactive = documented_catalog(self.doc)
        self.assertIn("cargo-target", catalogs["0.1"])
        self.assertNotIn("nix-garbage", catalogs["0.1"])
        # TASK-335: 0.2's catalog is 0.1's plus nix-garbage.
        self.assertLessEqual(catalogs["0.1"], catalogs["0.2"])
        self.assertIn("nix-garbage", catalogs["0.2"])
        for members in catalogs.values():
            self.assertLessEqual(members, sources)
        self.assertNotIn("docker-dangling", set().union(*catalogs.values()))
        self.assertIn("idle-1d", filters)
        self.assertIn("tmp-entry", inactive)
        self.assertFalse(sources & inactive)

    def test_verdict_catalogs(self):
        def labels(verdict):
            return [m.group(1) for m in CATALOGS_IN_VERDICT.finditer(verdict.lower())]
        self.assertEqual(labels("Shipped; in the 0.1 and 0.2 catalogs."), ["0.1 and 0.2"])
        self.assertEqual(labels("Shipped; in the 0.2 catalog, not 0.1's (TASK-335)."), ["0.2"])
        self.assertEqual(labels("In the 0.1 and 0.2 catalogs (TASK-65)."), ["0.1 and 0.2"])
        for none in ("Shipped; not in 0.1 or 0.2 (Docker actions are outside their scope).",
                     "Shipped in the development library only; in no release catalog.",
                     "Decision pending: in no catalog",
                     "Shipped; not in the 0.2 catalog."):
            self.assertEqual(labels(none), [], none)

    def test_exit_status_mapping_covers_every_status(self):
        out = section(self.doc, "## Output")
        statuses = enum_values(__import__("json").loads(code_blocks(out, "json")[0]))["status"]
        mapping = status_exit_codes(section(self.doc, "## Exit codes (`ensure`)"))
        self.assertEqual(set(mapping), statuses)

    def test_config_examples_found(self):
        examples = config_examples(self.doc)
        self.assertGreaterEqual(len(examples), 2)
        self.assertTrue(any(dev for _, dev in examples))
        self.assertTrue(any(not dev for _, dev in examples))

    def test_dev_only_refusals_name_built_ins(self):
        self.assertIn("npm-cache", dev_only_refusals(self.doc))

    def test_size_grammar_has_every_unit(self):
        units, invalid, durations = size_grammar(self.doc)
        self.assertEqual(units["KB"], 1000)
        self.assertEqual(units["TiB"], 1024**4)
        self.assertEqual(len(units), 12)
        self.assertTrue(invalid)
        self.assertIn("w", durations)

    def test_commands_usage_block(self):
        usage = parse_usage(code_blocks(section(self.doc, "## Commands"), "")[0].splitlines())
        self.assertIn(("--min-free", False), usage["ensure"][0])
        self.assertEqual(usage["ensure"][1], ("PATH",))


class RecordingRunner(Runner):
    """A Runner whose process execution is recorded, not performed: the
    host-safety rule lives above _exec, so what reaches it is what would run."""

    def __init__(self, container=None):
        super().__init__("/opt/rc/bin/diskgc", "/scratch", container)
        self.executed = []

    def _exec(self, script):
        self.executed.append(script)
        return Result(0, '{"status": "dry_run"}', "")


HELP_WITHOUT_DRY_RUN = ("usage: diskgc ensure ...\n\nStart here:\n"
                        "  diskgc ensure --min-free 50G --select profile:cargo-target \\\n"
                        "      --roots DIR PATH\n")
HELP_WITH_DRY_RUN = HELP_WITHOUT_DRY_RUN.replace("ensure --min-free", "ensure --dry-run --min-free")
CI_EXAMPLE = ("## Commands\n```\ndiskgc ensure --min-free 50G --select profile:cargo-target \\\n"
              "  --roots \"$CI_BUILDS_DIR\" \"$CI_BUILDS_DIR\"\n```\n")


class HostSafetyTests(unittest.TestCase):
    """The release artifact deletes outside a container: on the host, only the
    tool stands between a help example and the developer's files."""

    def test_reclaiming_argv_needs_dry_run(self):
        self.assertIsNotNone(host_refusal(["ensure", "--min-free", "1G", "/x"]))
        self.assertIsNotNone(host_refusal(["--json", "watch"]))
        self.assertIsNone(host_refusal(["ensure", "--dry-run", "--min-free", "1G", "/x"]))
        self.assertIsNone(host_refusal(["profiles", "list", "--json"]))

    def test_a_help_example_without_dry_run_is_refused_on_the_host(self):
        run, report = RecordingRunner(), Report()
        report.guarded(probe_ci_examples, CI_EXAMPLE, run, report, HELP_WITHOUT_DRY_RUN)
        self.assertEqual(len(report.unsafe), 1, report.unsafe)
        self.assertIn("without --dry-run", report.unsafe[0])
        self.assertFalse(any("--dry-run" not in cmd for cmd in run.executed), run.executed)

    def test_a_help_example_with_dry_run_runs_on_the_host(self):
        run, report = RecordingRunner(), Report()
        report.guarded(probe_ci_examples, CI_EXAMPLE, run, report, HELP_WITH_DRY_RUN)
        self.assertEqual(report.unsafe, [])
        self.assertEqual(len(run.executed), 2)

    def test_a_help_example_without_dry_run_runs_in_a_container(self):
        run, report = RecordingRunner(container="walk"), Report()
        report.guarded(probe_ci_examples, CI_EXAMPLE, run, report, HELP_WITHOUT_DRY_RUN)
        self.assertEqual(report.unsafe, [])
        self.assertIn("/opt/rc/bin/diskgc ensure --min-free 50G", run.executed[-1])

    def test_a_raw_shell_step_may_not_run_the_binary_on_the_host(self):
        with self.assertRaises(UnsafeProbe):
            RecordingRunner().sh("/opt/rc/bin/diskgc ensure --min-free 1G /x")
        RecordingRunner(container="walk").sh("/opt/rc/bin/diskgc ensure --min-free 1G /x")
        RecordingRunner().sh("mkdir -p targets")

    def test_an_empty_diskgc_config_is_set_not_unset(self):
        run = RecordingRunner()
        run.diskgc("check", env={"DISKGC_CONFIG": ""})
        run.diskgc("check")
        self.assertIn("DISKGC_CONFIG=''", run.executed[0])
        self.assertIn("unset DISKGC_CONFIG;", run.executed[1])


if __name__ == "__main__":
    unittest.main()
