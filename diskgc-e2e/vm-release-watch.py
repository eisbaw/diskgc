# vm-release-watch (TASK-362) test script, appended to the prelude in
# vm-release-watch.nix that defines RELEASE and VERSION. Runs in the NixOS
# test driver; every diskgc run is inside the guest. Each phase's records
# and journal are saved under $out/evidence and printed to the build log.
#
# alice's user instance watches /srv/alice (ext4): thresholds and
# hysteresis, the tally's pressure brake under a fork storm, a busy host
# (the storm beside 1,000 idle processes) waited out during a cycle, a
# cycle that ends `lock_timeout` behind a concurrent `ensure` whose
# cleaner holds the lock, a cycle that ends `deadline` while its own
# cleaner runs, each followed by a cycle that reclaims after its back-off,
# then SIGHUP restarts (Restart=always) and an exit 2 that is not
# restarted (RestartPreventExitStatus=2). The system instance watches
# /srv/ci as runner (a User= drop-in): a fill it reclaims, then
# `systemctl stop`.
import datetime
import json
import shlex
import time
from typing import Any, Callable

MIB = 1 << 20
PAD_MIB = 8
INTERVAL = "2s"

evidence_dir = machine.out_dir / "evidence"
evidence_dir.mkdir(parents=True, exist_ok=True)


def save(name: str, text: str) -> None:
    (evidence_dir / name).write_text(text)
    print(f"===== {name} =====\n{text}")


def su(user: str, command: str, timeout: int = 300) -> tuple[int, str]:
    return machine.execute(f"su - {user} -c {shlex.quote(command)}", timeout=timeout)


def su_ok(user: str, command: str) -> str:
    code, out = su(user, command)
    assert code == 0, (user, command, code, out)
    return out


def avail(path: str) -> int:
    a, s = machine.succeed(f"stat -f -c '%a %S' {path}").split()
    return int(a) * int(s)


def fill_to(user: str, path: str, level: int, name: str) -> None:
    """Writes a non-candidate file so that PATH's available space is LEVEL."""
    need = avail(path) - level
    assert need > 0, (path, level, avail(path))
    su_ok(user, f"fallocate -l {need} {path}/{name}")


def diagnostics() -> None:
    """The journals a failed wait needs, printed to the build log (a failed
    check keeps no $out)."""
    for unit in ("diskgc-watch@srv-alice.service",):
        print(f"===== user journal {unit} =====")
        print(machine.execute(f"journalctl --no-pager -o short-monotonic _SYSTEMD_USER_UNIT={unit} | tail -n 200")[1])
    for unit in ("diskgc-watch@srv-ci.service", "ensure-d.service"):
        print(f"===== journal {unit} =====")
        print(machine.execute(f"journalctl --no-pager -o short-monotonic _SYSTEMD_UNIT={unit} | tail -n 100")[1])
    print(machine.execute("uptime; cat /proc/pressure/cpu; ps -e --no-headers | wc -l")[1])


def wait_for(what: str, probe: Callable[[], Any], timeout: float) -> Any:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        got = probe()
        if got:
            return got
        time.sleep(1)
    diagnostics()
    raise AssertionError(f"timed out after {timeout:.0f} s: {what}")


def user_journal(unit: str, fmt: str = "cat") -> str:
    return machine.succeed(f"journalctl --no-pager -o {fmt} _SYSTEMD_USER_UNIT={unit}")


def system_journal(unit: str, fmt: str = "cat") -> str:
    return machine.succeed(f"journalctl --no-pager -o {fmt} _SYSTEMD_UNIT={unit}")


def records(journal: str) -> list[dict[str, Any]]:
    """The watch records (JSON lines on stdout) in a unit's journal."""
    out = []
    for line in journal.splitlines():
        if line.startswith("{") and '"watch":' in line:
            out.append(json.loads(line))
    return out


def ended_at(record: dict[str, Any]) -> datetime.datetime:
    started = datetime.datetime.fromisoformat(record["started_at"].replace("Z", "+00:00"))
    return started + datetime.timedelta(milliseconds=record["duration_ms"])


