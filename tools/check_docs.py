#!/usr/bin/env python3
"""Check local Markdown links and heading fragments in contract documents.

External URLs are outside this offline check. Rust tests separately check the
reference tables and safety examples against product behavior.
"""

import argparse
import html
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit


LINK = re.compile(r"(?<!!)\[[^\]]+\]\(([^)\s]+)\)")
HEADING = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$")
FENCE = re.compile(r"^\s*(`{3,}|~{3,})")


def prose_lines(text):
    """Yield lines outside fenced code; examples inside fences are literal."""
    opening = None
    for number, line in enumerate(text.splitlines(), 1):
        marker = FENCE.match(line)
        if marker:
            fence = marker.group(1)
            if opening is None:
                opening = fence
            elif fence[0] == opening[0] and len(fence) >= len(opening):
                opening = None
            continue
        if opening is None:
            yield number, line


def anchors(text):
    """GitHub-style heading anchors, including repeated heading suffixes."""
    counts = {}
    found = set()
    for _, line in prose_lines(text):
        heading = HEADING.match(line)
        if heading is None:
            continue
        title = html.unescape(heading.group(1))
        title = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", title)
        slug = re.sub(r"[^\w\- ]", "", title.lower()).replace(" ", "-")
        repeat = counts.get(slug, 0)
        counts[slug] = repeat + 1
        found.add(f"{slug}-{repeat}" if repeat else slug)
    return found


def check_document(path, text, read_file):
    """Validate local destinations; read_file returns text or None if absent."""
    errors = []
    cache = {path: anchors(text)}
    for number, line in prose_lines(text):
        for match in LINK.finditer(line):
            raw = html.unescape(match.group(1))
            url = urlsplit(raw)
            if url.scheme or url.netloc:
                continue
            if url.query:
                errors.append(f"{path}:{number}: local link has a query: {raw}")
                continue
            destination = (path.parent / unquote(url.path)).resolve() if url.path else path
            linked = text if destination == path else read_file(destination)
            if linked is None:
                errors.append(f"{path}:{number}: missing local file: {raw}")
                continue
            if url.fragment and destination.suffix.lower() == ".md":
                if destination not in cache:
                    cache[destination] = anchors(linked)
                fragment = unquote(url.fragment)
                if fragment not in cache[destination]:
                    errors.append(f"{path}:{number}: missing heading: {raw}")
    return errors


def read_file(path):
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except (OSError, UnicodeError):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("documents", nargs="+", type=Path)
    args = parser.parse_args()
    problems = []
    for named in args.documents:
        path = named.resolve()
        text = read_file(path)
        if text is None:
            problems.append(f"{path}: cannot read document")
        else:
            problems.extend(check_document(path, text, read_file))
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"Checked local links and headings in {len(args.documents)} documents")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
