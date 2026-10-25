#!/usr/bin/env python3
"""Run what docs/reference.md documents against a diskgc binary; report drift.

phase3.1 skill step 5b: before a release, every documented command, flag,
default, exit code, JSON field and built-in name is tried on the release
artifact, because a mismatch is the first thing a new user hits.

The binary runs only read-only commands and dry runs on the host, in a
scratch directory holding fake Cargo targets; dry runs of the fixed-root
/tmp built-ins read the host's /tmp but delete nothing. Reclaiming probes
(exit 1, 75, 130, 143; the `actions` and `skipped` field names) run only
with --container NAME, inside a disposable walk container (`just walk-up
NAME`), whose diskgc is the binary installed there as BIN.

The doc parsers are pure and unit-tested (test_docs_vs_binary.py). What the
tool cannot check is printed as NOT CHECKED, so a green run is not read as
more than it is. Exit 0: no mismatch; 1: mismatches; 2: usage or a broken
probe environment.
"""

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys

# A probe that runs longer than this is a hang, not a slow host: the longest
# documented run here is a dry run with one process scan (~0.5-2 s observed
# on a ~600-process laptop, static musl build up to ~6x slower).
PROBE_TIMEOUT_SECONDS = 120
CARGO_TAG = "Signature: 8a477f597d28d172789f06886806bc55\n"


# ---- Pure document parsing -------------------------------------------------


def section(doc, heading):
    """Text of the section whose heading line is `heading`, to the next heading
    of the same or a higher level. Raises if the heading is missing, so a
    renamed section fails loudly instead of checking nothing."""
    lines = doc.splitlines()
    level = len(heading) - len(heading.lstrip("#"))
    for start, line in enumerate(lines):
        if line.strip() == heading:
            break
    else:
        raise ValueError(f"reference.md has no heading {heading!r}")
    out = []
    in_fence = False
    for line in lines[start + 1:]:
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
        if not in_fence and line.startswith("#"):
            this = len(line) - len(line.lstrip("#"))
            if this <= level and line[this:this + 1] == " ":
                break
        out.append(line)
    return "\n".join(out)


def code_blocks(text, lang):
    """Fenced blocks of language `lang` ('' for a bare fence), in order."""
    blocks, current, opening = [], None, None
    for line in text.splitlines():
        stripped = line.strip()
        if current is None and stripped.startswith("```"):
            opening = stripped[3:].strip()
            current = []
        elif current is not None and stripped == "```":
            if opening == lang:
                blocks.append("\n".join(current))
            current = None
        elif current is not None:
            current.append(line)
    return blocks


def table_rows(text, header_prefix):
    """Cells of the Markdown table whose header row starts with header_prefix."""
    rows, inside = [], False
    for line in text.splitlines():
        if line.startswith(header_prefix):
            inside = True
            continue
        if inside:
            if not line.startswith("|"):
                if rows:
                    break
                continue
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if set("".join(cells)) <= set("-: "):
                continue
            rows.append(cells)
    if not inside:
        raise ValueError(f"no table with header {header_prefix!r}")
    return rows


def backticked(cell):
    return re.findall(r"`([^`]+)`", cell)


def parse_usage(lines):
    """{command: (frozenset((flag, optional)), positionals)} from usage lines,
    each `diskgc COMMAND ...` with indented continuations. `diskgc --help |
    --version` and separate `diskgc --help` lines both land on command ''."""
    commands = {}
    entries = []
    for raw in lines:
        line = raw.strip()
        if line.startswith("usage:"):
            line = line[len("usage:"):].strip()
        if line.startswith("diskgc"):
            entries.append(line[len("diskgc"):])
        elif line and entries:
            entries[-1] += " " + line
    for entry in entries:
        tokens = entry.split()
        words = []
        while tokens and re.fullmatch(r"[a-z]+", tokens[0]):
            words.append(tokens.pop(0))
        name = " ".join(words)
        flags, positionals = set(), []
        depth = 0
        takes_value = False
        for token in tokens:
            optional = depth > 0 or token.startswith("[")
            depth += token.count("[") - token.count("]")
            bare = token.strip("[]|.")
            if bare.startswith("--"):
                flags.add((bare, optional))
                # `--flag VALUE` or `[--flag VALUE]`; `[--flag]` takes none.
                takes_value = "]" not in token
                continue
            if re.fullmatch(r"[A-Z]+", bare) and not optional and not takes_value:
                positionals.append(bare)
            takes_value = False
        old_flags, old_pos = commands.get(name, (frozenset(), ()))
        commands[name] = (old_flags | frozenset(flags), old_pos + tuple(positionals))
    return commands


def help_usage_lines(help_text):
    out = []
    for line in help_text.splitlines():
        if not line.strip():
            break
        out.append(line)
    return out


def json_schema(value):
    """Key structure of a documented JSON example: dicts keep their keys,
    lists keep their first element's structure, leaves become None."""
    if isinstance(value, dict):
        return {k: json_schema(v) for k, v in value.items()}
    if isinstance(value, list):
        return [json_schema(value[0])] if value else []
    return None


def schema_diff(documented, observed, path=""):
    """(documented but absent, present but undocumented) key paths. Only keys
    the observation reaches are compared: an empty observed list or a null
    observed object says nothing about its element's fields."""
    absent, extra = [], []
    if isinstance(documented, dict) and isinstance(observed, dict):
        for key in documented:
            if key not in observed:
                absent.append(f"{path}.{key}".lstrip("."))
            else:
                a, e = schema_diff(documented[key], observed[key], f"{path}.{key}")
                absent += a
                extra += e
        extra += [f"{path}.{k}".lstrip(".") for k in observed if k not in documented]
    elif isinstance(documented, list) and isinstance(observed, list):
        if documented and observed:
            for item in observed:
                a, e = schema_diff(documented[0], item, f"{path}[]")
                absent += a
                extra += e
    return sorted(set(absent)), sorted(set(extra))