def started_at(record: dict[str, Any]) -> datetime.datetime:
    return datetime.datetime.fromisoformat(record["started_at"].replace("Z", "+00:00"))


def actions(record: dict[str, Any]) -> list[str]:
    return sorted(a["candidate"] for a in record["actions"])


def skips(record: dict[str, Any], candidate: str) -> list[dict[str, Any]]:
    return [s for s in record["skipped"] if s["candidate"] == candidate]


def show(unit: str, props: list[str], user: str | None = None) -> dict[str, str]:
    manager = f"--user -M {user}@ " if user else ""
    out = machine.succeed(
        f"systemctl {manager}show --all {unit} " + " ".join(f"-p {p}" for p in props)
    )
    return dict(line.split("=", 1) for line in out.strip().splitlines())


def projects(user: str, base: str, names: list[str]) -> list[str]:
    """Real pytest projects, one second apart (the `oldest` order), each
    with a PAD_MIB payload in its .pytest_cache (pytest's v/cache)."""
    caches = []
    for name in names:
        su_ok(
            user,
            f"mkdir -p {base}/{name} && cd {base}/{name}"
            " && printf 'def test_ok():\\n    assert True\\n' > test_ok.py"
            " && pytest -q >/dev/null && test -d .pytest_cache"
            f" && mkdir -p .pytest_cache/v/cache && fallocate -l {PAD_MIB}M .pytest_cache/v/cache/pad",
        )
        caches.append(f"{base}/{name}/.pytest_cache")
        time.sleep(1)
    return caches


# alice's idle processes, as on a desktop (the development laptop runs
# about 1,600): each scan reads every one. 1,500 made a plain cycle's scans
# slower than the 1 s freshness bound under host load (candidates kept
# `in_use_unknown`, stale), measured; 1,000 did not.
SLEEPERS = 1000
STORM = (
    "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do "
    "(while :; do /run/current-system/sw/bin/true; done) & "
    "done; wait"
)


def storm(name: str) -> None:
    """A fork storm of alice's: twelve loops starting processes, given 5 s
    to be in full swing (a cycle started at once can finish before it
    is: measured, 0.1 s). With alice's SLEEPERS, a scan takes long enough
    that every listing finds new processes: unsettled, a busy host (on a
    guest of a hundred processes the same storm let every scan settle,
    and so did processes whose thread listing kept changing)."""
    machine.succeed(f"systemd-run --uid=alice --unit={name} /bin/sh -c {shlex.quote(STORM)}")
    time.sleep(5)


TOO_BUSY = "the host is too busy to judge candidates"


# alice's ensure of 0.2's ccache, its cleaner a slow stand-in on PATH
# (diskgc trusts any program of that name: docs/safety.md, Command
# actions): started under the host lock, it holds the lock it inherits
# until the test releases it.
ENSURE_CCACHE = (
    "/usr/local/bin/diskgc ensure --json --no-index --min-free 100%"
    " --select profile:ccache --roots /home/alice /home/alice"
)
SLOW_CCACHE = """#!/run/current-system/sw/bin/sh
: > /home/alice/lock-turns/started
i=0
while [ ! -e /home/alice/lock-turns/release ] && [ $i -lt 3000 ]; do /run/current-system/sw/bin/sleep 0.1; i=$((i+1)); done
/run/current-system/sw/bin/rm -f -- "$CCACHE_DIR/payload"
"""


def calm(name: str) -> None:
    machine.succeed(f"systemctl stop {name}")


machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("user@1000.service")

