# vm-release-tmp-reclaim (TASK-156) test script, appended to the prelude in
# vm-release-tmp-reclaim.nix that defines RELEASE, VERSION, COHORT, PYTEST,
# NIX and BASH. Runs in the NixOS test driver; every diskgc run is inside
# the guest, as an ordinary user.
#
# Every diskgc command, its exit code, stdout (human summary or JSON report)
# and stderr are saved under $out/evidence and printed to the build log.
import json
import shlex
import time
from typing import Any

DAY_SECONDS = 86400
# Older than idle-1d and pytest's dead-lock age (3 d + 1 min, safety.md),
# younger than idle-7d and than systemd-tmpfiles' 10 d /tmp age rule.
CLOCK_BACK_SECONDS = 5 * DAY_SECONDS
USERS = ("alice", "bob")
UNREACHABLE = "100%"  # never met on a non-empty filesystem: reclaim all

evidence_dir = machine.out_dir / "evidence"
evidence_dir.mkdir(parents=True, exist_ok=True)
started_monotonic = time.monotonic()


def save(name: str, text: str) -> None:
    (evidence_dir / name).write_text(text)
    print(f"===== {name} =====\n{text}")


def su(user: str, command: str, timeout: int = 300) -> tuple[int, str]:
    return machine.execute(f"su - {user} -c {shlex.quote(command)}", timeout=timeout)


def ensure(user: str, args: str, tag: str) -> tuple[int, str]:
    """diskgc ensure as USER; returns its exit code and stdout."""
    err = f"/home/{user}/diskgc-{tag}.stderr"
    command = f"diskgc ensure {args}"
    began = time.monotonic()
    code, out = su(user, f"{command} 2> {err}")
    elapsed = time.monotonic() - began
    save(f"{tag}.cmd", f"user: {user}\ncommand: {command}\nexit: {code}\nseconds: {elapsed:.1f}\n")
    save(f"{tag}.stdout", out)
    save(f"{tag}.stderr", machine.succeed(f"cat {err}"))
    return code, out


def report(out: str, code: int, status: str) -> dict[str, Any]:
    result: dict[str, Any] = json.loads(out)
    assert result["exit_code"] == code, (result["exit_code"], code)
    assert result["status"] == status, (result["status"], status, result["errors"])
    assert result["version"] == VERSION, result["version"]
    assert result["path"] == "/tmp", result["path"]
    return result


def tmp_tree() -> set[str]:
    return set(machine.succeed("find /tmp -mindepth 1 -printf '%p\\n'").splitlines())


def require_removed(before: set[str], after: set[str], candidates: set[str], tag: str) -> None:
    """Exactly the candidates' trees left /tmp, nothing else did."""
    for candidate in candidates:
        assert candidate in before, (tag, candidate)
    expected = {p for p in before if any(p == c or p.startswith(c + "/") for c in candidates)}
    removed = before - after
    assert removed == expected, (tag, sorted(removed - expected), sorted(expected - removed))


def cohort(user: str) -> dict[str, str]:
    code, out = su(user, f"sh -x {COHORT} {PYTEST} {NIX} {BASH} 2>&1")
    save(f"cohort-{user}.txt", out)
    assert code == 0, (user, code)
    facts: dict[str, str] = {}
    for line in out.splitlines():
        if line.startswith("COHORT "):
            key, _, value = line[len("COHORT "):].partition("=")
            facts[key] = value
    assert facts.get("done") == "yes", out
    return facts


def skipped_row(result: dict[str, Any], candidate: str) -> dict[str, Any]:
    rows = [r for r in result["skipped"] if r["candidate"] == candidate]
    assert len(rows) == 1, (candidate, rows)
    return rows[0]


def require_kept(result: dict[str, Any], kept: dict[str, tuple[str, int | None]]) -> None:
    """Each kept candidate is in_use with the named evidence (and owner pid)."""
    for candidate, (evidence, pid) in kept.items():
        row = skipped_row(result, candidate)
        assert row["reason"] == "in_use", (candidate, row)
        blockers = row["process_blockers"] or []
        matching = [b for b in blockers if b["evidence"] == evidence]
        assert matching, (candidate, row)
        if pid is not None:
            assert any(b["pid"] == pid for b in matching), (candidate, pid, row)


def require_actions(result: dict[str, Any], profile: str, candidates: set[str]) -> None:
    actions = result["actions"]
    assert {a["candidate"] for a in actions} == candidates, (sorted(candidates), actions)
    assert len(actions) == len(candidates), actions
    for action in actions:
        assert action["profile"] == profile, action
        age = action["idle_age_seconds"]
        # The cohort's real kernel ctime, five days back, plus the test's
        # own minutes; neither a clock offset nor a fresh inode.
        assert age is not None and 4.9 * DAY_SECONDS < age < 5.2 * DAY_SECONDS, action
    deltas = [a["avail_delta_bytes"] for a in actions]
    assert all(d is not None for d in deltas) and sum(deltas) > 0, deltas


