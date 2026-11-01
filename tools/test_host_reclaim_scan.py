"""No host test source writes a reclaiming diskgc argv, and the scan bites.

Part of `just check` (docs/testing.md, Test isolation; TASK-282).
"""

import unittest

from host_reclaim_scan import ALLOWED, ROOT, rust_tokens, scan, scan_text, scanned_files

GATE_TEST = "a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate"
CLI = "crates/diskgc/tests/cli.rs"


def rust(body, name="t"):
    return f"#[test]\nfn {name}() {{\n{body}\n}}\n"


class RepositoryTests(unittest.TestCase):
    def test_no_host_test_source_reclaims(self):
        self.assertEqual(scan(), [])

    def test_the_scope_holds_the_binary_tests_and_the_python_tests(self):
        scanned = {p.relative_to(ROOT).as_posix() for p in scanned_files()}
        for path in (CLI, "tools/test_agent_pre_job.py", "tools/test_docs_vs_binary.py",
                     "crates/diskgc-model/tests/watch.rs", "fuzz/tests/seeds.rs"):
            self.assertIn(path, scanned)
        self.assertNotIn("crates/diskgc/src/main.rs", scanned)

    def test_a_reclaiming_ensure_added_to_the_real_cli_tests_is_found(self):
        text = (ROOT / CLI).read_text()
        spiked = text + rust('diskgc(&["ensure", "--min-free", "1G", "/x"]);', "spiked")
        messages = scan_text(CLI, spiked, {(CLI, GATE_TEST): (1, "named")})
        self.assertEqual(len(messages), 1, messages)
        self.assertIn("in spiked: `ensure` argv", messages[0])

    def test_a_second_reclaiming_argv_in_the_gate_test_is_found(self):
        text = (ROOT / CLI).read_text()
        anchor = f"fn {GATE_TEST}() {{\n"
        self.assertIn(anchor, text)
        spiked = text.replace(anchor, anchor + '    diskgc(&["watch", "/x"]);\n')
        messages = scan_text(CLI, spiked, {(CLI, GATE_TEST): (1, "named")})
        self.assertEqual(len(messages), 2, messages)
        self.assertTrue(all("allowlisted for 1, found 2" in m for m in messages), messages)


class RustShapeTests(unittest.TestCase):
    def found(self, body, allowed=None):
        return scan_text("x.rs", rust(body), allowed or {})

    def test_reclaiming_argv_is_found(self):
        for body in [
            'diskgc(&["ensure", "--min-free", "1G", "/x"]);',
            'diskgc(&["--json", "watch", "--min-free", "1G", "--target-free", "2G", "/x"]);',
            'let a = vec!["ensure"];',
            'Command::new(bin).arg("ensure").arg("--dry-run");',  # pieces: conservative
            'diskgc(&[&["ensure"][..], &["--dry-run"]].concat());',  # nested: own group only
            'let c = "watch";',
            'for c in ["watch"] { run(c); }',
            'let a = [\'"\', \'x\']; diskgc(&["ensure", "/x"]);',  # a quote char literal
            'let s = r#"a "quoted" ["--dry-run"]"#; diskgc(&["ensure", "/x"]);',
            'diskgc(&["ensure", "--tally-only", "/x"]);',  # the other command's flag
            'diskgc(&["watch", "--dry-run", "/x"]);',
        ]:
            with self.subTest(body=body):
                self.assertEqual(len(self.found(body)), 1, body)

    def test_dry_runs_tally_only_and_lookups_are_not(self):
        for body in [
            'diskgc(&["ensure", "--dry-run", "--min-free", "1G", "/x"]);',
            'diskgc(&["watch", "--tally-only", "/x"]);',
            'let common = ["ensure", "--dry-run", "--json"];',
            'assert_eq!(report["watch"]["cycle"], 1);',
            'assert_eq!(run.records[0].1["watch"], Value::Null);',
            'let x = usage()["ensure"];',
            '// diskgc(&["ensure", "/x"]);',
            '/* diskgc(&["ensure", /* nested */ "/x"]); */',
            'let s = "diskgc ensure --min-free 1G /x";',
            "fn f<'a>(x: &'a str) -> &'a str { x }",
        ]:
            with self.subTest(body=body):
                self.assertEqual(self.found(body), [], body)

    def test_the_allowlist_pins_the_function_and_the_count(self):
        body = 'diskgc(&["ensure", "/x"]);'
        self.assertEqual(self.found(body, {("x.rs", "t"): (1, "why")}), [])
        self.assertEqual(len(self.found(body, {("x.rs", "other"): (1, "why")})), 2)
        self.assertEqual(len(self.found(body, {("x.rs", "t"): (2, "why")})), 1)

    def test_a_stale_allowlist_entry_is_found(self):
        messages = self.found("let x = 1;", {("x.rs", "t"): (1, "why")})
        self.assertEqual(len(messages), 1)
        self.assertIn("holds no reclaiming argv", messages[0])

    def test_an_allowlist_entry_for_a_file_not_scanned_is_found(self):
        messages = scan(allowed={**ALLOWED, ("crates/diskgc/src/main.rs", "main"): (1, "why")})
        self.assertEqual(len(messages), 1, messages)
        self.assertIn("not a scanned file", messages[0])

    def test_source_it_cannot_read_fails_closed(self):
        for text in ['fn t() { let s = "open', "fn t() { /* open", 'fn t() { f(["ensure" }']:
            with self.subTest(text=text):
                messages = scan_text("x.rs", text, {})
                self.assertEqual(len(messages), 1, messages)
                self.assertIn("cannot scan with confidence", messages[0])

    def test_line_numbers_count_multiline_strings_and_comments(self):
        text = 'fn t() {\n/* a\nb */ let s = "x\ny";\ndiskgc(&["ensure"]);\n}\n'
        lines = {value: line for kind, value, line in rust_tokens(text) if kind == "str"}
        self.assertEqual(lines["ensure"], 5)


class PythonShapeTests(unittest.TestCase):
    def found(self, body):
        return scan_text("tools/test_x.py", f"def test_x():\n    {body}\n", {})

    def test_reclaiming_argv_is_found(self):
        for body in [
            'subprocess.run(["diskgc", "ensure", "--min-free", "1G", "/x"])',
            "subprocess.run(['diskgc', 'watch', '/x'])",
            'subprocess.run(("diskgc", "ensure"))',
        ]:
            with self.subTest(body=body):
                self.assertEqual(len(self.found(body)), 1, body)

    def test_dry_runs_and_lookups_are_not(self):
        for body in [
            'subprocess.run(["diskgc", "ensure", "--dry-run", "/x"])',
            'subprocess.run(["diskgc", "watch", "--tally-only", "/x"])',
            'flags = usage["ensure"]',
            '# subprocess.run(["diskgc", "ensure"])',
            '"""diskgc ensure --min-free 1G"""',
        ]:
            with self.subTest(body=body):
                self.assertEqual(self.found(body), [], body)

    def test_source_it_cannot_read_fails_closed(self):
        messages = scan_text("tools/test_x.py", 'def t():\n    f(["ensure"\n', {})
        self.assertEqual(len(messages), 1, messages)
        self.assertIn("cannot scan with confidence", messages[0])


if __name__ == "__main__":
    unittest.main()