with subtest("install the release package as a generic-Linux user would"):
    machine.succeed("test -f /run/diskgc/lock")
    machine.succeed(f"install -Dm0755 {RELEASE}/bin/diskgc /usr/local/bin/diskgc")
    version = machine.succeed("/usr/local/bin/diskgc --version").strip()
    assert version.startswith(f"diskgc {VERSION}"), version
    helped = machine.succeed("/usr/local/bin/diskgc --help")
    assert "diskgc watch --min-free SIZE --target-free SIZE" in helped, helped
    # /srv/alice gets inodes for the tally's many-file tree below.
    for disk, mount, owner, inodes in (
        ("/dev/vdb", "/srv/alice", "alice", 100000),
        ("/dev/vdc", "/srv/ci", "runner", 16384),
    ):
        machine.succeed(
            f"mkfs.ext4 -q -N {inodes} {disk}", f"mkdir -p {mount}", f"mount {disk} {mount}",
            f"chown {owner}:users {mount}",
        )
    # The user template, by alice herself (no root needed).
    su_ok(
        "alice",
        f"install -Dm0644 {RELEASE}/lib/systemd/user/diskgc-watch@.service"
        " ~/.config/systemd/user/diskgc-watch@.service",
    )
    machine.succeed(
        f"cmp {RELEASE}/lib/systemd/user/diskgc-watch@.service"
        " /home/alice/.config/systemd/user/diskgc-watch@.service"
    )
    # The system template: /run/systemd/system, since NixOS's
    # /etc/systemd/system is read-only.
    machine.succeed(
        f"install -Dm0644 {RELEASE}/lib/systemd/system/diskgc-watch@.service"
        " /run/systemd/system/diskgc-watch@.service",
        f"cmp {RELEASE}/lib/systemd/system/diskgc-watch@.service"
        " /run/systemd/system/diskgc-watch@.service",
    )

with subtest("alice's projects, her configuration, and the user instance"):
    alice_caches = projects("alice", "/srv/alice/src", [f"p-{i:02}" for i in range(12)])
    su_ok(
        "alice",
        "mkdir -p ~/lock-turns ~/stand-ins ~/.cache/ccache"
        " && ln -s /run/current-system/sw/bin/env ~/stand-ins/env"
        f" && printf '%s' {shlex.quote(SLOW_CCACHE)} > ~/stand-ins/ccache && chmod 755 ~/stand-ins/ccache"
        " && fallocate -l 1M ~/.cache/ccache/payload && touch ~/lock-turns/release"
        # The watch's own ccache cache, on its filesystem (a tier whose
        # roots are on another filesystem is skipped, other_fs).
        " && mkdir -p /srv/alice/.cache/ccache && fallocate -l 1M /srv/alice/.cache/ccache/payload",
    )
    # The newest cache holds 30,000 files: sizing it takes the background
    # tally many batches, each after a pressure reading (never deleted
    # here: the phases below take the oldest first).
    su_ok(
        "alice",
        f"mkdir -p {alice_caches[-1]}/v/cache/many && cd {alice_caches[-1]}/v/cache/many"
        " && seq 1 30000 | xargs touch",
    )
    su_ok(
        "alice",
        # First the ccache group (its cleaner the stand-in, on the
        # instance's PATH: the environment file), then the pytest caches.
        "mkdir -p ~/.config/diskgc/watch && printf '%s\\n' 'version: 1' 'groups:'"
        " '  - name: cleaners' '    tiers:' '      - name: ccache'"
        " '        select: profile:ccache' '        roots: [/srv/alice]'"
        " '  - name: caches' '    tiers:' '      - name: pytest-caches'"
        " '        select: profile:pytest-cache' '        roots: [/srv/alice/src]'"
        " > ~/.config/diskgc/config.yaml && /usr/local/bin/diskgc check",
    )
    start = avail("/srv/alice")
    MIN = start - 30 * MIB
    TARGET = MIN + 12 * MIB
    su_ok(
        "alice",
        f"echo 'DISKGC_WATCH_ARGS=--min-free {MIN // 1024}K --target-free {TARGET // 1024}K"
        f" --interval {INTERVAL} --timeout 1m --json' > ~/.config/diskgc/watch/srv-alice.env"
        " && echo 'PATH=/home/alice/stand-ins' >> ~/.config/diskgc/watch/srv-alice.env",
    )
    # p-00, the oldest, is held open by alice's own process.
    machine.succeed(
        "systemd-run --uid=alice --unit=holder /run/current-system/sw/bin/python3 -c "
        + shlex.quote(
            f"import time; f = open('{alice_caches[0]}/v/cache/pad', 'rb'); time.sleep(10**6)"
        )
    )
    machine.wait_until_succeeds(
        f"ls -l /proc/$(systemctl show -p MainPID --value holder)/fd | grep -q {alice_caches[0]}/v/cache/pad"
    )
    machine.succeed(
        "systemd-run --uid=alice --unit=sleepers -p TasksMax=infinity /bin/sh -c "
        # A transient unit's PATH holds no coreutils on NixOS: no `seq`.
        + shlex.quote(
            f"i=0; while [ $i -lt {SLEEPERS} ]; do /run/current-system/sw/bin/sleep 100000 & i=$((i+1)); done; wait"
        )
    )
    def sleeping() -> bool:
        tasks = int(machine.succeed("systemctl show -p TasksCurrent --value sleepers").strip())
        return tasks > SLEEPERS

    wait_for("alice's idle processes", sleeping, 180)
    # A fork storm of alice's from before the start, so that the
    # background tally's first sizing runs under CPU pressure (PSI `cpu
    # some avg10` above 50 %).
    storm("storm-start")
    time.sleep(10)
    machine.succeed(
        "systemctl --user -M alice@ daemon-reload",
        "systemctl --user -M alice@ enable --now diskgc-watch@srv-alice.service",
    )
    UNIT = "diskgc-watch@srv-alice.service"
    wait_for("the user instance started", lambda: "watch /srv/alice: --min-free" in user_journal(UNIT), 60)
    got = show(UNIT, ["MainPID", "ActiveState"], user="alice")
    assert got["ActiveState"] == "active", got
    machine.succeed(f"test \"$(stat -c %U /proc/{got['MainPID']})\" = alice")

