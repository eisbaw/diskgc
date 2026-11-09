# Driver script of vm-nixos-module (vm-test.nix), after the prelude that
# defines DEV (the development package). Unit names are the escaped
# watched paths: /home/runner -> home-runner, / -> -.
import json
import shlex
import time

MIB = 1 << 20

def show(unit, props, user=None):
    """systemctl show values of UNIT, empty ones included, as a dict
    (user=NAME: that user's manager)."""
    manager = f"--user -M {user}@ " if user else ""
    out = machine.succeed(
        f"systemctl {manager}show --all {unit} " + " ".join(f"-p {p}" for p in props)
    )
    return dict(line.split("=", 1) for line in out.strip().splitlines())


def journal(unit, user=False):
    field = "_SYSTEMD_USER_UNIT" if user else "_SYSTEMD_UNIT"
    return machine.succeed(f"journalctl --no-pager -o cat {field}={unit}")


# Hardening properties the units must not set (docs/architecture.md,
# Hardening: the "no" and "fatal" rows), as systemd reports them.
UNSET = {
    "ProtectProc": "default", "ProcSubset": "all", "PrivatePIDs": "no",
    "PrivateNetwork": "no", "RemoveIPC": "no", "DynamicUser": "no",
    "PrivateUsers": "no", "PrivateTmp": "no", "ProtectHome": "no",
    "ProtectSystem": "no",
}


def assert_not_namespaced(unit, user=None):
    got = show(unit, list(UNSET), user)
    print(unit, got)
    assert got == UNSET, (unit, got)


def wait_failed_exit_2(unit, user=None):
    manager = f"--user -M {user}@ " if user else ""
    machine.wait_until_succeeds(f"systemctl {manager}is-failed {unit}")
    got = show(unit, ["ExecMainStatus", "NRestarts", "Result"], user)
    assert got == {"ExecMainStatus": "2", "NRestarts": "0", "Result": "exit-code"}, (unit, got)


machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("user@1000.service")

with subtest("before the module: no diskgc unit, no host lock"):
    machine.fail("systemctl cat 'diskgc-watch@.service'")
    machine.fail("test -e /run/diskgc/lock")

with subtest("first switch: the host lock and the system instances"):
    machine.succeed(
        "mkdir -p /etc/diskgc/watch",
        "echo 'DISKGC_WATCH_ARGS=--stale-flag' > /etc/diskgc/watch/home-runner.env",
    )
    # The two refused system instances fail at start. Whether the switch
    # sees that is a race (Type=exec: the switch only notices a unit that
    # failed within its short wait), so it exits 0, or 4 naming some of
    # them and nothing else.
    status, out = machine.execute(
        "/run/current-system/specialisation/diskgc/bin/switch-to-configuration test 2>&1"
    )
    print(out)
    assert status in (0, 4), status
    if status == 4:
        line = next(l for l in out.splitlines() if "the following units failed:" in l)
        failed = set(line.split("the following units failed:", 1)[1].strip().split(", "))
        refused = {"diskgc-watch@nix-store.service", "diskgc-watch@var.service"}
        assert failed and failed <= refused, failed
    machine.succeed("test -f /run/diskgc/lock")
    machine.succeed("test \"$(stat -c '%a %U' /etc/diskgc/config.yaml)\" = '644 root'")
    machine.succeed("test ! -L /etc/diskgc/config.yaml")
    machine.wait_for_unit("diskgc-watch@home-runner.service")
    machine.wait_for_unit("diskgc-watch@-.service")
    got = show("diskgc-watch@home-runner.service", ["User", "MainPID"])
    machine.succeed(f"test \"$(stat -c %U /proc/{got['MainPID']})\" = runner")

with subtest("root without --allow-root and --allow-root without CAP_SYS_PTRACE exit 2"):
    wait_failed_exit_2("diskgc-watch@nix-store.service")
    log = journal("diskgc-watch@nix-store.service")
    print(log)
    assert "refuses to run as root" in log and "--allow-root" in log, log
    wait_failed_exit_2("diskgc-watch@var.service")
    log = journal("diskgc-watch@var.service")
    print(log)
    assert "CAP_SYS_PTRACE" in log, log

