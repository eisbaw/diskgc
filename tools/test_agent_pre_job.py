"""docs/agent-pre-job.py decides per documented diskgc exit code (TASK-150).

Runs the script as written against a stand-in `diskgc` on PATH that prints
a canned report and exits with a canned code, so every documented exit code
is covered without a reclaim. The real binary's reports are exercised by the
container journey (diskgc-e2e agent_journey.rs) and the TASK-150 walk.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parent.parent / "docs" / "agent-pre-job.py"

SPACE = {"min_free_bytes": 100, "target_free_bytes": 100}
LISTS = {"actions": [], "skipped": [], "errors": []}


def report(status, **fields):
    return {"status": status, **LISTS, **fields}


def error(cause):
    return [{"tier": "", "profile": "", "candidate": "", "cause": cause, "message": "m"}]


class AgentPreJobTest(unittest.TestCase):
    def run_script(self, code, body):
        with tempfile.TemporaryDirectory() as bin_dir:
            fake = Path(bin_dir) / "diskgc"
            fake.write_text(textwrap.dedent(f"""\
                #!{sys.executable}
                import sys
                sys.stdout.write({body!r})
                sys.exit({code})
                """))
            fake.chmod(0o755)
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ.get('PATH', '')}")
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "--min-free", "1G", "/w"],
                env=env, stdout=subprocess.PIPE, text=True, check=False)
        [line] = run.stdout.splitlines()
        return run.returncode, json.loads(line)

    def decision(self, code, body):
        exit_code, out = self.run_script(code, json.dumps(body))
        return exit_code, out["decision"]

    def test_every_documented_exit_code_has_its_own_decision(self):
        cases = [
            (0, report("met", avail_after_bytes=200, target_met=True, **SPACE), "start"),
            (1, report("exhausted", avail_after_bytes=50, target_met=False, **SPACE),
             "defer_no_space"),
            (2, report("usage_error", errors=error("provisioning")), "abort_provisioning"),
            (2, report("usage_error", errors=error("parse")), "abort_config"),
            (2, report("usage_error", errors=error("config")), "abort_config"),
            (3, report("error", avail_after_bytes=None, target_met=None,
                       errors=error("host"), **SPACE), "abort_error"),
            (75, report("lock_timeout", avail_after_bytes=50, target_met=False, **SPACE),
             "retry_later"),
            (124, report("deadline", avail_after_bytes=50, target_met=False, **SPACE),
             "retry_later"),
            (130, report("interrupted"), "abort_interrupted"),
            (143, report("interrupted"), "abort_interrupted"),
        ]
        for code, body, decision in cases:
            with self.subTest(code=code, decision=decision):
                self.assertEqual(self.decision(code, body), (code, decision))

    def test_a_report_that_contradicts_its_exit_code_is_invalid(self):
        cases = [
            (2, report("met")),                                   # status mismatch
            (2, report("usage_error")),                           # no errors
            (2, report("usage_error", errors=[{"message": "m"}])),  # no cause
            (124, report("deadline", avail_after_bytes=200, target_met=True, **SPACE)),
            (0, report("met", avail_after_bytes=50, target_met=False, **SPACE)),
            (75, report("lock_timeout", avail_after_bytes=50, target_met=False,
                        actions=[{}], **SPACE)),
            (101, report("error")),                               # a panic
        ]
        for code, body in cases:
            with self.subTest(code=code, body=body):
                self.assertEqual(self.decision(code, body), (4, "abort_invalid_report"))
        self.assertEqual(self.run_script(2, "")[1]["decision"], "abort_invalid_report")

    def test_the_documented_example_keeps_an_idle_filter(self):
        doc = SCRIPT.read_text()
        example = doc[doc.index("Example:"):doc.index("\n\n", doc.index("Example:"))]
        self.assertIn("filter:idle-", example)


if __name__ == "__main__":
    unittest.main()