with subtest("the background tally brakes under pressure and resumes after it"):
    wait_for(
        "the pressure brake engaged",
        lambda: "background tally: pressure brake engaged" in user_journal(UNIT),
        90,
    )
    calm("storm-start")
    wait_for(
        "the pressure brake released",
        lambda: "background tally: pressure brake released" in user_journal(UNIT),
        150,
    )
    # Above --min-free: no cycle.
    assert records(user_journal(UNIT)) == [], user_journal(UNIT)


def wait_records(n: int, timeout: float = 240) -> list[dict[str, Any]]:
    return wait_for(
        f"{n} record(s) of {UNIT}",
        lambda: (lambda r: r if len(r) >= n else None)(records(user_journal(UNIT))),
        timeout,
    )


# The unheld caches not yet deleted, oldest first.
remaining = alice_caches[1:]


def check_met(record: dict[str, Any]) -> None:
    """A `met` cycle: the held cache kept, the pytest caches it deleted
    taken oldest first (any it passed over are kept with a reason in its
    record, such as `in_use_unknown` on a host too loaded for a scan within
    the 1 s freshness bound), and it stopped at the goal: one deletion
    fewer would not have reached it."""
    global remaining
    assert record["status"] == "met" and record["target_met"] is True, record
    held = [x["reason"] for x in skips(record, alice_caches[0])]
    assert held and held[0] in ("in_use", "in_use_unknown"), record["skipped"]
    deleted = [a for a in record["actions"] if a["tier"] == "pytest-caches"]
    taken = [a["candidate"] for a in deleted]
    assert taken, record
    passed = {x["candidate"] for x in record["skipped"]}
    reached = [c for c in remaining if c in taken or c in passed]
    assert reached == remaining[: len(reached)] and set(taken) <= set(reached), (taken, remaining)
    smallest = min(a["avail_delta_bytes"] for a in deleted)
    after = record["avail_after_bytes"]
    assert after >= TARGET and after - smallest < TARGET, (after, smallest, TARGET)
    for cache in taken:
        machine.succeed(f"test ! -e {cache}")
    remaining = [c for c in remaining if c not in taken]
    machine.succeed(f"test -d {alice_caches[0]}")