with subtest("hardening and capability sets as systemd reports them"):
    runner = show(
        "diskgc-watch@home-runner.service",
        ["CapabilityBoundingSet", "NoNewPrivileges", "RestrictNamespaces",
         "RestrictSUIDSGID", "KillMode", "RestartPreventExitStatus", "MainPID"],
    )
    print(runner)
    assert runner["CapabilityBoundingSet"] == "", runner
    assert runner["NoNewPrivileges"] == "yes", runner
    assert runner["RestrictNamespaces"] == "yes", runner
    assert runner["RestrictSUIDSGID"] == "no", runner
    assert runner["KillMode"] == "mixed", runner
    assert runner["RestartPreventExitStatus"] == "2", runner
    # The drop-in resets EnvironmentFile=: the stale .env file left from
    # a generic-Linux install (made before the switch) is not read.
    machine.fail(f"grep -qz DISKGC_WATCH_ARGS /proc/{runner['MainPID']}/environ")
    assert_not_namespaced("diskgc-watch@home-runner.service")
    assert_not_namespaced("diskgc-watch@-.service")
    root = show("diskgc-watch@-.service", ["CapabilityBoundingSet", "User"])
    print(root)
    assert set(root["CapabilityBoundingSet"].split()) == {
        "cap_dac_override", "cap_fowner", "cap_sys_ptrace", "cap_checkpoint_restore"
    }, root

with subtest("the first switch starts the lingering user's instances; one per filesystem"):
    # The switch reloads each running user manager, which starts the user
    # units newly wanted by default.target (measured here, nixpkgs 26.11).
    # home (/home) and second (/var/tmp) are both mark's, both on the root
    # filesystem, started together: one holds the single-writer lock, the
    # other exits 2 after its wait, whichever it is.
    def state(unit):
        return show(unit, ["ActiveState", "ExecMainStatus", "NRestarts", "MainPID"], user="mark")

    pair = ["diskgc-watch@home.service", "diskgc-watch@var-tmp.service"]
    machine.wait_until_succeeds(
        " || ".join(f"systemctl --user -M mark@ is-failed {u}" for u in pair)
    )
    got = sorted((state(u) for u in pair), key=lambda s: s["ActiveState"])
    print(got)
    assert got[0]["ActiveState"] == "active", got
    assert {k: got[1][k] for k in ("ActiveState", "ExecMainStatus", "NRestarts")} == {
        "ActiveState": "failed", "ExecMainStatus": "2", "NRestarts": "0"
    }, got
    loser = next(u for u in pair if state(u)["ActiveState"] == "failed")
    log = journal(loser, user=True)
    print(log)
    assert "another watch holds index-" in log, log
    winner = next(u for u in pair if u != loser)
    assert_not_namespaced(winner, user="mark")
    pid = got[0]["MainPID"]
    machine.succeed(f"test \"$(stat -c %U /proc/{pid})\" = mark")
    # Its environment: the user manager's PATH, not NixOS's service PATH.
    env = machine.succeed(f"tr '\\0' '\\n' < /proc/{pid}/environ")
    print(env)
    assert "/run/current-system/sw/bin" in env, env

with subtest("the development package refuses a reclaiming watch here (sandbox gate)"):
    status, out = machine.execute(
        f"su - mark -c '{DEV}/bin/diskgc watch --min-free 1M --target-free 2M /tmp' 2>&1"
    )
    print(out)
    assert status == 2 and "sandbox" in out, (status, out)


def records(unit, user=False):
    lines = journal(unit, user=user).splitlines()
    return [json.loads(l) for l in lines if l.startswith("{") and '"watch":' in l]


