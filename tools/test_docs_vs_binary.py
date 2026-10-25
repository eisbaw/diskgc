"""The docs-vs-binary parsers must read reference.md's shapes and catch drift.

These cover the pure layer; the probes themselves are exercised by running
`just docs-vs-binary BIN` against a real build (no fake binary here: a fake
would only test the fake).
"""

from pathlib import Path
import unittest

from docs_vs_binary import (
    code_blocks, config_examples, dev_only_refusals, documented_catalog, enum_values, json_schema,
    observed_values, parse_usage, profiles_test_keys, schema_diff, section, size_grammar,
    status_exit_codes, table_rows, tier_names,
)

REFERENCE = Path(__file__).resolve().parent.parent / "docs" / "reference.md"


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
        release, sources, filters, inactive = documented_catalog(self.doc)
        self.assertIn("cargo-target", release)
        self.assertLessEqual(release, sources)
        self.assertIn("idle-1d", filters)
        self.assertIn("tmp-entry", inactive)
        self.assertFalse(sources & inactive)

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


if __name__ == "__main__":
    unittest.main()
