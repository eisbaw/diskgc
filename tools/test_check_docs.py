"""The offline document checker must catch broken contract links."""

from pathlib import Path
import unittest

from check_docs import anchors, check_document, prose_lines


class CheckDocsTests(unittest.TestCase):
    def test_anchors_handle_code_and_repeated_headings(self):
        text = "## `Cargo-target`\n## `Cargo-target`\n```md\n## Hidden\n```\n"
        self.assertEqual(anchors(text), {"cargo-target", "cargo-target-1"})
        self.assertNotIn("hidden", anchors(text))

    def test_broken_file_and_fragment_are_reported(self):
        origin = Path("/check/reference.md")
        other = Path("/check/safety.md")
        source = "# Reference\n[valid](safety.md#safety)\n[bad](safety.md#gone)\n[missing](lost.md)\n"
        docs = {other: "# Safety\n"}
        errors = check_document(origin, source, docs.get)
        self.assertEqual(len(errors), 2)
        self.assertIn("missing heading: safety.md#gone", errors[0])
        self.assertIn("missing local file: lost.md", errors[1])

    def test_images_and_badge_targets_are_checked(self):
        origin = Path("/check/README.md")
        docs = {Path("/check/docs/assets/model.svg"): "<svg/>", Path("/check/LICENSE"): "MIT"}
        source = (
            "# Readme\n"
            '<p><img src="docs/assets/banner.svg" alt="x"></p>\n'
            "![fig](docs/assets/model.svg) ![gone](docs/assets/states.svg)\n"
            "[![MIT](https://img.shields.io/x.svg)](LICENSE) [![b](https://img.shields.io/y.svg)](#readme)\n"
            "[![CI](https://img.shields.io/z.svg)](NOPE) [![a](https://img.shields.io/a.svg)](#gone)\n"
        )
        errors = check_document(origin, source, docs.get)
        self.assertEqual(len(errors), 4, errors)
        self.assertIn("README.md:2: missing local file: docs/assets/banner.svg", errors[0])
        self.assertIn("README.md:3: missing local file: docs/assets/states.svg", errors[1])
        self.assertIn("README.md:5: missing local file: NOPE", errors[2])
        self.assertIn("README.md:5: missing heading: #gone", errors[3])

    def test_code_fence_links_are_literal(self):
        source = "```md\n[not a link](gone.md)\n```\n[real](here.md)\n"
        lines = list(prose_lines(source))
        self.assertEqual(lines, [(4, "[real](here.md)")])


if __name__ == "__main__":
    unittest.main()