def enum_values(documented_json):
    """{key path: set of values} for documented string fields `a | b | c`."""
    found = {}

    def walk(value, path):
        if isinstance(value, dict):
            for k, v in value.items():
                walk(v, f"{path}.{k}" if path else k)
        elif isinstance(value, list):
            for item in value:
                walk(item, f"{path}[]")
        elif isinstance(value, str) and " | " in value:
            found[path] = set(value.split(" | "))

    walk(documented_json, "")
    return found


def observed_values(value, path):
    """Every value at a key path like `skipped[].reason` in observed JSON."""
    parts = re.findall(r"[^.\[\]]+|\[\]", path)
    current = [value]
    for part in parts:
        nxt = []
        for item in current:
            if part == "[]" and isinstance(item, list):
                nxt.extend(item)
            elif isinstance(item, dict) and part in item:
                nxt.append(item[part])
        current = nxt
    return [v for v in current if v is not None]


def status_exit_codes(exit_section):
    """{status: exit code} from the sentence `The JSON `status` of each: 2
    `usage_error` ..., 130 / 143 `interrupted`, ...`."""
    flat = " ".join(exit_section.split())
    start = flat.find("The JSON `status` of each:")
    if start < 0:
        raise ValueError("exit-code section lost its `status` sentence")
    mapping = {}
    for codes, status in re.findall(r"(\d+(?: / \d+)?) `([a-z_]+)`", flat[start:]):
        mapping[status] = {int(c) for c in codes.split(" / ")}
    for status in re.findall(r"\(or `([a-z_]+)`\)", flat[start:]):
        mapping.setdefault(status, mapping.get("met", set()))
    return mapping


def documented_catalog(reference):
    """(0.1 source names, every library source name, filter names, planned or
    withdrawn names) from the built-in library and liveness tables."""
    library = section(reference, "### Built-in profile library")
    rows = table_rows(library, "| Category | Profile |")
    sources, filters, inactive = set(), set(), set()
    for category, profiles, finds, action in rows:
        names = set(backticked(profiles))
        if category == "filters":
            filters |= names
        elif (finds.startswith(("planned", "withdrawn")) or action.startswith(("no ", "none"))
              or category == "withdrawn"):
            inactive |= names
        else:
            sources |= names
    liveness = section(reference, "### Liveness contracts")
    release = set()
    for cells in table_rows(liveness, "| Profile |"):
        verdict = cells[-1].lower()
        if "in the proposed 0.1 catalog" in verdict and "not in 0.1" not in verdict:
            release |= set(backticked(cells[0]))
    return release, sources, filters, inactive


def size_grammar(reference):
    """({unit: bytes}, [invalid examples], [duration units]) from Sizes and
    durations: `K M G T` and `KiB ...` are powers of 1024, `KB ...` of 1000."""
    flat = " ".join(section(reference, "## Sizes and durations").split())
    units = {}
    for group, base in _power_groups(flat):
        for exponent, unit in enumerate(group.split(), 1):
            units[unit] = int(base) ** exponent
    invalid = backticked(re.search(r"any other unit \(([^)]*)\)", flat).group(1))
    durations = re.search(r"Durations: integer plus `([a-z ]+)`", flat).group(1).split()
    return units, invalid, durations


def _power_groups(flat):
    """[(unit run, base)]: each run of backticked units before `are powers of N`
    belongs to that N; the run's position in the run list is its exponent."""
    groups = []
    for spans, base in re.findall(r"((?:`[A-Za-z ]+`(?: and )?)+) are powers of (\d+)", flat):
        for span in backticked(spans):
            groups.append((span, base))
    return groups


def profiles_test_keys(reference):
    """The keys `profiles test --json` prints, from its bullet."""
    flat = " ".join(section(reference, "### `profiles` command").split())
    span = re.search(r"`--json` prints (.*?)\. Exit 0 when", flat).group(1)
    return [k for k in backticked(span) if re.fullmatch(r"[a-z_]+", k)]


def config_examples(reference):
    """[(yaml text, development_only)] for whole configurations in the
    Configuration file section."""
    out = []
    for block in code_blocks(section(reference, "## Configuration file"), "yaml"):
        if re.search(r"^version: 1$", block, re.M):
            out.append((block, "Development build only" in block.splitlines()[0]))
    return out


