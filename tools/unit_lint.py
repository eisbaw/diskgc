#!/usr/bin/env python3
"""Lint the packaged `watch` unit templates (docs/testing.md, Deployment units).

The spec is docs/architecture.md, Deployment: its ```ini block after "The
user unit, in full:" is the user template's whole text, and the next ```ini
block lists what the system template adds (its [Install] replaces the
user's). This lint checks that the files in packaging/systemd/ are exactly
that, and, independently of the doc, that both carry the required restart,
stop and hardening lines and none of the options the Hardening table rejects,
so an edit of the doc and the files together cannot slip one in.

`systemd-analyze verify` (flake check `deployment-units`) checks that systemd
accepts the files; it only warns on unknown keys and bad values, so the
exact text is pinned here.
"""

import argparse
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
USER_UNIT = ROOT / "packaging/systemd/user/diskgc-watch@.service"
SYSTEM_UNIT = ROOT / "packaging/systemd/system/diskgc-watch@.service"
ARCHITECTURE = ROOT / "docs/architecture.md"

# Both templates, [Service]: restart and stop (architecture.md, Restart and
# Stop) and the shipped hardening rows of the Hardening table.
REQUIRED_SERVICE = {
    "Type": "exec",
    "Restart": "always",
    "RestartSec": "30s",
    "RestartPreventExitStatus": "2",
    "KillMode": "mixed",
    "TimeoutStopSec": "30s",
    "NoNewPrivileges": "yes",
    "LockPersonality": "yes",
    "RestrictRealtime": "yes",
    "RestrictNamespaces": "yes",
    "RestrictSUIDSGID": "no",
    "RestrictAddressFamilies": "AF_UNIX AF_NETLINK",
    "SystemCallArchitectures": "native",
    "SystemCallFilter": "@system-service",
    "SystemCallErrorNumber": "EPERM",
}

# Keys neither template may set, with why (the Hardening table's "no",
# "fatal" and "not set" rows, and the unit design's omissions).
FORBIDDEN_ANY = {
    "PrivateTmp": "fatal: every /tmp candidate invisible",
    "PrivateUsers": "fatal: every in-use read refused",
    "PrivatePIDs": "unsafe: the in-use scan sees only the unit",
    "PrivateNetwork": "unsafe under ipc: live-only",
    "ProtectHome": "hides or freezes the user's trees",
    "ProcSubset": "hides boot_id and /proc/pressure",
    "DynamicUser": "fatal: implies PrivateTmp, owns no candidate",
    "RemoveIPC": "removes the user's own IPC objects",
    "ReadWritePaths": "would copy the configuration's roots into the unit",
    "Nice": "the reclaim must not run at lowered priority",
    "IOSchedulingClass": "the reclaim must not run at lowered priority",
    "IOSchedulingPriority": "the reclaim must not run at lowered priority",
    "CPUSchedulingPolicy": "the reclaim must not run at lowered priority",
    "CPUWeight": "the reclaim must not run at lowered priority",
    "IOWeight": "the reclaim must not run at lowered priority",
    # Forbidden outright, not only below some value: the unit cannot know
    # the host's process count, and systemd's default hard limit (524288)
    # is the one the design relies on (architecture.md, LimitNOFILE=).
    "LimitNOFILE": "a lower limit fails the in-use scan",
    "MemoryDenyWriteExecute": "crashes JIT command actions",
    "User": "the system template gets User= from a per-instance drop-in",
    "StateDirectory": "would split the watcher's snapshot from the hook's",
    "ExecReload": "diskgc does not reload",
}
# Forbidden unless the value is the harmless one.
FORBIDDEN_UNLESS = {
    "ProtectProc": ("default", "hidepid hides the user's own processes"),
    "ProtectSystem": ("no", "a mount namespace, and strict makes every tree read-only"),
}
# In a user manager every option that needs a mount namespace or edits the
# capability bounding set implies PrivateUsers= (fatal). The list is the
# Sandboxing and Capabilities options of systemd.exec (systemd 260) that do;
# a new systemd may add more, which the doc-equality check still catches.
FORBIDDEN_USER = {
    "CapabilityBoundingSet", "AmbientCapabilities", "ProtectClock", "PrivateDevices",
    "ProtectKernelTunables", "ProtectKernelModules", "ProtectKernelLogs",
    "ProtectControlGroups", "ProtectHostname", "PrivateMounts", "PrivateIPC",
    "ReadOnlyPaths", "InaccessiblePaths", "TemporaryFileSystem", "BindPaths",
    "BindReadOnlyPaths", "RootDirectory", "RootImage", "MountAPIVFS",
    "NoExecPaths", "ExecPaths", "MountFlags", "NetworkNamespacePath",
    "IPCNamespacePath", "ExtensionImages", "ExtensionDirectories", "MountImages",
    "LogNamespace",
}

# The host lock (docs/safety.md, Host lock; TASK-277): the system template
# requires it, so a fleet's instances never fall back to per-user locks;
# the user template must work without it (a user's reclaims fall back).
HOST_LOCK = "/run/diskgc/lock"
REQUIRED_SYSTEM_UNIT = {"AssertPathExists": HOST_LOCK}