with subtest("mark's reclaiming instance: a fill reclaims to --target-free, keeps a held cache"):
    unit = "diskgc-watch@srv-mark.service"
    got = show(unit, ["ActiveState", "MainPID"], user="mark")
    assert got["ActiveState"] == "active", got
    caches = []
    for i in range(8):
        name = f"/srv/mark/src/p-{i:02}"
        machine.succeed(
            "su - mark -c " + shlex.quote(
                f"mkdir -p {name} && cd {name}"
                " && printf 'def test_ok():\\n    assert True\\n' > test_ok.py"
                " && pytest -q >/dev/null && mkdir -p .pytest_cache/v/cache"
                " && fallocate -l 8M .pytest_cache/v/cache/pad"
            )
        )
        caches.append(f"{name}/.pytest_cache")
        time.sleep(1)
    machine.succeed(
        "systemd-run --uid=mark --unit=holder /run/current-system/sw/bin/python3 -c "
        + shlex.quote(f"import time; f = open('{caches[0]}/v/cache/pad', 'rb'); time.sleep(10**6)")
    )
    machine.wait_until_succeeds(
        f"ls -l /proc/$(systemctl show -p MainPID --value holder)/fd | grep -q {caches[0]}"
    )
    blocks, size, free = machine.succeed("stat -f -c '%b %S %a' /srv/mark").split()
    total = int(blocks) * int(size)
    minimum, target = total * 40 // 100, total * 50 // 100
    avail = int(free) * int(size)
    assert avail > minimum + 3 * MIB, (avail, minimum)
    assert records(unit, user=True) == [], "a cycle above --min-free"
    machine.succeed(
        "su - mark -c " + shlex.quote(f"fallocate -l {avail - (minimum - 3 * MIB)} /srv/mark/fill")
    )
    machine.wait_until_succeeds(
        f"journalctl -o cat _SYSTEMD_USER_UNIT={unit} | grep -q '\"watch\":'", timeout=120
    )
    first = records(unit, user=True)[0]
    print(json.dumps(first, indent=1))
    assert first["status"] == "met" and first["watch"]["cycle"] == 1, first
    taken = sorted(a["candidate"] for a in first["actions"])
    assert taken and taken == caches[1 : 1 + len(taken)] and len(taken) < 7, taken
    held = [s["reason"] for s in first["skipped"] if s["candidate"] == caches[0]]
    assert held == ["in_use"], first["skipped"]
    after = int(machine.succeed("stat -f -c '%a %S' /srv/mark").split()[0]) * int(size)
    # Stopped at the goal: one deletion fewer would not have reached it.
    smallest = min(a["avail_delta_bytes"] for a in first["actions"])
    assert target <= after and after - smallest < target, (after, smallest, target)
    for cache in caches[1 + len(taken) :] + [caches[0]]:
        machine.succeed(f"test -d {cache}")
    machine.succeed("systemctl stop holder")

with subtest("the tally-only snapshot lands in the runner's state directory"):
    machine.succeed("systemctl stop diskgc-watch@home-runner.service")
    print(journal("diskgc-watch@home-runner.service"))
    machine.succeed("ls /home/runner/.local/state/diskgc/index-* >&2")
    machine.succeed("systemctl start diskgc-watch@home-runner.service")
    machine.wait_for_unit("diskgc-watch@home-runner.service")
    machine.wait_until_succeeds(
        "journalctl -o cat _SYSTEMD_UNIT=diskgc-watch@home-runner.service"
        " | grep -q 'index: started from snapshot'"
    )
    # The restarted watcher started from it (above). Whether the runner's
    # own ensure does is not shown here: this configuration's only tier is
    # a Cargo one, and the index holds fs tiers only (a Cargo tier is
    # discovered afresh by every run), so neither reads the snapshot's
    # units for it.

with subtest("SIGHUP restarts an instance, which reads its configuration afresh"):
    # An edited (here: invalid) configuration takes effect at the restart:
    # exit 0 on SIGHUP, restart after RestartSec=30s, exit 2 on the new
    # configuration, and no further restart.
    machine.succeed("echo 'version: 1\nno_such_key: 1' > /etc/diskgc/config.yaml")
    machine.succeed("systemctl kill -s HUP --kill-whom=main diskgc-watch@home-runner.service")
    # diskgc's own clean stop, before the restart 30 s later.
    machine.wait_until_succeeds(
        "journalctl -o cat _SYSTEMD_UNIT=diskgc-watch@home-runner.service"
        " | grep -q 'stopping on SIGHUP'"
    )
    # Exit 0, then Restart=always waits RestartSec=30s.
    machine.wait_until_succeeds(
        "systemctl show -p SubState --value diskgc-watch@home-runner.service | grep -qx auto-restart"
    )
    machine.wait_until_succeeds(
        "systemctl is-failed diskgc-watch@home-runner.service"
    )
    got = show("diskgc-watch@home-runner.service", ["ExecMainStatus", "NRestarts"])
    assert got == {"ExecMainStatus": "2", "NRestarts": "1"}, got
    log = journal("diskgc-watch@home-runner.service")
    print(log)
    assert "no_such_key" in log, log