def dev_only_refusals(reference):
    """Built-in names the `Development build only` paragraph says 0.1's
    refusal names (the hyphenated backticked names: `npm-cache`, ...)."""
    flat = " ".join(section(reference, "## Configuration file").split())
    paragraph = re.search(r"\*\*Development build only\.\*\*(.*?)```", flat).group(1)
    return sorted({n for n in backticked(paragraph) if re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)+", n)})


def tier_names(yaml_text):
    """Tier names: `- name: X` entries followed by a `select:` of their own."""
    lines = yaml_text.splitlines()
    names = []
    for i, line in enumerate(lines):
        m = re.match(r"^(\s*)- name: ([a-z0-9][a-z0-9_-]*)", line)
        if m and i + 1 < len(lines) and lines[i + 1].strip().startswith("select:"):
            names.append(m.group(2))
    return names


# ---- Running the binary ----------------------------------------------------


@dataclass
class Result:
    code: int
    out: str
    err: str

    def json(self):
        try:
            return json.loads(self.out)
        except json.JSONDecodeError:
            return None


class Runner:
    """Runs diskgc and shell fixture steps, on the host or in a walk container.
    The prefix is the only difference; everything else is identical."""

    def __init__(self, binary, scratch, container=None):
        self.binary = binary
        self.scratch = scratch
        self.container = container
        self.prefix = (["podman", "exec", "-i", "-u", "tester", "-e", "HOME=/home/tester",
                        f"walk-diskgc-{container}"] if container else [])

    def sh(self, script):
        # Braces: a trailing `&` inside the script must not background the cd.
        argv = self.prefix + ["sh", "-c", f"cd {shlex.quote(self.scratch)} || exit 2\n{{ {script}\n}}"]
        # Own process group, so a hung probe's diskgc dies with its shell.
        child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, start_new_session=True)
        try:
            out, err = child.communicate(timeout=PROBE_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired as error:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
            raise RuntimeError(f"probe hung past {PROBE_TIMEOUT_SECONDS} s: {script}") from error
        return Result(child.returncode, out, err)

    def diskgc(self, *args, env=None, redirect=""):
        """XDG_CONFIG_HOME and DISKGC_CONFIG are pinned so no developer config
        leaks in; callers override them to probe the lookup."""
        base = {"XDG_CONFIG_HOME": f"{self.scratch}/xdg-empty", "DISKGC_CONFIG": ""}
        base.update(env or {})
        unset = "unset DISKGC_CONFIG;" if base.get("DISKGC_CONFIG") == "" else ""
        if base.get("DISKGC_CONFIG") == "":
            del base["DISKGC_CONFIG"]
        argv = " ".join(shlex.quote(a) for a in (self.binary, *args))
        return self.sh(f"{unset} env {' '.join(f'{k}={shlex.quote(v)}' for k, v in base.items())} "
                       f"{argv} {redirect}")


# ---- Probes ----------------------------------------------------------------


class Report:
    def __init__(self):
        self.mismatches = []
        self.checked = 0

    def expect(self, ok, area, what, docs, binary):
        self.checked += 1
        line = f"{area}: {what}: docs say {docs}; binary says {binary}"
        if not ok and line not in self.mismatches:
            self.mismatches.append(line)
        return ok


def make_fixtures(run):
    """Fake Cargo targets: one eligible, one with a keep marker, an empty
    roots dir, and config directories."""
    s = run.scratch
    script = f"""
set -e
rm -rf targets empty xdg-empty xdg cfg
mkdir -p targets/live/target/debug/deps targets/kept/target/debug empty xdg-empty xdg/diskgc cfg
for t in targets/live/target targets/kept/target; do
  printf '{CARGO_TAG}' > $t/CACHEDIR.TAG
  touch $t/debug/.cargo-lock
  # A project target: a detached one is in use while any Cargo runs.
  printf '[package]\\nname = "fixture"\\n' > $t/../Cargo.toml
done
echo x > targets/live/target/debug/deps/a
touch targets/kept/target/.diskgc-keep
printf 'version: 1\\ngroups:\\n  - name: g\\n    tiers:\\n      - name: t\\n        select: profile:cargo-target\\n        roots: [{s}/targets]\\n' > xdg/diskgc/config.yaml
"""
    result = run.sh(script)
    if result.code != 0:
        raise RuntimeError(f"fixture setup failed: {result.err}")


def probe_usage(reference, run, report):
    documented = parse_usage(code_blocks(section(reference, "## Commands"), "")[0].splitlines())
    help_text = run.diskgc("--help")
    report.expect(help_text.code == 0, "help", "`diskgc --help` exit", 0, help_text.code)
    actual = parse_usage(help_usage_lines(help_text.out))
    for name in sorted(set(documented) | set(actual)):
        doc_flags, doc_pos = documented.get(name, (frozenset(), ()))
        bin_flags, bin_pos = actual.get(name, (frozenset(), ()))
        label = f"`diskgc {name}`".replace(" `", "`")
        report.expect(name in actual, "usage", f"{label} in --help usage", "present", "absent")
        report.expect(name in documented, "usage", f"{label} in reference Commands", "absent", "present")
        if name in actual and name in documented:
            for flag, optional in sorted(doc_flags - bin_flags):
                report.expect(False, "usage", f"{label} {flag}",
                              "optional" if optional else "required", "missing or other optionality")
            for flag, optional in sorted(bin_flags - doc_flags):
                report.expect(False, "usage", f"{label} {flag}",
                              "undocumented", "optional" if optional else "required")
            report.expect(doc_pos == bin_pos, "usage", f"{label} positionals", doc_pos, bin_pos)
    return help_text.out


def probe_errors(reference, run, report):
    out_doc = json.loads(code_blocks(section(reference, "## Output"), "json")[0])
    error_keys = set(out_doc["errors"][0])
    causes = enum_values(out_doc)["errors[].cause"]
    r = run.diskgc("bogus")
    report.expect(r.code == 2, "usage error", "unknown command exit", 2, r.code)
    report.expect(r.err.startswith("diskgc: error: "), "usage error", "stderr prefix",
                  "`diskgc: error: MESSAGE`", r.err.splitlines()[:1])
    report.expect("diskgc --help" in r.err.splitlines()[-1], "usage error",
                  "last stderr line points to --help", "yes", r.err.splitlines()[-1:])
    r = run.diskgc("ensure", "--min-fre", "1G", "/tmp")
    report.expect("did you mean --min-free?" in r.err, "usage error", "`--min-fre` suggestion",
                  "did you mean --min-free?", r.err.splitlines()[:1])
    for argv in (["bogus", "--json"], ["profiles", "frob", "--json"], ["check", "--bogus", "--json"]):
        r = run.diskgc(*argv)
        obj = r.json()
        report.expect(obj is not None and set(obj) == {"status", "exit_code", "errors"},
                      "usage error", f"`{' '.join(argv)}` JSON keys",
                      "status, exit_code, errors", sorted(obj) if obj else r.out[:80])
        if obj:
            report.expect(obj["status"] == "usage_error" and obj["exit_code"] == r.code == 2,
                          "usage error", f"`{' '.join(argv)}` status/exit",
                          "usage_error/2", f"{obj['status']}/{r.code}")
            for err in obj["errors"]:
                report.expect(set(err) == error_keys, "usage error", "errors[] keys",
                              sorted(error_keys), sorted(err))
                report.expect(err["cause"] in causes, "usage error", "cause", sorted(causes), err["cause"])
    for planned in ("watch", "survey"):
        r = run.diskgc(planned, "--json")
        report.expect(r.code == 2, "planned command", f"`{planned}` exit", 2, r.code)


def probe_help_and_version(run, report):
    version = run.diskgc("--version")
    report.expect(version.code == 0 and re.fullmatch(r"diskgc \S+\n", version.out) is not None,
                  "version", "`--version`", "`diskgc VERSION`, exit 0", f"{version.out!r} exit {version.code}")
    for argv in (["-h"], ["--json", "--help"], ["profiles", "list", "--help"], ["ensure", "-h", "--json"]):
        r = run.diskgc(*argv)
        report.expect(r.code == 0 and r.out.startswith("usage:") and r.json() is None,
                      "help", f"`{' '.join(argv)}`", "help text, exit 0", f"exit {r.code}, {r.out[:30]!r}")
    for argv in (["--version"], ["--help"], ["profiles", "list"]):
        r = run.diskgc(*argv, redirect="> /dev/full")
        report.expect(r.code == 3, "stdout failure", f"`{' '.join(argv)} > /dev/full` exit", 3, r.code)
    return version.out.split()[1] if version.code == 0 else None


def dry(run, *args, select="profile:cargo-target", roots="empty", path="empty", env=None):
    """A dry run; roots=None for fixed-root sources, which take no --roots."""
    s = run.scratch
    root_args = ["--roots", f"{s}/{roots}"] if roots is not None else []
    return run.diskgc("ensure", "--dry-run", "--json", *args, "--select", select,
                      *root_args, f"{s}/{path}", env=env)


def probe_sizes_and_durations(reference, run, report):
    """reference.md, Sizes and durations. T units are not probed: on most
    hosts 1T exceeds the filesystem, which is exit 2 for another reason."""
    units, invalid, duration_units = size_grammar(reference)
    report.expect(len(units) == 12, "sizes", "documented units parsed", 12, sorted(units))
    for unit, factor in sorted(units.items()):
        if factor >= 1000**4:
            continue
        value = f"1{unit}"
        r = dry(run, "--min-free", value)
        obj = r.json() or {}
        report.expect(r.code == 0 and obj.get("min_free_bytes") == factor, "sizes",
                      f"--min-free {value}", f"exit 0, {factor} bytes",
                      f"exit {r.code}, {obj.get('min_free_bytes')}")
        report.expect(obj.get("target_free_bytes") == obj.get("min_free_bytes"), "defaults",
                      "--target-free default", "same as --min-free",
                      f"{obj.get('target_free_bytes')} vs {obj.get('min_free_bytes')}")
    for value in invalid:
        value = value if value[0].isdigit() else f"1{value}"
        r = dry(run, "--min-free", value)
        report.expect(r.code == 2, "sizes", f"--min-free {value}", "usage error, exit 2", f"exit {r.code}")
    for value in ("10%", "100%"):
        r = dry(run, "--min-free", value)
        report.expect(r.code == 0, "sizes", f"--min-free {value}", "exit 0", f"exit {r.code}")
    for value in ("1", "101%", "-1G"):
        r = dry(run, "--min-free", value)
        report.expect(r.code == 2, "sizes", f"--min-free {value}", "usage error, exit 2", f"exit {r.code}")
    for flag, value, ok in [("--timeout", f"2{u}", True) for u in duration_units] + \
            [("--timeout", "0", True), ("--tally-budget", "0", True), ("--timeout", "10", False),
             ("--timeout", "1M", False), ("--timeout", "1y", False), ("--timeout", "1.5h", False)]:
        r = dry(run, "--min-free", "1K", flag, value)
        report.expect((r.code == 0) == ok, "durations", f"{flag} {value}",
                      "accepted" if ok else "usage error", f"exit {r.code}")
    r = dry(run, "--min-free", "2G", "--target-free", "1G")
    report.expect(r.code == 2, "sizes", "--target-free below --min-free", "exit 2", f"exit {r.code}")


def probe_ensure_arguments(run, report):
    s = run.scratch
    cases = [
        (["ensure", "--dry-run", "--min-free", "1G", "--select", "profile:cargo-target",
          "--roots", f"{s}/empty", "empty"], "relative PATH"),
        (["ensure", "--dry-run", "--select", "profile:cargo-target", "--roots", f"{s}/empty",
          f"{s}/empty"], "missing --min-free"),
        (["ensure", "--dry-run", "--min-free", "1G", "--roots", f"{s}/empty", f"{s}/empty"],
         "--roots without --select"),
        (["ensure", "--dry-run", "--min-free", "1G", "--select", "profile:cargo-target",
          "--roots", f"{s}/no-such-root", f"{s}/empty"], "missing --roots root"),
        (["ensure", "--dry-run", "--min-free", "1G", "--select", "profile:cargo-target",
          f"{s}/empty"], "roots: required without --roots"),
        (["ensure", "--dry-run", "--min-free", "1G", "--select", "profile:cargo-target && ",
          "--roots", f"{s}/empty", f"{s}/empty"], "malformed --select"),
    ]
    for argv, what in cases:
        r = run.diskgc(*argv, "--json")
        obj = r.json() or {}
        report.expect(r.code == 2 and obj.get("status") == "usage_error", "ensure arguments", what,
                      "exit 2, usage_error", f"exit {r.code}, {obj.get('status')}")
    r = run.diskgc("ensure", "--dry-run", "--json", "--min-free", "1G", "--select",
                   "profile:cargo-target", "--roots", f"{s}/no-such-root", f"{s}/empty")
    causes = [e.get("cause") for e in (r.json() or {}).get("errors", [])]
    report.expect(causes == ["usage"], "ensure arguments", "missing --roots root cause",
                  "usage (only --roots missing)", causes)


def probe_report_fields(reference, run, report, version, status_codes):
    out_doc = json.loads(code_blocks(section(reference, "## Output"), "json")[0])
    schema = json_schema(out_doc)
    enums = enum_values(out_doc)
    seen_paths = set()
    runs = [dry(run, "--min-free", "100%", roots="targets", path="targets"),
            dry(run, "--min-free", "1K", select="profile:cargo-target && filter:idle-1d",
                roots="targets", path="targets")]
    if run.container:
        # The /tmp built-ins have the fixed root /tmp: on a host that is the
        # real, possibly huge /tmp (a pytest-tmp dry run on the developer's
        # laptop outran 120 s), in a walk container a seeded tmpfs.
        runs += [dry(run, "--min-free", "100%", select="profile:pytest-tmp && filter:idle-1d",
                     roots=None, path="empty"),
                 dry(run, "--min-free", "1K", "--target-free", "100%",
                     select="profile:nix-shell-rc", roots=None, path="empty")]
    for r in runs:
        obj = r.json()
        if obj is None:
            report.expect(False, "report", "dry run JSON", "one object", f"exit {r.code}: {r.err[-200:]}")
            continue
        absent, extra = schema_diff(schema, obj)
        report.expect(not absent, "report fields", "documented but absent", "present", absent)
        report.expect(not extra, "report fields", "present but undocumented", "documented", extra)
        seen_paths |= reached_paths(obj)
        for path, allowed in enums.items():
            for value in observed_values(obj, path):
                report.expect(value in allowed, "report enums", path, sorted(allowed), value)
        report.expect(obj.get("exit_code") == r.code, "report", "exit_code equals process exit",
                      r.code, obj.get("exit_code"))
        report.expect(r.code in status_codes.get(obj.get("status"), set()), "report",
                      f"status {obj.get('status')} exit", status_codes.get(obj.get("status")), r.code)
        report.expect(obj.get("version") == version, "report", "version", version, obj.get("version"))
        report.expect(obj.get("dry_run") is True, "report", "dry_run", True, obj.get("dry_run"))
    plan = (runs[0].json() or {}).get("plan", [])
    reasons = {Path(p["candidate"]).parent.name: (p["eligible"], p["reason"]) for p in plan
               if p.get("candidate")}
    report.expect(reasons.get("live") == (True, None), "dry run", "fresh target with --min-free 100%",
                  "eligible", reasons.get("live"))
    report.expect(reasons.get("kept") == (False, "keep_marker"), "dry run", "target with .diskgc-keep",
                  "keep_marker", reasons.get("kept"))
    idle = {Path(p["candidate"]).parent.name: p["reason"] for p in (runs[1].json() or {}).get("plan", [])
            if p.get("candidate")}
    report.expect(idle.get("live") == "not_idle", "dry run", "fresh target under filter:idle-1d",
                  "not_idle", idle.get("live"))
    report.expect((runs[0].json() or {}).get("seed", 0) is None, "report", "seed without random",
                  None, (runs[0].json() or {}).get("seed"))
    s = run.scratch
    run.sh(f"printf 'version: 1\\ngroups:\\n  - name: g\\n    ranking: random\\n    tiers:\\n"
           f"      - name: a\\n        select: profile:cargo-target\\n        roots: [{s}/targets]\\n"
           f"      - name: b\\n        select: profile:cargo-target && filter:idle-1d\\n"
           f"        roots: [{s}/targets]\\n' > cfg/random.yaml")
    r = run.diskgc("ensure", "--dry-run", "--json", "--min-free", "1K", "--config",
                   f"{s}/cfg/random.yaml", f"{s}/targets", env={"DISKGC_SEED": "42"})
    report.expect((r.json() or {}).get("seed") == 42, "configuration", "DISKGC_SEED with ranking: random",
                  "seed 42 in the report", f"exit {r.code}, seed {(r.json() or {}).get('seed')}")
    r = run.diskgc("ensure", "--dry-run", "--json", "--min-free", "1K", "--select",
                   "profile:cargo-target", "--roots", f"{run.scratch}/targets",
                   f"{run.scratch}/targets", redirect="> /dev/full")
    report.expect(r.code == 3, "stdout failure", "dry run `--json > /dev/full` exit", 3, r.code)
    return seen_paths


def reached_paths(obj, path=""):
    """Key paths an observation populated with a non-null value."""
    found = set()
    if isinstance(obj, dict):
        for k, v in obj.items():
            child = f"{path}.{k}" if path else k
            if v is not None and v != []:
                found.add(child)
            found |= reached_paths(v, child)
    elif isinstance(obj, list):
        for item in obj:
            found |= reached_paths(item, f"{path}[]")
    return found


def probe_ci_examples(reference, run, report, help_text):
    """The literal examples ask for 50G, more than a walk container's tmpfs
    has, so they run on the host only."""
    commands = section(reference, "## Commands")
    example = next(b for b in code_blocks(commands, "") if "$CI_BUILDS_DIR" in b)
    argv = shlex.split(example.replace("\\\n", " "))
    argv = argv[1:2] + ["--dry-run", "--json"] + argv[2:]
    builds = f"{run.scratch}/targets"
    argv = [a.replace("$CI_BUILDS_DIR", builds) for a in argv]
    r = run.diskgc(*argv)
    obj = r.json() or {}
    report.expect(r.code == 0 and obj.get("status") == "dry_run", "examples",
                  "Commands CI runner example (+ --dry-run --json)", "exit 0 dry_run",
                  f"exit {r.code} {obj.get('status')} {r.err[-200:] if r.code else ''}")
    start = help_text.split("Start here", 1)[1]
    lines = [ln.strip() for ln in start.splitlines()[1:3]]
    help_argv = shlex.split(" ".join(lines).replace("\\", " "))
    help_argv = [run.scratch + "/targets" if a in ("DIR", "PATH") else a for a in help_argv]
    r = run.diskgc(*help_argv[1:])
    report.expect(r.code == 0 and (r.json() or {}).get("status") == "dry_run", "examples",
                  "--help Start here example", "exit 0 dry_run", f"exit {r.code}")


def probe_profiles(reference, run, report, help_text):
    profiles_doc = section(reference, "### `profiles` command")
    blocks = code_blocks(profiles_doc, "json")
    list_doc, show_doc = json.loads(blocks[0]), json.loads(blocks[1])
    listed = run.diskgc("profiles", "list", "--json")
    lobj = listed.json() or {}
    report.expect(set(lobj) == set(list_doc), "profiles list --json", "top-level keys",
                  sorted(list_doc), sorted(lobj))
    profile_schema = json_schema(show_doc["profile"])
    for p in lobj.get("profiles", []):
        absent, extra = schema_diff(profile_schema, p)
        report.expect(not absent, "profiles list --json", f"{p.get('name')} documented but absent",
                      "present", absent)
        report.expect(not extra, "profiles list --json", f"{p.get('name')} present but undocumented",
                      "documented", extra)
    shown = run.diskgc("profiles", "show", "cargo-target", "--json").json() or {}
    report.expect(set(shown) == set(show_doc), "profiles show --json", "top-level keys",
                  sorted(show_doc), sorted(shown))
    unknown = run.diskgc("profiles", "show", "cargo-targe", "--json")
    causes = [e.get("cause") for e in (unknown.json() or {}).get("errors", [])]
    report.expect(unknown.code == 2 and causes == ["usage"], "profiles show", "unknown NAME",
                  "exit 2, cause usage", f"exit {unknown.code}, {causes}")
    report.expect("cargo-target" in unknown.err, "profiles show", "unknown NAME suggestion",
                  "names close names", unknown.err.splitlines()[:1])
    tested = run.diskgc("profiles", "test", "--json")
    tobj = tested.json() or {}
    documented_test = set(profiles_test_keys(reference))
    report.expect(set(tobj) == documented_test, "profiles test --json", "keys",
                  sorted(documented_test), sorted(tobj))
    report.expect(tested.code == 0 and tobj.get("status") == "passed", "profiles test",
                  "built-in examples", "passed, exit 0", f"{tobj.get('status')}, exit {tested.code}")
    table = run.diskgc("profiles", "list").out.splitlines()
    header = table[0].split() if table else []
    documented_header = ["NAME", "ORIGIN", "ROLE", "CATEGORY", "KIND", "ACTION", "ROOTS",
                         "CHECKOUTS", "DESCRIPTION"]
    report.expect(header == documented_header, "profiles list", "table columns",
                  documented_header, header)
    probe_catalog(reference, report, lobj.get("profiles", []), help_text)


def probe_catalog(reference, report, profiles, help_text):
    release, sources, filters, inactive = documented_catalog(reference)
    names = {p["name"] for p in profiles}
    bin_sources = {p["name"] for p in profiles if p["role"] == "source"}
    bin_filters = [p["name"] for p in profiles if p["role"] == "filter"]
    bin_release = {p["name"] for p in profiles if "0.1" in (p.get("releases") or [])}
    is_release_build = all("0.1" in (p.get("releases") or []) for p in profiles if p["role"] == "source")
    # The 0.1 catalog is its sources and every built-in filter.
    report.expect(bin_release == release | filters, "catalog", "0.1 catalog (`releases`)",
                  sorted(release | filters), sorted(bin_release))
    report.expect(set(bin_filters) == filters, "catalog", "built-in filters", sorted(filters), bin_filters)
    if is_release_build:
        report.expect(bin_sources == release, "catalog", "release build sources",
                      sorted(release), sorted(bin_sources))
    else:
        report.expect(not bin_sources - sources, "catalog", "built-ins missing from the library table",
                      "listed", sorted(bin_sources - sources))
        report.expect(not (sources - bin_sources), "catalog", "library rows the binary lacks",
                      "built in", sorted(sources - bin_sources))
    report.expect(not names & inactive, "catalog", "planned/withdrawn names in the binary",
                  "absent", sorted(names & inactive))
    in_help = section_lines_after(help_text, "Profiles built in")
    help_sources = {ln.split()[0] for ln in in_help if ln.startswith("  ") and not
                    ln.strip().startswith("filters:")}
    report.expect(help_sources == bin_sources, "help", "Profiles section sources",
                  sorted(bin_sources), sorted(help_sources))
    sorted_sources = sorted(bin_sources)
    order = [p["name"] for p in profiles]
    report.expect(order[:len(sorted_sources)] == sorted_sources, "profiles list", "order",
                  "sources first, by name", order[:len(sorted_sources)])


def section_lines_after(text, marker):
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.startswith(marker):
            return [ln for ln in lines[i + 1:] if ln.startswith("  ")]
    return []


def probe_config(reference, run, report, example_yaml, is_release_build):
    s = run.scratch
    r = run.diskgc("profiles", "list", "--json", env={"XDG_CONFIG_HOME": f"{s}/xdg"})
    report.expect((r.json() or {}).get("config") == f"{s}/xdg/diskgc/config.yaml", "config lookup",
                  "$XDG_CONFIG_HOME/diskgc/config.yaml", "used", (r.json() or {}).get("config"))
    report.expect(f"diskgc: config {s}/xdg/diskgc/config.yaml" in r.err, "config lookup",
                  "stderr `diskgc: config PATH`", "logged", r.err.splitlines()[:1])
    r = run.diskgc("profiles", "list", "--json")
    report.expect(r.code == 0 and (r.json() or {}).get("config") is None, "config lookup",
                  "no config found, profiles list", "exit 0, config null", f"exit {r.code}")
    for env, what in (({"DISKGC_CONFIG": "relative.yaml"}, "relative $DISKGC_CONFIG"),
                      ({"DISKGC_CONFIG": f"{s}/missing.yaml"}, "missing $DISKGC_CONFIG")):
        r = run.diskgc("check", env=env)
        report.expect(r.code == 2, "config lookup", what, "exit 2", f"exit {r.code}")
    r = run.sh(f"env XDG_CONFIG_HOME={s}/xdg-empty DISKGC_CONFIG= {shlex.quote(run.binary)} check")
    report.expect(r.code == 2, "config lookup", "empty $DISKGC_CONFIG", "exit 2", f"exit {r.code}")
    r = run.diskgc("check")
    report.expect(r.code == 2, "config lookup", "check with no config", "exit 2", f"exit {r.code}")
    r = run.diskgc("check", "--config", "xdg/diskgc/config.yaml")
    report.expect(r.code == 2, "config lookup", "relative --config", "exit 2", f"exit {r.code}")
    refused = dev_only_refusals(reference)
    examples = [(text, dev, f"reference example {i + 1}") for i, (text, dev)
                in enumerate(config_examples(reference))]
    examples.append((example_yaml, False, "docs/example.yaml"))
    for index, (text, dev_only, what) in enumerate(examples):
        path = f"{s}/cfg/example-{index}.yaml"
        run.sh(f"cat > {shlex.quote(path)} <<'EOF'\n{text}\nEOF")
        r = run.diskgc("check", "--config", path)
        expected = 2 if (dev_only and is_release_build) else 0
        report.expect(r.code == expected, "config examples", f"`check` on {what}",
                      f"exit {expected}", f"exit {r.code}: {r.err.strip()[-300:]}")
        if expected == 2:
            unnamed = [name for name in refused if f"`{name}`" not in r.err]
            report.expect(not unnamed, "config examples", f"0.1 refusal of {what} names",
                          sorted(refused), f"misses {unnamed}")
        if r.code == 0:
            lines = r.out.splitlines()
            report.expect(lines[:1] == [f"config {path}"], "check output", f"{what} first line",
                          f"config {path}", lines[:1])
            tiers = [ln.split(":")[0].split(".", 1)[-1] for ln in lines[1:]]
            report.expect(tiers == tier_names(text), "check output", f"{what} tier lines",
                          tier_names(text), tiers)
            bad = [ln for ln in lines[1:] if not re.fullmatch(r"[a-z0-9_-]+\.[a-z0-9_-]+: .+ => \[.*\]", ln)]
            report.expect(not bad, "check output", f"{what} line format",
                          "GROUP.TIER: EXPR => [profiles]", bad)
        r = run.diskgc("check", "--config", path, redirect="> /dev/full")
        if expected == 0:
            report.expect(r.code == 3, "stdout failure", f"`check` on {what} > /dev/full", 3, r.code)


def probe_exit_table(reference, report):
    """Lists the documented codes; returns those this mode cannot provoke."""
    rows = table_rows(section(reference, "## Exit codes (`ensure`)"), "| Code |")
    codes = set()
    for cells in rows:
        codes |= {int(c) for c in re.findall(r"\d+", cells[0])}
    return codes


def probe_reclaim(run, report, status_codes, reference):
    """Container only: a reclaim, a held host lock, and signals while waiting."""
    out_doc = json.loads(code_blocks(section(reference, "## Output"), "json")[0])
    schema = json_schema(out_doc)
    s = run.scratch
    sel = ["--select", "profile:cargo-target", "--roots", f"{s}/targets", f"{s}/targets"]
    r = run.diskgc("ensure", "--json", "--min-free", "100%", *sel)
    obj = r.json() or {}
    report.expect(r.code == 1 and obj.get("status") == "exhausted", "reclaim", "exit 1 exhausted",
                  "1 exhausted", f"{r.code} {obj.get('status')} {r.err[-300:] if r.code != 1 else ''}")
    absent, extra = schema_diff(schema, obj)
    report.expect(not absent and not extra, "reclaim fields", "actions/skipped",
                  "as documented", f"absent {absent}, undocumented {extra}")
    reasons = [x.get("reason") for x in obj.get("skipped", [])]
    report.expect("keep_marker" in reasons, "reclaim", "keep marker skip", "keep_marker", reasons)
    report.expect(len(obj.get("actions", [])) == 1, "reclaim", "actions for the live target", 1,
                  len(obj.get("actions", [])))
    make_fixtures(run)
    binary = shlex.quote(run.binary)
    args = " ".join(shlex.quote(a) for a in ["ensure", "--json", "--min-free", "100%", *sel])
    for signal, code, status, timeout in (("", 75, "lock_timeout", "2s"), ("TERM", 143, "interrupted", "60s"),
                                          ("INT", 130, "interrupted", "60s")):
        kill = f"sleep 2; kill -{signal} $pid;" if signal else ""
        # The holder execs into sleep with the only lock descriptor, so
        # killing it frees the lock (BusyBox flock has no -o).
        script = (f"(exec 9</run/diskgc/lock; flock 9; exec sleep 30) & holder=$!; sleep 1; "
                  f"{binary} {args} --timeout {timeout} > out.json 2> err.txt & pid=$!; {kill} "
                  f"wait $pid; echo $?; cat out.json; kill $holder")
        r = run.sh(script)
        lines = r.out.splitlines()
        got = int(lines[0]) if lines and lines[0].isdigit() else None
        try:
            obj = json.loads("\n".join(lines[1:]))
        except json.JSONDecodeError:
            obj = {}
        report.expect(got == code and obj.get("status") == status, "exit codes",
                      f"lock held{', SIG' + signal if signal else ''}", f"{code} {status}",
                      f"{got} {obj.get('status')}")
    return {1, 75, 130, 143}


NOT_CHECKED = [
    "exit 3 from a host or planning failure (only the stdout-write case is provoked)",
    "every documented default other than --target-free and the config lookup: --timeout 10m, "
    "--tally-budget, tier timeout 10m and the 10 s SIGKILL grace leave no trace in the report",
    "human output wording (summary lines, `would reclaim`, progress logs): text is not a contract",
    "stderr warnings of `check` (checkouts: allow, wildcard paths, missing roots) beyond exit codes",
    "the profile schema and selection grammar tables (Predicates, Selection grammar, Actions): the "
    "Rust doc tests (diskgc-core doc.rs) and the config loader cover them; user profiles are refused in 0.1",
    "enum values the probes never produce (most skip reasons, process_blockers evidence): only "
    "observed values are checked against the documented sets",
    "`index` object fields: no build has an index yet (always null)",
    "watch and survey syntax: planned, only their exit 2 is checked",
    "T, TiB and TB sizes: 1T exceeds most test filesystems, which is exit 2 for another reason",
    "the RLIMIT_NOFILE raise, signal handling during a running child, Docker/Nix backend skips",
    "the /etc/diskgc/config.yaml fallback: probes pin XDG_CONFIG_HOME and never write /etc",
]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("binary", help="diskgc binary (inside the container with --container)")
    parser.add_argument("--reference", default="docs/reference.md")
    parser.add_argument("--example", default="docs/example.yaml")
    parser.add_argument("--scratch", help="scratch directory (default tmp/docs-vs-binary; "
                        "/home/tester/docs-vs-binary in a container)")
    parser.add_argument("--container", metavar="NAME",
                        help="run in walk container walk-diskgc-NAME and add the reclaiming probes")
    args = parser.parse_args(argv)
    reference = Path(args.reference).read_text()
    example_yaml = Path(args.example).read_text()
    if args.container:
        scratch = args.scratch or "/home/tester/docs-vs-binary"
        binary = args.binary
    else:
        scratch = str(Path(args.scratch or "tmp/docs-vs-binary").resolve())
        binary = str(Path(args.binary).resolve())
        if not os.access(binary, os.X_OK):
            parser.error(f"{args.binary} is not executable")
        Path(scratch).mkdir(parents=True, exist_ok=True)
    run = Runner(binary, scratch, args.container)
    if args.container:
        made = run.prefix + ["mkdir", "-p", scratch]
        subprocess.run(made, check=True, timeout=PROBE_TIMEOUT_SECONDS)
    make_fixtures(run)
    report = Report()
    status_codes = status_exit_codes(section(reference, "## Exit codes (`ensure`)"))
    help_text = probe_usage(reference, run, report)
    probe_errors(reference, run, report)
    version = probe_help_and_version(run, report)
    probe_sizes_and_durations(reference, run, report)
    probe_ensure_arguments(run, report)
    probe_report_fields(reference, run, report, version, status_codes)
    if not args.container:
        probe_ci_examples(reference, run, report, help_text)
    profiles = (run.diskgc("profiles", "list", "--json").json() or {}).get("profiles", [])
    is_release_build = bool(profiles) and all(
        "0.1" in (p.get("releases") or []) for p in profiles if p["role"] == "source")
    probe_profiles(reference, run, report, help_text)
    probe_config(reference, run, report, example_yaml, is_release_build)
    documented_codes = probe_exit_table(reference, report)
    provoked = {0, 2, 3}
    if args.container:
        provoked |= probe_reclaim(run, report, status_codes, reference)
    not_checked = list(NOT_CHECKED)
    if args.container:
        not_checked.insert(0, "the 50G Commands and --help examples: more than the container's "
                           "tmpfs; they run on the host")
    else:
        not_checked.insert(0, "dry runs of the fixed-root /tmp built-ins (pytest-tmp, "
                           "nix-shell-rc): the host's /tmp is real and large; rerun with --container NAME")
    missing = sorted(documented_codes - provoked)
    if missing:
        not_checked.insert(0, f"exit codes {missing}: "
                           + ("124 needs a reclaim that outlasts --timeout" if args.container else
                              "need a reclaim; rerun with --container NAME"))
    build = "release (0.1 catalog)" if is_release_build else "development"
    print(f"docs-vs-binary: {binary} ({build} build, {version}), "
          f"{'container ' + args.container if args.container else 'host, read-only'}")
    for line in report.mismatches:
        print(f"MISMATCH {line}")
    for line in not_checked:
        print(f"NOT CHECKED {line}")
    print(f"docs-vs-binary: {report.checked} checks, {len(report.mismatches)} mismatches")
    return 1 if report.mismatches else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, ValueError, OSError) as error:
        print(f"docs-vs-binary: error: {error}", file=sys.stderr)
        sys.exit(2)
