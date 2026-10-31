"""The packaged watch units match docs/architecture.md, and the lint bites.

Part of `just check` (docs/testing.md, Deployment units, 1. Unit lint).
"""

import unittest

from unit_lint import ARCHITECTURE, SYSTEM_UNIT, USER_UNIT, lint, parse


def files():
    return USER_UNIT.read_text(), SYSTEM_UNIT.read_text(), ARCHITECTURE.read_text()


def edit(text, old, new):
    assert old in text, old
    return text.replace(old, new, 1)


class UnitLintTests(unittest.TestCase):
    def test_the_shipped_templates_pass(self):
        self.assertEqual(lint(*files()), [])

    def test_a_missing_or_changed_required_line_is_named(self):
        user, system, doc = files()
        for old, new, needle in [
            ("RestrictSUIDSGID=no\n", "", "RestrictSUIDSGID=no (missing)"),
            ("Restart=always\n", "Restart=on-failure\n", "Restart=always (Restart=on-failure)"),
            ("RestartPreventExitStatus=2\n", "", "RestartPreventExitStatus=2"),
            ("KillMode=mixed\n", "KillMode=control-group\n", "KillMode=mixed"),
        ]:
            with self.subTest(old=old):
                errors = lint(user, edit(system, old, new), doc)
                self.assertTrue(any(needle in e for e in errors), errors)

    def test_forbidden_options_are_refused_even_when_the_doc_agrees(self):
        user, system, doc = files()
        for line in [
            "PrivateTmp=yes", "PrivateUsers=yes", "ProtectProc=invisible",
            "PrivatePIDs=yes", "ProtectHome=read-only", "DynamicUser=yes",
            "RemoveIPC=yes", "Nice=10", "IOSchedulingClass=idle",
            "ProtectSystem=strict", "ExecReload=/bin/kill -HUP $MAINPID",
        ]:
            with self.subTest(line=line):
                key = line.partition("=")[0]
                spiked = edit(user, "KillMode=mixed\n", f"KillMode=mixed\n{line}\n")
                spiked_doc = edit(doc, "KillMode=mixed\n", f"KillMode=mixed\n{line}\n")
                errors = lint(spiked, system, spiked_doc)
                self.assertTrue(any(f"{key}=" in e and "forbidden" in e for e in errors), errors)

    def test_the_user_unit_may_not_touch_capabilities(self):
        user, system, doc = files()
        spiked = edit(user, "KillMode=mixed\n", "KillMode=mixed\nCapabilityBoundingSet=\n")
        errors = lint(spiked, system, doc)
        self.assertTrue(any("implies PrivateUsers" in e for e in errors), errors)
        # The system template does carry an empty bounding set.
        self.assertIn(("Service", "CapabilityBoundingSet", ""), parse(system, "system"))

    def test_drift_from_the_doc_is_caught(self):
        user, system, doc = files()
        errors = lint(edit(user, "RestartSec=30s", "RestartSec=31s"), system, doc)
        self.assertTrue(any("differs from architecture.md" in e for e in errors), errors)
        errors = lint(user, edit(system, "RequiresMountsFor=%f\n", ""), doc)
        self.assertTrue(any("plus architecture.md's additions" in e for e in errors), errors)
        errors = lint(user, edit(system, "WantedBy=multi-user.target", "WantedBy=default.target"), doc)
        self.assertTrue(any("plus architecture.md's additions" in e for e in errors), errors)

    def test_unexpected_syntax_is_an_error_not_a_pass(self):
        with self.assertRaises(ValueError):
            parse("[Service]\nKillMode=mixed\nKillMode=mixed\n", "t")
        with self.assertRaises(ValueError):
            parse("KillMode=mixed\n", "t")
        with self.assertRaises(ValueError):
            parse("[Service]\nExecStart=/a \\\n  b\n", "t")


if __name__ == "__main__":
    unittest.main()