with subtest("a fill below --min-free: one cycle reclaims to --target-free, keeps the held cache"):
    fill_to("alice", "/srv/alice", MIN - 3 * MIB, "fill-a")
    first = wait_records(1)[0]
    save("cycle-1.json", json.dumps(first, indent=1))
    assert first["watch"] == {"cycle": 1, "backoff": None}, first
    assert first["exit_code"] is None and first["dry_run"] is False, first
    check_met(first)

def next_record(timeout: float = 240) -> dict[str, Any]:
    """The record after those seen so far."""
    seen = len(records(user_journal(UNIT)))
    return wait_records(seen + 1, timeout)[seen]


def release_cleaner() -> None:
    su_ok("alice", "touch ~/lock-turns/release")


def hold_cleaner(cache: str) -> None:
    """The next run of the stand-in blocks; CACHE has a payload again (an
    empty cache is skipped, `empty`, and its cleaner not run)."""
    su_ok(
        "alice",
        "mv ~/lock-turns/release ~/lock-turns/release.old"
        " && { [ ! -e ~/lock-turns/started ] || mv ~/lock-turns/started ~/lock-turns/started.old; }"
        f" && fallocate -l 1M {cache}/payload",
    )


with subtest("between the thresholds: no cycle, nothing deleted"):
    fill_to("alice", "/srv/alice", MIN + 6 * MIB, "fill-b")
    time.sleep(8)
    assert len(records(user_journal(UNIT))) == 1, user_journal(UNIT)
    for cache in remaining:
        machine.succeed(f"test -d {cache}")

# A busy host's scans are unsettled only while the storm outpaces them,
# which depends on the guest's and the host's timing: here the cycle has
# to wait at least once instead of giving its candidate up. An `ensure`
# beside it is not asserted (in one run the cycle's scan settled within
# seconds and the `ensure` got the lock); the container test
# release_candidate_watch_and_ensure_take_turns_through_the_host_lock shows
# the `ensure`'s 75 beside a cycle that holds the lock deterministically.
with subtest("a busy host during a cycle: it waits instead of giving up, then reclaims"):
    storm("storm-c")
    busy_before = user_journal(UNIT).count(TOO_BUSY)
    seen = len(records(user_journal(UNIT)))
    fill_to("alice", "/srv/alice", MIN - 3 * MIB, "fill-c")
    wait_for(
        "the cycle waits out the storm",
        lambda: user_journal(UNIT).count(TOO_BUSY) > busy_before,
        60,
    )
    calm("storm-c")
    # A cycle the storm held to its deadline is followed, a minute later,
    # by one that reclaims.
    met = wait_for(
        "a cycle that reclaims after the storm",
        lambda: next((r for r in records(user_journal(UNIT))[seen:] if r["status"] == "met"), None),
        240,
    )
    for r in records(user_journal(UNIT))[seen:]:
        assert r["status"] in ("met", "deadline"), r
    save("cycle-busy.json", json.dumps(met, indent=1))
    check_met(met)

with subtest("lock_timeout: a concurrent ensure's cleaner holds the lock past the cycle's deadline"):
    hold_cleaner("/home/alice/.cache/ccache")
    machine.succeed(
        "systemd-run --uid=alice --unit=ensure-d -p RemainAfterExit=yes"
        f" --setenv=PATH=/home/alice/stand-ins {ENSURE_CCACHE} --timeout 300s"
    )
    machine.wait_until_succeeds("test -e /home/alice/lock-turns/started", timeout=60)
    fill_to("alice", "/srv/alice", MIN - 3 * MIB, "fill-d")
    blocked = next_record(150)
    save("cycle-lock-timeout.json", json.dumps(blocked, indent=1))
    assert blocked["status"] == "lock_timeout" and blocked["actions"] == [], blocked
    assert blocked["watch"]["backoff"]["class"] == "retry", blocked
    assert blocked["watch"]["backoff"]["attempt"] == 1, blocked
    assert blocked["watch"]["backoff"]["wait_seconds"] == 60, blocked
    machine.succeed("test -f /home/alice/.cache/ccache/payload")
    release_cleaner()
    machine.wait_until_succeeds(
        "test \"$(systemctl show -p ExecMainExitTimestampMonotonic --value ensure-d)\" != 0",
        timeout=120,
    )
    ensured = show("ensure-d.service", ["ExecMainStatus"])
    save("ensure-d.journal", system_journal("ensure-d.service", "short-iso-precise"))
    # 100 % is never met: the ensure ran its cleaner and is exhausted.
    assert ensured["ExecMainStatus"] == "1", ensured
    machine.succeed("test ! -e /home/alice/.cache/ccache/payload")
    after_lock = next_record(150)
    save("cycle-after-lock.json", json.dumps(after_lock, indent=1))
    assert after_lock["watch"]["backoff"] is None, after_lock
    check_met(after_lock)
    gap = (started_at(after_lock) - ended_at(blocked)).total_seconds()
    assert 55 <= gap <= 75, f"retry back-off of 60 s, got {gap:.1f} s"