SECTION = re.compile(r"^\[([A-Za-z]+)\]$")


def parse(text, origin):
    """The unit's assignments in order, as (section, key, value).

    Raises ValueError on anything this lint does not expect in a shipped
    template: a continuation line, an assignment outside a section, a
    repeated key in a section, or a line that is not `Key=value`.
    """
    entries, seen, section = [], set(), None
    for number, line in enumerate(text.splitlines(), 1):
        where = f"{origin}:{number}"
        if not line.strip() or line.lstrip().startswith(("#", ";")):
            continue
        if line.endswith("\\"):
            raise ValueError(f"{where}: continuation lines are not used in the templates")
        header = SECTION.match(line)
        if header:
            section = header.group(1)
            continue
        if section is None:
            raise ValueError(f"{where}: assignment before any [Section]")
        key, sep, value = line.partition("=")
        if not sep or not re.fullmatch(r"[A-Za-z]+", key):
            raise ValueError(f"{where}: not a Key=value line: {line!r}")
        if (section, key) in seen:
            raise ValueError(f"{where}: {key}= repeated in [{section}]")
        seen.add((section, key))
        entries.append((section, key, value))
    return entries


def check_hardening(entries, manager):
    """Errors for one template; `manager` is "user" or "system"."""
    errors = []
    service = {key: value for section, key, value in entries if section == "Service"}
    for key, want in REQUIRED_SERVICE.items():
        got = service.get(key)
        if got != want:
            shown = "missing" if got is None else f"{key}={got}"
            errors.append(f"{manager}: [Service] needs {key}={want} ({shown})")
    for section, key, value in entries:
        if key in FORBIDDEN_ANY:
            errors.append(f"{manager}: [{section}] {key}= is forbidden: {FORBIDDEN_ANY[key]}")
        if key in FORBIDDEN_UNLESS and value != FORBIDDEN_UNLESS[key][0]:
            allowed, why = FORBIDDEN_UNLESS[key]
            errors.append(f"{manager}: [{section}] {key}={value} is forbidden ({allowed} only): {why}")
        if manager == "user" and key in FORBIDDEN_USER:
            errors.append(
                f"user: [{section}] {key}= implies PrivateUsers= in a user manager (fatal)"
            )
        if manager == "user" and HOST_LOCK in value:
            errors.append(
                f"user: [{section}] {key}= names {HOST_LOCK}: the user template must work "
                "without the host lock (a user's reclaims fall back to a per-user lock)"
            )
    if manager == "system":
        unit = {key: value for section, key, value in entries if section == "Unit"}
        for key, want in REQUIRED_SYSTEM_UNIT.items():
            got = unit.get(key)
            if got != want:
                shown = "missing" if got is None else f"{key}={got}"
                errors.append(
                    f"system: [Unit] needs {key}={want} ({shown}): the system template "
                    "never falls back to a per-user lock"
                )
    return errors


def doc_blocks(doc):
    """The user unit's full text and the system additions, from the doc."""
    def ini_after(marker):
        at = doc.find(marker)
        if at < 0:
            raise ValueError(f"architecture.md: no {marker!r}")
        start = doc.find("```ini\n", at)
        end = doc.find("\n```", start + 1)
        if start < 0 or end < 0:
            raise ValueError(f"architecture.md: no ```ini block after {marker!r}")
        return doc[start + len("```ini\n"):end + 1]
    return (
        ini_after("The user unit, in full:"),
        ini_after("The system template is the same file with"),
    )


def expected_system(user_entries, additions):
    """The user template with the doc's additions: [Unit] and [Service] lines
    appended to their sections, [Install] replaced."""
    replaced = {section for section, _, _ in additions if section == "Install"}
    out = []
    for section in ("Unit", "Service", "Install"):
        if section not in replaced:
            out += [e for e in user_entries if e[0] == section]
        out += [e for e in additions if e[0] == section]
    return out


def lint(user_text, system_text, doc):
    """Every error; empty when the templates match the spec."""
    errors = []
    user = parse(user_text, "user")
    system = parse(system_text, "system")
    errors += check_hardening(user, "user")
    errors += check_hardening(system, "system")
    doc_user, doc_additions = doc_blocks(doc)
    if user_text != doc_user:
        errors.append("user: the file differs from architecture.md's \"The user unit, in full\"")
    want = expected_system(parse(doc_user, "architecture.md user"),
                           parse(doc_additions, "architecture.md system"))
    if system != want:
        missing = [e for e in want if e not in system]
        extra = [e for e in system if e not in want]
        errors.append(
            f"system: not the user unit plus architecture.md's additions "
            f"(missing {missing}, extra {extra}, or out of order)"
        )
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--user", type=Path, default=USER_UNIT)
    parser.add_argument("--system", type=Path, default=SYSTEM_UNIT)
    parser.add_argument("--doc", type=Path, default=ARCHITECTURE)
    args = parser.parse_args(argv)
    try:
        errors = lint(args.user.read_text(), args.system.read_text(), args.doc.read_text())
    except (OSError, ValueError) as error:
        print(f"unit lint: {error}", file=sys.stderr)
        return 1
    for error in errors:
        print(f"unit lint: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