# ---- The guest, the package as installed ----
machine.start()
machine.wait_for_unit("multi-user.target")
assert "diskgc.e2e.release_tmp_reclaim=1" in machine.succeed("cat /proc/cmdline").split()
installed = su("alice", "readlink -f $(command -v diskgc)")[1].strip()
assert installed == f"{RELEASE}/bin/diskgc", installed
assert su("alice", "diskgc --version")[1].strip() == f"diskgc {VERSION}"
# The drop-in, applied at boot by systemd-tmpfiles as on any NixOS host.
lock = machine.succeed("stat -c '%U %G %a %F' /run/diskgc /run/diskgc/lock").splitlines()
assert lock == ["root root 755 directory", "root root 644 regular empty file"], lock
assert machine.succeed("findmnt -n -o FSTYPE -T /tmp").strip() == "tmpfs"
# Owner PID's preconditions (safety.md): no hidepid on /proc.
assert "hidepid" not in machine.succeed("findmnt -n -o OPTIONS /proc")

# ---- The cohorts, made five days in the past ----
host_monotonic = time.monotonic()
guest_now = int(machine.succeed("date -u +%s"))
facts: dict[str, dict[str, str]] = {}
try:
    machine.succeed(f"date -u -s '@{guest_now - CLOCK_BACK_SECONDS}' >/dev/null")
    for user in USERS:
        facts[user] = cohort(user)
    machine.succeed("echo root > /tmp/root-notes.txt")
    guest_old = int(machine.succeed("date -u +%s"))
    assert 0 <= guest_old - (guest_now - CLOCK_BACK_SECONDS) < 600, guest_old
finally:
    restored = int(guest_now + (time.monotonic() - host_monotonic))
    machine.succeed(f"date -u -s '@{restored}' >/dev/null")
ctime = int(machine.succeed("stat -c %Z /tmp/pytest-of-alice/pytest-0"))
assert 4.9 * DAY_SECONDS < restored - ctime < 5.1 * DAY_SECONDS, (restored, ctime)

expected: dict[str, dict[str, Any]] = {}
for user in USERS:
    f = facts[user]
    p = f"/tmp/pytest-of-{user}"
    sessions = sorted(f["sessions"].strip(",").split(","))
    assert sessions == ["notes"] + [f"pytest-{n}" for n in range(6)] + ["pytest-current"], sessions
    assert f["lock_0"] == f["killed_pytest_0"] and f["lock_1"] == f["killed_pytest_1"], f
    assert f["lock_2"] == f["lock_3"] == f["lock_4"] == "none", f
    assert f["lock_5"] == f["live_pytest"], f
    live_shell, live_shell_pid = f["live_nix_shell"].split(":")
    live_develop, live_develop_pid = f["live_nix_develop"].split(":")
    expected[user] = {
        "pytest_reclaim": {f"{p}/pytest-{n}" for n in (0, 1, 2)},
        "pytest_kept": {
            f"{p}/pytest-3": ("temp_root", None),
            f"{p}/pytest-4": ("temp_root", None),
            f"{p}/pytest-5": ("owner_pid", int(f["live_pytest"])),
        },
        "nix_reclaim": {f["dead_nix_shell"], f["dead_nix_develop"]},
        "nix_kept": {
            # The kernel shows the shell still reading its rc script (fd
            # 255, bash's script fd under --run), so the in-use scan keeps
            # it before Owner PID is asked.
            live_shell: ("fd", int(live_shell_pid)),
            live_develop: ("owner_pid", int(live_develop_pid)),
        },
        "live_pids": [int(f["live_pytest"]), int(live_shell_pid), int(live_develop_pid)],
    }
unrelated = {
    "/tmp/root-notes.txt": "root",
    **{f"/tmp/{u}-notes.txt": "notes" for u in USERS},
    **{f"/tmp/{u}-work/data.txt": "data" for u in USERS},
    **{f"/tmp/nix-shell-notapid-{u}/rc": "rc" for u in USERS},
    **{f"/tmp/nix-shell.{u}/rc": "rc" for u in USERS},
    **{f"/tmp/pytest-of-{u}/notes/kept.txt": "kept" for u in USERS},
}
alice = expected["alice"]

# ---- alice: --select, the built-in profiles alone ----
before = tmp_tree()
code, out = ensure("alice", "/tmp --min-free 1M --select profile:pytest-tmp", "a1-met")
assert code == 0 and out == "", (code, out)  # met: silent, like cp
assert tmp_tree() == before