with subtest("deadline: the cycle's own cleaner runs past its --timeout; the next cycle reclaims"):
    hold_cleaner("/srv/alice/.cache/ccache")
    fill_to("alice", "/srv/alice", MIN - 3 * MIB, "fill-e")
    timed_out = next_record(150)
    save("cycle-deadline.json", json.dumps(timed_out, indent=1))
    machine.succeed("test -e /home/alice/lock-turns/started")
    assert timed_out["status"] == "deadline", timed_out
    assert timed_out["watch"]["backoff"]["class"] == "retry", timed_out
    assert timed_out["watch"]["backoff"]["attempt"] == 1, timed_out
    # The cleaner was stopped at the deadline (it removes the payload only
    # when released), and the later group, the pytest caches, never ran.
    machine.succeed("test -f /srv/alice/.cache/ccache/payload")
    assert [a for a in timed_out["actions"] if a["tier"] == "pytest-caches"] == [], timed_out
    for cache in remaining:
        machine.succeed(f"test -d {cache}")
    release_cleaner()
    after_deadline = next_record(150)
    save("cycle-after-deadline.json", json.dumps(after_deadline, indent=1))
    check_met(after_deadline)
    gap = (started_at(after_deadline) - ended_at(timed_out)).total_seconds()
    assert 55 <= gap <= 75, f"retry back-off of 60 s, got {gap:.1f} s"

with subtest("SIGHUP: exit 0, restarted after 30 s from its snapshot"):
    before = show(UNIT, ["MainPID", "NRestarts"], user="alice")
    machine.succeed(f"systemctl --user -M alice@ kill -s HUP --kill-whom=main {UNIT}")
    wait_for(
        "diskgc stopped on SIGHUP",
        lambda: "stopping on SIGHUP: diskgc does not reload" in user_journal(UNIT),
        30,
    )
    # Exit 0 (Restart=always would restart a failure too), while the
    # restart waits its 30 s.
    stopped = wait_for(
        "the stop's exit status",
        lambda: (lambda g: g if g["SubState"] == "auto-restart" else None)(
            show(UNIT, ["SubState", "ExecMainStatus", "Result"], user="alice")
        ),
        30,
    )
    assert stopped["ExecMainStatus"] == "0" and stopped["Result"] == "success", stopped
    wait_for(
        "the restart",
        lambda: (lambda g: g if g["NRestarts"] == "1" and g["ActiveState"] == "active" else None)(
            show(UNIT, ["MainPID", "NRestarts", "ActiveState"], user="alice")
        ),
        60,
    )
    after = show(UNIT, ["MainPID"], user="alice")
    assert after["MainPID"] != before["MainPID"], (before, after)
    wait_for(
        "the restarted watcher starts from the snapshot",
        lambda: user_journal(UNIT).count("index: started from snapshot") >= 1,
        30,
    )

with subtest("exit 2 at start is not restarted"):
    su_ok("alice", "echo 'DISKGC_WATCH_ARGS=--no-such-flag' > ~/.config/diskgc/watch/srv-alice.env")
    machine.succeed(f"systemctl --user -M alice@ kill -s HUP --kill-whom=main {UNIT}")
    machine.wait_until_succeeds(f"systemctl --user -M alice@ is-failed {UNIT}", timeout=90)
    got = show(UNIT, ["ExecMainStatus", "NRestarts", "Result"], user="alice")
    assert got == {"ExecMainStatus": "2", "NRestarts": "2", "Result": "exit-code"}, got
    # diskgc's own message, in the journal. Matched by the process's pid:
    # a process that exits at once can lose its unit field to a journald
    # race (seen here: present on the console, absent under
    # _SYSTEMD_USER_UNIT), so `systemctl status` may not show it.
    pid = show(UNIT, ["ExecMainPID"], user="alice")["ExecMainPID"]
    wait_for(
        "the refusal in the journal",
        lambda: "unknown flag --no-such-flag"
        in machine.succeed(f"journalctl --no-pager -o cat _PID={pid}"),
        30,
    )
    print(machine.execute(f"systemctl --user -M alice@ status --no-pager -n 20 {UNIT}")[1])
    save("user-instance.journal", user_journal(UNIT, "short-iso-precise"))

with subtest("the system template as runner: a fill it reclaims, then systemctl stop"):
    ci_caches = projects("runner", "/srv/ci/src", [f"c-{i:02}" for i in range(4)])
    ci_start = avail("/srv/ci")
    ci_min = ci_start - 20 * MIB
    ci_target = ci_min + 12 * MIB
    machine.succeed(
        "mkdir -p /etc/diskgc/watch /run/systemd/system/diskgc-watch@srv-ci.service.d",
        "printf '%s\\n' 'version: 1' 'groups:' '  - name: caches' '    tiers:'"
        " '      - name: pytest-caches' '        select: profile:pytest-cache'"
        " '        roots: [/srv/ci/src]' > /etc/diskgc/config.yaml",
        f"echo 'DISKGC_WATCH_ARGS=--min-free {ci_min // 1024}K --target-free {ci_target // 1024}K"
        f" --interval {INTERVAL} --json' > /etc/diskgc/watch/srv-ci.env",
        "printf '[Service]\\nUser=runner\\n'"
        " > /run/systemd/system/diskgc-watch@srv-ci.service.d/user.conf",
        "/usr/local/bin/diskgc check --config /etc/diskgc/config.yaml",
        "systemctl daemon-reload",
        "systemctl enable --runtime --now diskgc-watch@srv-ci.service",
    )
    CI = "diskgc-watch@srv-ci.service"
    wait_for("the system instance started", lambda: "watch /srv/ci: --min-free" in system_journal(CI), 60)
    got = show(CI, ["MainPID", "User", "CapabilityBoundingSet"])
    assert got["User"] == "runner" and got["CapabilityBoundingSet"] == "", got
    machine.succeed(f"test \"$(stat -c %U /proc/{got['MainPID']})\" = runner")
    machine.succeed(
        "su - runner -c " + shlex.quote(f"fallocate -l {avail('/srv/ci') - (ci_min - 3 * MIB)} /srv/ci/fill")
    )
    ci_first = wait_for(
        "the system instance's cycle", lambda: records(system_journal(CI)), 120
    )[0]
    save("ci-cycle-1.json", json.dumps(ci_first, indent=1))
    assert ci_first["status"] == "met", ci_first
    assert actions(ci_first) == ci_caches[0:2], actions(ci_first)
    machine.succeed("systemctl stop diskgc-watch@srv-ci.service")
    got = show(CI, ["ActiveState", "Result", "ExecMainStatus", "NRestarts"])
    assert got == {"ActiveState": "inactive", "Result": "success", "ExecMainStatus": "0", "NRestarts": "0"}, got
    ci_log = system_journal(CI)
    save("system-instance.journal", system_journal(CI, "short-iso-precise"))
    assert "stopping on SIGTERM" in ci_log, ci_log
    machine.succeed("ls /home/runner/.local/state/diskgc/index-* >&2")