# The age is real: five days passes idle-1d and fails idle-7d.
code, out = ensure(
    "alice",
    f"/tmp --min-free {UNREACHABLE} --select 'profile:pytest-tmp && filter:idle-7d' --dry-run --json",
    "a2-dry-idle-7d",
)
result = report(out, 0, "dry_run")
assert not [r for r in result["plan"] if r["eligible"]], result["plan"]
too_young = {r["candidate"]: r["reason"] for r in result["plan"]
             if (r["candidate"] or "").startswith("/tmp/pytest-of-alice/")}
assert too_young == {f"/tmp/pytest-of-alice/pytest-{n}": "not_idle" for n in range(6)}, too_young

code, out = ensure(
    "alice", f"/tmp --min-free {UNREACHABLE} --select profile:pytest-tmp --dry-run --json", "a3-dry-pytest"
)
result = report(out, 0, "dry_run")
mine = [r for r in result["plan"] if (r["candidate"] or "").startswith("/tmp/pytest-of-alice/")]
assert {r["candidate"] for r in mine if r["eligible"]} == alice["pytest_reclaim"], mine
for candidate in alice["pytest_kept"]:
    rows = [r for r in mine if r["candidate"] == candidate]
    assert len(rows) == 1 and not rows[0]["eligible"] and rows[0]["reason"] == "in_use", rows
assert tmp_tree() == before

code, out = ensure("alice", f"/tmp --min-free {UNREACHABLE} --select profile:pytest-tmp --json", "a4-pytest")
result = report(out, 1, "exhausted")
assert result["dry_run"] is False and result["errors"] == [], result["errors"]
require_actions(result, "pytest-tmp", alice["pytest_reclaim"])
require_kept(result, alice["pytest_kept"])
after = tmp_tree()
require_removed(before, after, alice["pytest_reclaim"], "a4-pytest")

before = after
code, out = ensure("alice", f"/tmp --min-free {UNREACHABLE} --select profile:nix-shell-rc --json", "a5-nix")
result = report(out, 1, "exhausted")
assert result["errors"] == [], result["errors"]
require_actions(result, "nix-shell-rc", alice["nix_reclaim"])
require_kept(result, alice["nix_kept"])
assert skipped_row(result, "/tmp/nix-shell-notapid-alice")["reason"] == "filtered"
after = tmp_tree()
require_removed(before, after, alice["nix_reclaim"], "a5-nix")

# ---- bob: the shipped example config ----
example = f"{RELEASE}/share/doc/diskgc/examples/example.yaml"
before = after
code, out = ensure("bob", f"/tmp --min-free {UNREACHABLE} --config {example} --json", "b1-example-as-shipped")
result = report(out, 2, "usage_error")
assert [e["cause"] for e in result["errors"]] == ["config"], result["errors"]
assert "/home/example/src" in result["errors"][0]["message"], result["errors"]
assert tmp_tree() == before

# What its header says to do: point the roots at a directory that exists.
su_code, su_out = su("bob", f"mkdir -p src && sed 's#/home/example/src#/home/bob/src#' {example} > diskgc.yaml && diskgc check --config /home/bob/diskgc.yaml")
save("b2-check.txt", f"exit: {su_code}\n{su_out}")
assert su_code == 0, su_out

code, out = ensure("bob", f"/tmp --min-free {UNREACHABLE} --config /home/bob/diskgc.yaml", "b3-example-human")
assert code == 1, code
lines = out.splitlines()
bob = expected["bob"]
for candidate in bob["pytest_reclaim"] | bob["nix_reclaim"]:
    assert [l for l in lines if f"reclaimed {candidate}:" in l], (candidate, out)
for candidate in {**bob["pytest_kept"], **bob["nix_kept"]}:
    assert [l for l in lines if "kept" in l and f"{candidate}: in_use" in l], (candidate, out)
after = tmp_tree()
require_removed(before, after, bob["pytest_reclaim"] | bob["nix_reclaim"], "b3-example-human")

before = after
code, out = ensure("bob", f"/tmp --min-free {UNREACHABLE} --config /home/bob/diskgc.yaml --json", "b4-example-json")
result = report(out, 1, "exhausted")
assert result["errors"] == [] and result["actions"] == [], result
require_kept(result, {**bob["pytest_kept"], **bob["nix_kept"]})
assert tmp_tree() == before

# ---- Nothing else was touched, and the live producers never noticed ----
for path, content in unrelated.items():
    assert machine.succeed(f"cat {path}").strip() == content, path
for user in USERS:
    for pid in expected[user]["live_pids"]:
        machine.succeed(f"test -d /proc/{pid}")
    # The live pytest session finishes its test in the directory diskgc kept.
    machine.succeed(f"su - {user} -c 'touch release'")
    machine.wait_until_succeeds(f"test -s /home/{user}/live-pytest.exit", timeout=60)
    assert machine.succeed(f"cat /home/{user}/live-pytest.exit").strip() == "0", user
save("summary.txt", f"seconds since driver start: {time.monotonic() - started_monotonic:.0f}\n")
