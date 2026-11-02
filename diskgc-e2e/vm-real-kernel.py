# vm-real-kernel (TASK-304) test script, appended to the prelude in
# vm-real-kernel.nix that defines IMAGE, IMAGE_REF, UNDER_TEST, TOOLS,
# LIVE_ISO, BCACHEFS_TOOLS_VERSION and KERNEL_VERSION. Runs in the NixOS
# test driver.
#
# Every diskgc run is the development package inside the guest's one
# privileged Podman container (see the .nix file for why). Each scenario
# makes its own mounts and loop devices, starts with no loop device
# configured (a loop device is seen by every scan, so one left behind would
# change the next scenario's verdicts), and ends with the deletion oracle:
# the fixture trees listed before and after each reclaim, by path, type and
# inode, and exactly the expected candidates' trees gone.
#
# Every diskgc command, exit code, stdout (the JSON report), stderr and, where
# taken, its ioctl strace are saved under $out/evidence and printed to the
# build log. The expectations cite docs/safety.md and docs/architecture.md
# by section (rule names, not line numbers).
import json
import shlex
import time
from typing import Any

CTR = "diskgc-real-kernel"
BIN = "/opt/diskgc-e2e/bin"
DISKGC = f"{BIN}/diskgc-development"
ROOT = "0:0"
TESTER = "1000:1000"
ALICE = "1002:100"  # alice, group users: like tester, not in disk
HOMES = {ROOT: "/root", TESTER: "/home/tester", ALICE: "/home/alice"}
UNREACHABLE = "100%"  # never met: reclaim every eligible candidate (exit 1)
PATH_ENV = f"{TOOLS}/bin:{TOOLS}/sbin:/bin"
EV = "/work/ev"  # container scratch for stderr and strace files
PROFILE = "scratch-probe"

evidence_dir = machine.out_dir / "evidence"
evidence_dir.mkdir(parents=True, exist_ok=True)
started_monotonic = time.monotonic()
summary: list[str] = []


def save(name: str, text: str) -> None:
    (evidence_dir / name).write_text(text)
    print(f"===== {name} =====\n{text}")


def cx(command: str, user: str = ROOT, timeout: int = 300, detach: bool = False) -> tuple[int, str]:
    """COMMAND by /bin/sh in the container as USER (uid:gid)."""
    env = {"PATH": PATH_ENV, "HOME": HOMES[user]}
    flags = " ".join(f"-e {k}={shlex.quote(v)}" for k, v in env.items())
    detached = "--detach " if detach else ""
    return machine.execute(
        f"podman exec {detached}--user {user} {flags} {CTR} /bin/sh -c {shlex.quote(command)}",
        timeout=timeout,
    )


def run(command: str, user: str = ROOT, timeout: int = 300) -> str:
    code, out = cx(f"exec 2>&1\nset -e\n{command}", user, timeout)
    assert code == 0, f"as {user} in the container, exit {code}:\n{command}\n--- output ---\n{out}"
    return out


def diskgc(tag: str, user: str, args: str, strace: bool = False, trace: str = "ioctl") -> tuple[int, str, str]:
    """diskgc ARGS as USER in the container; returns exit code, stdout, stderr."""
    err = f"{EV}/{tag}.stderr"
    trace_file = f"{EV}/{tag}.strace"
    # --seccomp-bpf stops only at ioctl; plain -f stops at every syscall and
    # slows the in-use scan past its freshness bound (in_use_unknown: busy).
    prefix = f"strace --seccomp-bpf -f -qq -v -e trace={trace} -o {trace_file} " if strace else ""
    command = f"{prefix}{DISKGC} {args}"
    began = time.monotonic()
    code, out = cx(f"{command} 2>{err}", user)
    elapsed = time.monotonic() - began
    save(f"{tag}.cmd", f"user: {user}\ncommand: {command}\nexit: {code}\nseconds: {elapsed:.1f}\n")
    save(f"{tag}.stdout", out)
    stderr = run(f"cat {err}")
    save(f"{tag}.stderr", stderr)
    if strace:
        save(f"{tag}.strace", run(f"cat {trace_file}"))
    return code, out, stderr


def report(out: str, code: int, status: str) -> dict[str, Any]:
    result: dict[str, Any] = json.loads(out)
    assert result["exit_code"] == code, (result["exit_code"], code, result["errors"])
    assert result["status"] == status, (result["status"], status, result["errors"])
    return result


def ensure(tag: str, user: str, config: str, path: str, extra: str = "",
           min_free: str = UNREACHABLE, strace: bool = False, trace: str = "ioctl") -> tuple[int, str, str]:
    allow_root = "--allow-root " if user == ROOT else ""
    return diskgc(
        tag, user,
        f"ensure {allow_root}--json --config {config} --select profile:{PROFILE} "
        f"--min-free {min_free} {extra} {path}",
        strace, trace,
    )


def subvols_seen(tag: str, names: list[str]) -> dict[str, set[str]]:
    """The stx_subvol values diskgc's statx calls returned for each of
    NAMES (the last path component a call named), from its strace."""
    import re
    seen: dict[str, set[str]] = {name: set() for name in names}
    for line in (evidence_dir / f"{tag}.strace").read_text().splitlines():
        call = re.search(r'statx\(\d+, "([^"]*)",.*stx_subvol=(0x[0-9a-f]+|\d+)', line)
        if call and call.group(1).rsplit("/", 1)[-1] in seen:
            seen[call.group(1).rsplit("/", 1)[-1]].add(call.group(2))
    return seen


def outcomes(result: dict[str, Any]) -> dict[str, tuple[str, str | None]]:
    """Each skipped candidate's reason and unknown_cause (one row each)."""
    rows: dict[str, tuple[str, str | None]] = {}
    for row in result["skipped"]:
        assert row["count"] == 1 and row["candidate"] is not None, row
        assert row["candidate"] not in rows, row
        rows[row["candidate"]] = (row["reason"], row["unknown_cause"])
    return rows


def row_of(result: dict[str, Any], candidate: str) -> dict[str, Any]:
    rows = [r for r in result["skipped"] if r["candidate"] == candidate]
    assert len(rows) == 1, (candidate, result["skipped"])
    return rows[0]


def plan_of(result: dict[str, Any]) -> dict[str, str | None]:
    return {r["candidate"]: r["reason"] for r in result["plan"]}


def acted(result: dict[str, Any]) -> set[str]:
    return {a["candidate"] for a in result["actions"]}


def tree(*roots: str) -> dict[str, str]:
    """Every entry below ROOTS, crossing subvolumes and mounts: path -> type and inode."""
    out = run("find " + " ".join(roots) + " -printf '%y %D:%i %p\\n'")
    entries: dict[str, str] = {}
    for line in out.splitlines():
        kind, ident, path = line.split(" ", 2)
        entries[path] = f"{kind} {ident}"
    return entries


def require_removed(before: dict[str, str], after: dict[str, str], candidates: set[str], tag: str) -> None:
    """The deletion oracle: exactly the candidates' trees are gone, nothing else changed."""
    for candidate in candidates:
        assert candidate in before, (tag, candidate)
    expected = {p for p in before if any(p == c or p.startswith(c + "/") for c in candidates)}
    removed = {p for p in before if after.get(p) != before[p]}
    added = sorted(set(after) - set(before))
    save(
        f"{tag}.oracle",
        f"removed ({len(removed)}):\n" + "\n".join(sorted(removed))
        + f"\nexpected ({len(expected)}):\n" + "\n".join(sorted(expected))
        + f"\nadded ({len(added)}):\n" + "\n".join(added) + "\n",
    )
    assert removed == expected, (tag, sorted(removed - expected), sorted(expected - removed))
    assert not added, (tag, added)


def config(name: str, roots: list[str], extra: str = "", owners: bool = False) -> str:
    """A root-owned configuration with the scratch probe over ROOTS."""
    path = f"/work/cfg/{name}.yaml"
    text = (
        "version: 1\n"
        + ("owners: [{uid: 1000}]\n" if owners else "")
        + "profiles:\n"
        + f"  - name: {PROFILE}\n"
        + f"    find: {{roots: [{', '.join(roots)}], paths: ['scratch-*'], type: dir}}\n"
        + "    action: delete\n"
        + f"    examples: {{match: [{roots[0]}/scratch-a]}}\n"
        + extra
    )
    run(f"cat > {path} <<'EOF'\n{text}EOF\nchmod 0644 {path}")
    save(f"config-{name}.yaml", text)
    return path


def no_loop_devices(tag: str) -> None:
    listing = run("losetup -a -l -n")
    assert listing.strip() == "", (tag, listing)


def loop_backing(device: str) -> str:
    name = device.removeprefix("/dev/")
    return run(f"cat /sys/block/{name}/loop/backing_file").rstrip("\n")


def strace_calls(tag: str, request: str) -> list[str]:
    text = (evidence_dir / f"{tag}.strace").read_text()
    return [line for line in text.splitlines() if "ioctl(" in line and request in line]


def passed(scenario: str, detail: str) -> None:
    line = f"PASS {scenario}: {detail}"
    summary.append(line)
    print(line)


def cleanup(scenario: str) -> None:
    """Undo what a scenario mounted and attached, whether it passed or not."""
    run("mount -o remount,hidepid=off /proc")
    cx("awk '{print $5}' /proc/self/mountinfo | grep -E '^/(mnt/|live|rofs|cdrom)' | sort -r "
       "| while read -r m; do umount -l \"$m\"; done; losetup -D")
    try:
        machine.wait_until_succeeds(
            f"podman exec {CTR} /bin/sh -c 'test -z \"$({TOOLS}/bin/losetup -a)\"'", timeout=30
        )
    except Exception:
        save(f"cleanup-{scenario}.txt", run("losetup -a -l; cat /proc/self/mountinfo"))
        raise


def kernel_threads() -> list[str]:
    """Pids with PF_KTHREAD in their stat flags (field 9), read as root."""
    pids = []
    for line in run("cat /proc/[0-9]*/stat 2>/dev/null || true").splitlines():
        pid = line.split(" ", 1)[0]
        fields = line.rsplit(")", 1)[-1].split()
        if len(fields) > 6 and int(fields[6]) & 0x0020_0000:
            pids.append(pid)
    return pids


def kernel_thread_facts(pids: list[str]) -> str:
    """What a kernel thread shows the in-use scan (TASK-306 premise): its
    environ's error, its fd table, memory map, cwd and root, its PPid and
    status Kthread line, and an unprivileged reader's view of its stat."""
    rows = run(
        f"for p in {' '.join(pids)}; do "
        "if cat /proc/$p/environ >/dev/null 2>/work/ev/kt.err; then e=ok; else e=$(cat /work/ev/kt.err); fi; "
        "echo \"$p|$e|$(ls /proc/$p/fd 2>&1 | wc -l)|$(wc -c </proc/$p/maps)|$(readlink /proc/$p/cwd)|"
        "$(readlink /proc/$p/root)|$(awk '/^PPid/{print $2}' /proc/$p/status)|$(awk '/^Kthread/{print $2}' /proc/$p/status)\"; "
        "done"
    ).splitlines()
    environ_errors: dict[str, int] = {}
    with_fds, with_maps, odd_dirs, not_kthreadd, status_disagrees = [], [], [], [], []
    for row in rows:
        pid, error, fds, maps, cwd, root_dir, ppid, kthread = row.split("|")
        message = error.rsplit(": ", 1)[-1]
        environ_errors[message] = environ_errors.get(message, 0) + 1
        if fds != "0":
            with_fds.append(pid)
        if maps != "0":
            with_maps.append(pid)
        if (cwd, root_dir) != ("/", "/"):
            odd_dirs.append(f"{pid}:{cwd}:{root_dir}")
        if ppid != "2" and pid != "2":
            not_kthreadd.append(pid)
        if kthread != "1":
            status_disagrees.append(pid)
    unprivileged = run(f"cut -d' ' -f1 /proc/{pids[0]}/stat >/dev/null && echo readable", TESTER).strip()
    facts = (
        f"kernel threads: {len(pids)}\nenviron as root: {environ_errors}\n"
        f"with fds: {with_fds}\nwith a memory map: {with_maps}\ncwd/root other than /: {odd_dirs}\n"
        f"not children of kthreadd: {not_kthreadd}\nstatus Kthread not 1: {status_disagrees}\n"
        f"stat of pid {pids[0]} as tester: {unprivileged}\n"
    )
    # The premise of TASK-306's rule, asserted: kernel threads hold no fd,
    # memory map, cwd or root a user file could be, and their environ says
    # ESRCH on this kernel; their stat is readable unprivileged.
    assert pids and environ_errors == {"No such process": len(pids)}, facts
    assert not with_fds and not with_maps and not odd_dirs, facts
    assert not not_kthreadd and not status_disagrees, facts
    assert unprivileged == "readable", facts
    return facts


# ---- The guest and its one container ----
machine.start()
machine.wait_for_unit("multi-user.target")
assert "diskgc.e2e.real_kernel=1" in machine.succeed("cat /proc/cmdline").split()
# Outside a container the interlock refuses (docs/testing.md, Safety interlock).
assert machine.execute(f"{UNDER_TEST}/bin/diskgc-e2e-canary")[0] == 86
machine.succeed(f"{IMAGE} | podman load")
machine.succeed("install -d -m 0755 /var/lib/diskgc-real-kernel")
machine.succeed(
    "podman run --detach --rm --pull=never --network=none --privileged --pid=host "
    f"--name {CTR} --volume /nix/store:/nix/store:ro --volume {UNDER_TEST}/bin:{BIN}:ro "
    "--volume /dev:/dev --volume /var/lib/diskgc-real-kernel:/work "
    "--volume /home/alice:/home/alice "
    f"--entrypoint /bin/sh {IMAGE_REF} -c 'exec sleep 86400'"
)
# Both markers (safety.md, Pre-release sandbox gate) and the real canary.
run(f"{BIN}/diskgc-e2e-canary")
# The guest's PID namespace (safety.md, In use, Visibility: a process list
# that cannot be complete): the development package's rule passes here.
guest_pidns = machine.succeed("readlink /proc/self/ns/pid").strip()
assert run("readlink /proc/self/ns/pid").strip() == guest_pidns, guest_pidns
assert "hidepid" not in run("findmnt -n -o OPTIONS /proc")
# /run/diskgc/lock as the package's tmpfiles drop-in makes it (--allow-root
# needs it: safety.md, Host lock); tester's home is a tmpfs of its own, as
# the e2e harness mounts there.
run(f"mkdir -p /run/diskgc && : > /run/diskgc/lock && chmod 0644 /run/diskgc/lock && "
    f"install -d -m 1777 {EV} && install -d -m 0755 /work/cfg && "
    "mount -t tmpfs -o size=64m,mode=0700,uid=1000,gid=1000 tmpfs /home/tester")
save("environment.txt", run(
    "uname -r; mount --version; btrfs --version; "
    "grep -E 'btrfs|bcachefs|overlay|squashfs|iso9660' /proc/filesystems | tr '\\n' ' '; echo; "
    f"{DISKGC} --version"
))


# ---- 0. Root's in-use scan on a real kernel, no loop device ----
def scenario_root_scan() -> None:
    # A container's own PID namespace shows no kernel thread; the guest's
    # does. Root's scan must still be young enough to clear a candidate
    # (safety.md, In use, Freshness: a deletion starts on a scan younger
    # than one second). On Linux 6.18 a kernel thread's environ says ESRCH,
    # which the scan once retried for 16 rounds per kernel thread, so every
    # root reclaim ended in_use_unknown, busy (TASK-306).
    probe = run(
        "for f in environ maps cmdline; do if cat /proc/2/$f >/dev/null 2>/work/ev/kt.err; "
        "then rc=0; else rc=$?; fi; echo \"/proc/2/$f exit $rc $(cat /work/ev/kt.err)\"; done"
    )
    save("s0-kthread-proc.txt", probe + kernel_thread_facts(kernel_threads()))
    run("install -d -m 0755 -o 1000 -g 1000 /work/s0")
    run("mkdir /work/s0/scratch-a && echo a > /work/s0/scratch-a/f", TESTER)
    cfg = config("s0-root", ["/work/s0"], owners=True)
    before = tree("/work/s0")
    code, out, _ = ensure("s0-root-dry", ROOT, cfg, "/work/s0", "--dry-run")
    dry_ms = report(out, 0, "dry_run")["duration_ms"]
    code, out, stderr = ensure("s0-root", ROOT, cfg, "/work/s0")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s0"), acted(result), "s0-root")
    assert acted(result) == {"/work/s0/scratch-a"}, result["skipped"]
    unreadable = [line for line in stderr.splitlines() if "initial environment unreadable" in line]
    assert not unreadable, unreadable[:3]
    passed("0 root scan", f"an unheld candidate reclaimed by root with every guest process visible; "
           f"dry run {dry_ms} ms, reclaim {result['duration_ms']} ms; no kernel thread's environment noted")


# ---- 1. btrfs: two loop members, subvolumes, snapshot key, membership ----
def scenario_btrfs() -> None:
    devices = run("""
mkdir -p /work/s1/images /work/proj/target/debug/deps /mnt/btrfs-top /mnt/home /mnt/scratch
printf 'Signature: 8a477f597d28d172789f06886806bc55\\n# This file is a cache directory tag created by cargo.\\n' > /work/proj/target/CACHEDIR.TAG
: > /work/proj/target/debug/.cargo-lock
echo dep > /work/proj/target/debug/deps/libdep.rlib
chown -R 1000:1000 /work/proj
# Both member images are root's, as an administrator makes them; the second
# lies inside tester's Cargo target.
truncate -s 300M /work/s1/images/a.img /work/proj/target/b.img
A=$(losetup -f --show /work/s1/images/a.img)
B=$(losetup -f --show /work/proj/target/b.img)
mkfs.btrfs -q -f -L diskgc-rk -d single -m raid1 "$A" "$B" >/dev/null
mount "$A" /mnt/btrfs-top
btrfs -q subvolume create /mnt/btrfs-top/@home
btrfs -q subvolume create /mnt/btrfs-top/@scratch
chown 1000:1000 /mnt/btrfs-top/@home /mnt/btrfs-top/@scratch
mount -o subvol=@home "$A" /mnt/home
mount -o subvol=@scratch "$A" /mnt/scratch
echo "$A $B"
""").split()
    member_a, member_b = devices
    fsid = run("btrfs filesystem show /mnt/btrfs-top | sed -n 's/.*uuid: //p'").strip()
    # mountinfo names only the first member; the second is found by
    # BTRFS_IOC_DEV_INFO alone (safety.md, Not protected, not excluded:
    # Btrfs membership).
    sources = run("for m in /mnt/home /mnt/scratch /mnt/btrfs-top; do findmnt -n -o SOURCE \"$m\"; done")
    save("s1-mounts.txt", run("btrfs filesystem show /mnt/btrfs-top; grep ' /mnt/' /proc/self/mountinfo") + f"fsid={fsid}\n")
    assert member_b not in sources and member_a in sources, (member_a, member_b, sources)
    run("""
cd /mnt/scratch
mkdir -p scratch-a/sub && head -c 65536 /dev/urandom > scratch-a/sub/data
mkdir scratch-nested && btrfs -q subvolume create scratch-nested/inner && echo x > scratch-nested/inner/f
btrfs -q subvolume create scratch-subvol && echo y > scratch-subvol/f
""", TESTER)
    run("mkdir /mnt/scratch/scratch-snap && btrfs -q subvolume snapshot /mnt/btrfs-top/@home /mnt/scratch/scratch-snap/home-snap "
        "&& chown 1000:1000 /mnt/scratch/scratch-snap")
    cfg = config("s1", ["/mnt/scratch"])

    # Roots on a sibling subvolume are on PATH's filesystem (architecture.md,
    # Filesystem matching: every btrfs subvolume of one btrfs); a candidate
    # holding a nested subvolume or snapshot, or being one, is contains_mount
    # (Filesystem matching, Subvolume boundaries).
    # Does btrfs report stx_subvol, consistently with its devices (TASK-307)?
    code, out, _ = ensure("s1-statx", TESTER, cfg, "/mnt/home", "--dry-run", strace=True, trace="statx")
    report(out, 0, "dry_run")
    btrfs_subvols = subvols_seen("s1-statx", ["scratch-nested", "inner", "scratch-subvol", "scratch-a"])
    save("s1-stx-subvol.txt", f"{btrfs_subvols}\n")
    assert all(len(v) == 1 for v in btrfs_subvols.values()), btrfs_subvols
    assert btrfs_subvols["scratch-nested"] == btrfs_subvols["scratch-a"], btrfs_subvols
    assert btrfs_subvols["inner"] != btrfs_subvols["scratch-nested"], btrfs_subvols
    assert btrfs_subvols["scratch-subvol"] != btrfs_subvols["scratch-a"], btrfs_subvols
    before = tree("/mnt/scratch", "/mnt/home")
    code, out, stderr = ensure("s1-reclaim", TESTER, cfg, "/mnt/home", strace=True)
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/scratch", "/mnt/home"), acted(result), "s1-reclaim")
    assert result["mount"] == "/mnt/home", result["mount"]
    assert acted(result) == {"/mnt/scratch/scratch-a"}, result["actions"]
    kept = outcomes(result)
    for name in ("scratch-nested", "scratch-subvol", "scratch-snap"):
        assert kept.get(f"/mnt/scratch/{name}", ("",))[0] == "contains_mount", (name, kept)
    passed("1a btrfs sibling subvolume", "scratch-a reclaimed from PATH /mnt/home; nested subvolume, "
           f"subvolume and snapshot kept contains_mount; stx_subvol reported {btrfs_subvols}")

    # One snapshot key for every subvolume (architecture.md, The tally,
    # Snapshot: uuid-<FSID> by BTRFS_IOC_FS_INFO); a second watch on a
    # sibling subvolume exits 2 (reference.md, watch: one process per
    # filesystem and state directory).
    state = "/home/tester/.local/state/diskgc"
    watch = f"{DISKGC} watch --tally-only --config {cfg} --select profile:{PROFILE}"
    code, _ = cx(
        f"{watch} /mnt/home >{EV}/s1-watch1.stdout 2>{EV}/s1-watch1.stderr & "
        f"echo $! > {EV}/s1-watch1.pid; wait $!; echo $? > {EV}/s1-watch1.code",
        TESTER, detach=True,
    )
    assert code == 0, code
    try:
        machine.wait_until_succeeds(
            f"podman exec {CTR} /bin/sh -c 'ls {state} | grep -q \"^index-.*\\.lock$\"'", timeout=60
        )
    finally:
        save("s1-watch1.stderr", run(f"cat {EV}/s1-watch1.stderr"))
    time.sleep(2)
    began = time.monotonic()
    code, out, stderr = diskgc(
        "s1-watch2", TESTER,
        f"watch --tally-only --config {cfg} --select profile:{PROFILE} /mnt/scratch", strace=True,
    )
    waited = time.monotonic() - began
    assert code == 2, (code, stderr)
    run(f"kill -TERM $(cat {EV}/s1-watch1.pid)")
    machine.wait_until_succeeds(f"podman exec {CTR} test -s {EV}/s1-watch1.code", timeout=60)
    first_code = run(f"cat {EV}/s1-watch1.code").strip()
    save("s1-watch1.stderr", run(f"cat {EV}/s1-watch1.stderr"))
    listing = run(f"ls -1 {state}")
    save("s1-state-dir.txt", listing)
    assert first_code == "0", first_code
    assert f"index-uuid-{fsid}" in listing.split(), (fsid, listing)
    fs_info = strace_calls("s1-watch2", "BTRFS_IOC_FS_INFO")
    assert fs_info and all("EBADF" not in c for c in fs_info), fs_info
    passed("1b btrfs snapshot key", f"index-uuid-{fsid} written by watch --tally-only on /mnt/home; "
           f"second watch on /mnt/scratch exit 2 after {waited:.1f}s; FS_INFO without EBADF ({len(fs_info)} calls)")

    # A member's image inside a protected Cargo target protects every
    # candidate on the btrfs (safety.md, Not protected, not excluded: a
    # filesystem on a loop device; Btrfs membership).
    run("mkdir -p /mnt/scratch/scratch-b && echo b > /mnt/scratch/scratch-b/f", TESTER)
    protected_cfg = config("s1-protect", ["/mnt/scratch"],
                           "protect:\n  - {profile: cargo-target, roots: [/work/proj]}\n")
    code, out, _ = ensure("s1-protect-dry", TESTER, protected_cfg, "/mnt/home", "--dry-run", strace=True)
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan.get("/mnt/scratch/scratch-b") == "protected", plan
    dev_info = strace_calls("s1-protect-dry", "BTRFS_IOC_DEV_INFO")
    assert dev_info and all("EBADF" not in c for c in dev_info), dev_info
    before = tree("/mnt/scratch", "/mnt/home", "/work/proj")
    code, out, _ = ensure("s1-protect", TESTER, protected_cfg, "/mnt/home")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/scratch", "/mnt/home", "/work/proj"), acted(result), "s1-protect")
    assert acted(result) == set(), result["actions"]
    assert outcomes(result)["/mnt/scratch/scratch-b"][0] == "protected", result["skipped"]
    passed("1c btrfs membership", f"second member {member_b} (image in protected target) found by "
           f"DEV_INFO ({len(dev_info)} calls); scratch-b protected, nothing deleted")


# ---- 2. Loop backing files: a user outside disk, and root ----
def scenario_loop_in_candidate() -> None:
    run("install -d -m 0755 -o 1000 -g 1000 /work/s2")
    make = "mkdir -p /work/s2/scratch-free && echo free > /work/s2/scratch-free/data"
    run(f"mkdir -p /work/s2/scratch-img/vm && truncate -s 8M /work/s2/scratch-img/vm/disk.img && {make}", TESTER)
    loop = run("losetup -f --show /work/s2/scratch-img/vm/disk.img").strip()
    save("s2-loop.txt", run(f"stat -c '%n %u:%g %a' {loop}; losetup -l {loop}"))
    cfg_user = config("s2", ["/work/s2"])
    cfg_root = config("s2-root", ["/work/s2"], owners=True)

    # A user outside disk cannot open the device: the name reaches a file of
    # the user's own, which proves nothing, so every candidate is in doubt
    # (safety.md, In use, Loop devices, 2: anything else).
    before = tree("/work/s2")
    code, out, _ = ensure("s2-user", TESTER, cfg_user, "/work/s2", strace=True)
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s2"), acted(result), "s2-user")
    assert acted(result) == set(), result["actions"]
    assert outcomes(result) == {
        "/work/s2/scratch-img": ("in_use_unknown", "loop_device"),
        "/work/s2/scratch-free": ("in_use_unknown", "loop_device"),
    }, result["skipped"]
    detail = row_of(result, "/work/s2/scratch-img")["detail"]
    passed("2a loop as tester", f"both candidates in_use_unknown/loop_device ({detail})")

    # Root opens the device: LOOP_GET_STATUS64's exact (device, inode) is a
    # use, the candidate holding the image in_use (Loop devices, 1); the
    # other candidate is judged.
    code, out, _ = ensure("s2-root-dry", ROOT, cfg_root, "/work/s2", "--dry-run", strace=True)
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {"/work/s2/scratch-img": "in_use", "/work/s2/scratch-free": None}, plan
    status = strace_calls("s2-root-dry", "LOOP_GET_STATUS64")
    assert status and all(c.rstrip().endswith("= 0") for c in status), status
    passed("2b loop as root (plan)", f"scratch-img in_use, scratch-free eligible; LOOP_GET_STATUS64 "
           f"answered ({len(status)} calls)")
    before = tree("/work/s2")
    code, out, _ = ensure("s2-root", ROOT, cfg_root, "/work/s2")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s2"), acted(result), "s2-root")
    row = row_of(result, "/work/s2/scratch-img")
    assert row["reason"] == "in_use", row
    assert acted(result) == {"/work/s2/scratch-free"}, result["skipped"]
    passed("2b loop as root (reclaim)", f"scratch-img in_use ({row['detail']}); scratch-free reclaimed")


def scenario_loop_deleted_name() -> None:
    # A deleted backing name whose hard link survives in a candidate: for
    # the user the doubt is scoped to the surviving parent's filesystem, and
    # a candidate elsewhere is judged; root's exact identity finds the link
    # (safety.md, In use, Loop devices, 2: ` (deleted)`; 1).
    run("install -d -m 0755 -o 1000 -g 1000 /work/s2d && install -d -m 0755 /mnt/s2tmp")
    make_plain = "mkdir -p /work/s2d/scratch-plain && echo plain > /work/s2d/scratch-plain/data"
    run(f"""
mkdir -p /work/s2d/a /work/s2d/scratch-link
truncate -s 8M /work/s2d/a/img
ln /work/s2d/a/img /work/s2d/scratch-link/img-link
{make_plain}
""", TESTER)
    # tester's own tmpfs, with root's filler so 100% stays out of reach.
    run("mount -t tmpfs -o size=16m,mode=0755,uid=1000,gid=1000 tmpfs /mnt/s2tmp && "
        "head -c 1048576 /dev/zero > /mnt/s2tmp/filler && install -d -o 1000 -g 1000 /mnt/s2tmp/scratch-other")
    run("echo other > /mnt/s2tmp/scratch-other/data", TESTER)
    loop = run("losetup -f --show /work/s2d/a/img").strip()
    run("rm /work/s2d/a/img")
    backing = loop_backing(loop)
    assert backing == "/work/s2d/a/img (deleted)", backing
    cfg_d = config("s2d", ["/work/s2d"])
    cfg_d_root = config("s2d-root", ["/work/s2d"], owners=True)
    cfg_t = config("s2t", ["/mnt/s2tmp"])
    before = tree("/work/s2d")
    code, out, _ = ensure("s2d-user", TESTER, cfg_d, "/work/s2d")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s2d"), acted(result), "s2d-user")
    assert acted(result) == set(), result["actions"]
    assert outcomes(result) == {
        "/work/s2d/scratch-link": ("in_use_unknown", "loop_device"),
        "/work/s2d/scratch-plain": ("in_use_unknown", "loop_device"),
    }, result["skipped"]
    before = tree("/mnt/s2tmp")
    code, out, _ = ensure("s2d-user-other-fs", TESTER, cfg_t, "/mnt/s2tmp")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/s2tmp"), acted(result), "s2d-user-other-fs")
    assert acted(result) == {"/mnt/s2tmp/scratch-other"}, (result["actions"], result["skipped"])
    passed("2c deleted backing name (tester)", f"'{backing}': both /work/s2d candidates in doubt; "
           "the tmpfs candidate reclaimed")
    code, out, _ = ensure("s2d-root-dry", ROOT, cfg_d_root, "/work/s2d", "--dry-run")
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {"/work/s2d/scratch-link": "in_use", "/work/s2d/scratch-plain": None}, plan
    passed("2c deleted backing name (root plan)", "the surviving hard link in_use, scratch-plain eligible")
    before = tree("/work/s2d")
    code, out, _ = ensure("s2d-root", ROOT, cfg_d_root, "/work/s2d")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s2d"), acted(result), "s2d-root")
    row = row_of(result, "/work/s2d/scratch-link")
    assert row["reason"] == "in_use", row
    assert acted(result) == {"/work/s2d/scratch-plain"}, result["skipped"]
    passed("2c deleted backing name (root reclaim)", f"hard link kept in_use ({row['detail']}); scratch-plain reclaimed")


def scenario_loop_partition() -> None:
    # losetup -P: a partition of a loop device (extended major 259) belongs
    # to its loop device by its sysfs parent, so a candidate on the
    # partition's filesystem is judged as the image, here inside a protect
    # match (safety.md, Not protected, not excluded: a filesystem on a loop
    # device, the loop device a partition of one belongs to).
    loop = run("""
install -d -m 0755 /work/s2p /mnt/part
truncate -s 64M /work/s2p/disk.img
printf 'label: dos\\n,,L\\n' | sfdisk -q /work/s2p/disk.img
P=$(losetup -P -f --show /work/s2p/disk.img)
test -b "${P}p1"
mkfs.ext4 -q "${P}p1"
mount "${P}p1" /mnt/part
chown 1000:1000 /mnt/part
echo "$P"
""").strip()
    save("s2p-partition.txt", run(f"lsblk -o NAME,MAJ:MIN,TYPE,MOUNTPOINTS {loop}; findmnt -n -o SOURCE,MAJ:MIN /mnt/part"))
    assert run("findmnt -n -o MAJ:MIN /mnt/part").strip().startswith("259:"), "partition not on major 259"
    run("mkdir -p /mnt/part/scratch-a /mnt/part/scratch-b && echo a > /mnt/part/scratch-a/f && echo b > /mnt/part/scratch-b/f", TESTER)
    images_profile = (
        "  - name: precious-images\n"
        "    find: {roots: [/work/s2p], paths: ['disk.img'], type: file}\n"
        "    action: delete\n"
        "    examples: {match: [/work/s2p/disk.img]}\n"
        "protect: [precious-images]\n"
    )
    cfg_p = config("s2p-protect", ["/mnt/part"], images_profile)
    before = tree("/mnt/part", "/work/s2p")
    code, out, _ = ensure("s2p-protected", TESTER, cfg_p, "/mnt/part")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/part", "/work/s2p"), acted(result), "s2p-protected")
    assert acted(result) == set(), result["actions"]
    assert outcomes(result) == {
        "/mnt/part/scratch-a": ("protected", None),
        "/mnt/part/scratch-b": ("protected", None),
    }, result["skipped"]
    cfg_open = config("s2p", ["/mnt/part"])
    before = tree("/mnt/part", "/work/s2p")
    code, out, _ = ensure("s2p-open", TESTER, cfg_open, "/mnt/part")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/part", "/work/s2p"), acted(result), "s2p-open")
    assert acted(result) == {"/mnt/part/scratch-a", "/mnt/part/scratch-b"}, (result["actions"], result["skipped"])
    passed("2d losetup -P partition", f"{loop}p1's candidates protected by the protected image; "
           "reclaimed without the protect entry")


# ---- 3. idmapped bind over a root-owned image ----
def scenario_idmapped() -> None:
    run("""
install -d -m 0755 /work/s3 /work/s3/srv /work/s3/srv/build /work/s3/srv/build/scratch-c /mnt/u
install -d -m 0755 -o 1000 -g 1000 /work/s3/plain
truncate -s 8M /work/s3/srv/build/scratch-c/disk.img
echo f > /work/s3/srv/build/scratch-c/f
""")
    make_plain = "mkdir /work/s3/plain/scratch-p && echo p > /work/s3/plain/scratch-p/data"
    run(make_plain, TESTER)
    run("losetup -f --show /work/s3/srv/build/scratch-c/disk.img")
    run("mount --bind --map-users 0:1000:1 --map-groups 0:1000:1 /work/s3/srv/build /mnt/u")
    shown = run("stat -c '%u:%g' /mnt/u/scratch-c /mnt/u/scratch-c/disk.img /work/s3/srv/build/scratch-c/disk.img")
    line = run("grep ' /mnt/u ' /proc/self/mountinfo")
    save("s3-idmap.txt", shown + line)
    assert shown.split() == ["1000:1000", "1000:1000", "0:0"], shown
    assert "idmapped" in line, line
    cfg = config("s3", ["/mnt/u", "/work/s3/plain"])
    cfg_root = config("s3-root", ["/mnt/u", "/work/s3/plain"], owners=True)
    # For the user, root's image shown as the user's by the idmapped bind is
    # in doubt on that mount, while the same filesystem's plain mount is
    # judged (safety.md, In use, Loop devices, 2: an idmapped mount).
    before = tree("/work/s3")
    code, out, _ = ensure("s3-user", TESTER, cfg, "/mnt/u")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s3"), acted(result), "s3-user")
    assert acted(result) == {"/work/s3/plain/scratch-p"}, (result["actions"], result["skipped"])
    row = row_of(result, "/mnt/u/scratch-c")
    assert (row["reason"], row["unknown_cause"]) == ("in_use_unknown", "loop_device"), row
    assert "/mnt/u" in (row["detail"] or ""), row
    passed("3 idmapped bind (tester)", f"/mnt/u/scratch-c in_use_unknown/loop_device ({row['detail']}); "
           "the plain-mount candidate of the same ext4 reclaimed")
    # Root reads the device: idmapped mounts keep the exact match (Loop
    # devices, 1).
    run(make_plain, TESTER)
    code, out, _ = ensure("s3-root-dry", ROOT, cfg_root, "/mnt/u", "--dry-run")
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {"/mnt/u/scratch-c": "in_use", "/work/s3/plain/scratch-p": None}, plan
    passed("3 idmapped bind (root plan)", "/mnt/u/scratch-c in_use by the device's exact identity, plain eligible")
    before = tree("/work/s3")
    code, out, _ = ensure("s3-root", ROOT, cfg_root, "/mnt/u")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/work/s3"), acted(result), "s3-root")
    root_row = row_of(result, "/mnt/u/scratch-c")
    assert root_row["reason"] == "in_use", root_row
    assert acted(result) == {"/work/s3/plain/scratch-p"}, (result["actions"], result["skipped"])
    passed("3 idmapped bind (root reclaim)", f"in_use ({root_row['detail']}); plain reclaimed")


# ---- 6. A live overlay root with a snap-like root-owned squashfs ----
def scenario_live_overlay() -> None:
    run("""
test -b /dev/sr0
mkdir -p /cdrom /rofs /live /work/s6/cow/upper /work/s6/cow/work /work/s6/snapsrc/meta
install -d -m 0755 -o 1000 -g 1000 /work/s6/elsewhere
mount -o ro /dev/sr0 /cdrom
mount -o ro,loop /cdrom/casper/filesystem.squashfs /rofs
mount -t overlay overlay -o lowerdir=/rofs,upperdir=/work/s6/cow/upper,workdir=/work/s6/cow/work /live
chown 1000:1000 /live/home/tester
echo 'name: core' > /work/s6/snapsrc/meta/snap.yaml
mksquashfs /work/s6/snapsrc /live/var/lib/snapd/snaps/core_1.snap -quiet -noappend -all-root >/dev/null
mount -o ro,loop /live/var/lib/snapd/snaps/core_1.snap /live/snap/core/1
""")
    loops = run("losetup -a -l -n -O NAME,BACK-FILE")
    save("s6-mounts.txt", loops + run("for m in /cdrom /rofs /live /live/snap/core/1; do findmnt -n -o TARGET,SOURCE,FSTYPE,MAJ:MIN \"$m\"; done"))
    assert "/live/var/lib/snapd/snaps/core_1.snap" in loops and "/cdrom/casper/filesystem.squashfs" in loops, loops
    assert run("findmnt -n -o FSTYPE /cdrom").strip() == "iso9660"
    make = ("mkdir -p /live/home/tester/scratch-a /live/home/tester/scratch-b && "
            "echo a > /live/home/tester/scratch-a/f && echo b > /live/home/tester/scratch-b/f")
    both = {"/live/home/tester/scratch-a", "/live/home/tester/scratch-b"}
    run(make, TESTER)
    cfg = config("s6", ["/live/home/tester"])
    # Both images are root's and a plain overlay shows owners as stored:
    # neither keeps the user's candidates on the overlay (safety.md, In use,
    # Loop devices, 2; vector inuse/loop-live-overlay-root-snaps-reclaims).
    before = tree("/live/home/tester")
    code, out, _ = ensure("s6-user", TESTER, cfg, "/live/home/tester")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/live/home/tester"), acted(result), "s6-user")
    assert acted(result) == both, (result["actions"], result["skipped"])
    passed("6 live overlay root (tester)", "both overlay candidates reclaimed with the snap and the casper squashfs attached")
    # Root reads the snap's device: opened through the overlay, its identity
    # is the overlay's, matched exactly on the overlay's own mounts; every
    # other candidate is in doubt while it is attached (Loop devices, 1).
    run(make, TESTER)
    run("mkdir -p /work/s6/elsewhere/scratch-a && echo e > /work/s6/elsewhere/scratch-a/f", TESTER)
    cfg_root = config("s6-root", ["/live/home/tester"], owners=True)
    cfg_elsewhere = config("s6-root-elsewhere", ["/work/s6/elsewhere"], owners=True)
    code, out, _ = ensure("s6-root-dry", ROOT, cfg_root, "/live/home/tester", "--dry-run", strace=True)
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {c: None for c in both}, plan
    code, out, _ = ensure("s6-root-elsewhere-dry", ROOT, cfg_elsewhere, "/work/s6/elsewhere", "--dry-run")
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {"/work/s6/elsewhere/scratch-a": "in_use_unknown"}, plan
    passed("6 live overlay root (root plan)", "overlay candidates eligible; a candidate off the overlay in_use_unknown")
    before = tree("/live/home/tester")
    code, out, _ = ensure("s6-root", ROOT, cfg_root, "/live/home/tester")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/live/home/tester"), acted(result), "s6-root")
    assert acted(result) == both, (result["actions"], result["skipped"])
    passed("6 live overlay root (root reclaim)", "both overlay candidates reclaimed")


# ---- 7. bcachefs (the out-of-tree module), one subvolume case ----
# architecture.md, Filesystem matching, Subvolume boundaries: diskgc never
# removes a nested subvolume. A bcachefs one keeps its parent's st_dev, so
# the boundary there is its stx_subvol (TASK-307; before the fix this
# candidate was deleted, subvolume included).
def scenario_bcachefs() -> None:
    # The pinned kernel (vm-real-kernel.nix) the out-of-tree module is
    # built for: a flake.lock bump that moves it fails here, not silently.
    kernel = run("uname -r").strip()
    assert kernel == KERNEL_VERSION, (kernel, KERNEL_VERSION)
    if "bcachefs" not in run("cat /proc/filesystems"):
        summary.append("UNAVAILABLE 7 bcachefs: not in the guest kernel's /proc/filesystems")
        return
    loop = run("""
install -d -m 0755 /work/s7 /mnt/bcfs
truncate -s 512M /work/s7/bc.img
L=$(losetup -f --show /work/s7/bc.img)
bcachefs format "$L" >/dev/null
mount -t bcachefs "$L" /mnt/bcfs
chown 1000:1000 /mnt/bcfs
echo "$L"
""").strip()
    run("mkdir -p /mnt/bcfs/scratch-a /mnt/bcfs/scratch-nested && echo a > /mnt/bcfs/scratch-a/f", TESTER)
    made = cx("bcachefs subvolume create /mnt/bcfs/scratch-nested/inner 2>&1 && echo i > /mnt/bcfs/scratch-nested/inner/f", TESTER)
    if made[0] != 0:
        run("bcachefs subvolume create /mnt/bcfs/scratch-nested/inner && echo i > /mnt/bcfs/scratch-nested/inner/f "
            "&& chown -R 1000:1000 /mnt/bcfs/scratch-nested/inner")
    devs = run("stat -c '%d %n' /mnt/bcfs/scratch-nested /mnt/bcfs/scratch-nested/inner")
    save("s7-bcachefs.txt", f"{loop}\nsubvolume create as tester: exit {made[0]} {made[1]}\n{devs}"
         + run("bcachefs version; findmnt -n -o TARGET,SOURCE,FSTYPE,MAJ:MIN /mnt/bcfs"))
    cfg = config("s7", ["/mnt/bcfs"])
    code, out, _ = ensure("s7-statx", TESTER, cfg, "/mnt/bcfs", "--dry-run", strace=True, trace="statx")
    report(out, 0, "dry_run")
    bc_subvols = subvols_seen("s7-statx", ["scratch-nested", "inner", "scratch-a"])
    save("s7-stx-subvol.txt", f"{bc_subvols}\n")
    before = tree("/mnt/bcfs")
    code, out, _ = ensure("s7-user", TESTER, cfg, "/mnt/bcfs")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/mnt/bcfs"), acted(result), "s7-user")
    nested = outcomes(result).get("/mnt/bcfs/scratch-nested")
    assert acted(result) == {"/mnt/bcfs/scratch-a"}, (result["actions"], result["skipped"])
    assert nested is not None and nested[0] == "contains_mount", (nested, devs)
    passed("7 bcachefs", f"on Linux {kernel}: scratch-a reclaimed, scratch-nested (a subvolume inside) kept contains_mount "
           f"({row_of(result, '/mnt/bcfs/scratch-nested')['detail']}); st_dev {' vs '.join(devs.split()[0::2])}; "
           f"stx_subvol {bc_subvols}")


# ---- 8. An overlay whose upper layer is on btrfs ----
# architecture.md, Filesystem matching, Subvolume boundaries (TASK-307
# review): an rmdir through an overlay removes a directory's upper entry, a
# btrfs subvolume of the upper layer included, and leaves a lower-only
# directory alone. The upper layer is proved through upperdir=; a lower
# layer on another filesystem (ext4 here: Docker's overlay2 on a btrfs
# upper with images elsewhere) changes stx_subvol between layers and must
# still reclaim. Where upperdir= cannot be reached (a name through a
# symlink), only a directory whose stx_subvol differs is refused.
def scenario_overlay_upper() -> None:
    loop = run("""
install -d -m 0755 /work/s8 /mnt/ovb /mnt/ov /mnt/ov2
truncate -s 300M /work/s8/upper.img
L=$(losetup -f --show /work/s8/upper.img)
mkfs.btrfs -q -f "$L" >/dev/null
mount "$L" /mnt/ovb
for n in 1 2; do
  install -d -m 0755 /mnt/ovb/upper$n /mnt/ovb/work$n
  install -d -m 0755 -o 1000 -g 1000 /mnt/ovb/upper$n/cache /work/s8/lower$n/cache
done
ln -s /mnt/ovb /work/s8/ovlink
echo "$L"
""").strip()
    run("""
for n in 1 2; do
  U=/mnt/ovb/upper$n/cache; L=/work/s8/lower$n/cache
  mkdir -p $U/scratch-a && echo a > $U/scratch-a/f && btrfs -q subvolume create $U/scratch-a/inner && echo i > $U/scratch-a/inner/f
  mkdir -p $U/scratch-mixed && echo m > $U/scratch-mixed/f
  mkdir -p $L/scratch-mixed/lowsub && echo l > $L/scratch-mixed/lowsub/f
  mkdir -p $L/scratch-low/sub && echo l > $L/scratch-low/sub/f
done
""", TESTER)
    # Overlay 1 names its upper layer plainly; overlay 2 through a symlink,
    # which diskgc does not follow (its upperdir= cannot be reached).
    run("mount -t overlay overlay -o lowerdir=/work/s8/lower1,upperdir=/mnt/ovb/upper1,workdir=/mnt/ovb/work1 /mnt/ov && "
        "mount -t overlay overlay -o lowerdir=/work/s8/lower2,upperdir=/work/s8/ovlink/upper2,workdir=/work/s8/ovlink/work2 /mnt/ov2")
    save("s8-mounts.txt", run("grep ' overlay ' /proc/self/mountinfo"))
    assert "upperdir=/work/s8/ovlink/upper2" in run("grep ' /mnt/ov2 ' /proc/self/mountinfo")
    cfg = config("s8", ["/mnt/ov/cache"])
    code, out, _ = ensure("s8-statx", TESTER, cfg, "/mnt/ov", "--dry-run", strace=True, trace="statx")
    plan = plan_of(report(out, 0, "dry_run"))
    # The control: different stx_subvol between layers (btrfs upper, ext4
    # lower) refuses nothing; only the upper-layer subvolume does.
    assert plan == {
        "/mnt/ov/cache/scratch-a": "contains_mount",
        "/mnt/ov/cache/scratch-mixed": None,
        "/mnt/ov/cache/scratch-low": None,
    }, plan
    shown = subvols_seen("s8-statx", ["scratch-a", "inner", "scratch-mixed", "lowsub", "scratch-low"])
    save("s8-stx-subvol.txt", f"{shown}\n")
    before = tree("/mnt/ov/cache", "/mnt/ovb/upper1", "/work/s8/lower1")
    code, out, _ = ensure("s8-upper", TESTER, cfg, "/mnt/ov")
    result = report(out, 1, "exhausted")
    after = tree("/mnt/ov/cache", "/mnt/ovb/upper1", "/work/s8/lower1")
    gone = {p for p in before if p not in after}
    save("s8-upper.oracle", "gone:\n" + "\n".join(sorted(gone)) + "\nadded:\n" + "\n".join(sorted(set(after) - set(before))) + "\n")
    row = row_of(result, "/mnt/ov/cache/scratch-a")
    assert row["reason"] == "contains_mount" and "upper layer" in (row["detail"] or ""), row
    # The oracle across the layers: nothing of scratch-a, in the merged view
    # or its upper layer, and nothing of the lower layer is gone; the
    # upper subvolume still exists.
    assert not any(p.startswith(("/mnt/ov/cache/scratch-a", "/mnt/ovb/upper1/cache/scratch-a", "/work/s8/lower1")) for p in gone), sorted(gone)
    run("btrfs subvolume show /mnt/ovb/upper1/cache/scratch-a/inner >/dev/null")  # still a subvolume
    # The control's reclaim: not refused for a subvolume. Deleting a lower
    # entry copies its directory up, which changes the directory's birth
    # time: the walk then stops `changed` (a finding of TASK-307's review,
    # outside this rule: recorded, not asserted as wanted).
    others = {c: outcomes(result).get(c, ("reclaimed", None))[0]
              for c in ("/mnt/ov/cache/scratch-mixed", "/mnt/ov/cache/scratch-low")}
    assert all(o not in ("contains_mount", "unsupported_fs") for o in others.values()), others
    assert acted(result) == {"/mnt/ov/cache/scratch-mixed", "/mnt/ov/cache/scratch-low"}, (result["actions"], result["skipped"])
    passed("8 overlay upper layer", f"scratch-a (upper btrfs subvolume inside) kept ({row['detail']}); the mixed-layer "
           f"and lower-only candidates eligible in the plan and acted on ({others}); lower layer and upper subvolume "
           f"untouched; stx_subvol through the overlay {shown}")
    cfg2 = config("s8-unreached", ["/mnt/ov2/cache"])
    code, out, _ = ensure("s8-unreached", TESTER, cfg2, "/mnt/ov2")
    result = report(out, 1, "exhausted")
    kept = outcomes(result)
    assert kept.get("/mnt/ov2/cache/scratch-a", ("",))[0] == "contains_mount", kept
    assert "cannot be reached" in (row_of(result, "/mnt/ov2/cache/scratch-a")["detail"] or ""), kept
    run("btrfs subvolume show /mnt/ovb/upper2/cache/scratch-a/inner >/dev/null")  # still a subvolume
    passed("8 overlay upper layer unreached", f"upperdir= through a symlink: scratch-a kept "
           f"({row_of(result, '/mnt/ov2/cache/scratch-a')['detail']}); outcomes {kept}; acted {sorted(acted(result))}")
    run(f"umount /mnt/ov /mnt/ov2 /mnt/ovb && losetup -d {loop}")


def overlay_oracle(tag: str, before: dict[str, str], after: dict[str, str], candidates: set[str],
                   untouched: tuple[str, ...]) -> None:
    """The deletion oracle through an overlay: every non-directory entry of
    CANDIDATES is gone (a lower-only directory may stay: deleting through
    it copies it up, a change the walk then stops at, see scenario 8),
    nothing outside them is, nothing is added, and every entry below the
    UNTOUCHED prefixes (layers, other names) is exactly as it was."""
    def inside(p: str) -> bool:
        return any(p == c or p.startswith(c + "/") for c in candidates)
    gone = {p for p in before if p not in after}
    added = sorted(set(after) - set(before))
    files = {p for p in before if inside(p) and not before[p].startswith("d ")}
    moved = sorted(p for p in before if p.startswith(untouched) and after.get(p) != before[p])
    save(f"{tag}.oracle", "gone:\n" + "\n".join(sorted(gone)) + "\nadded:\n" + "\n".join(added)
         + "\nchanged below untouched:\n" + "\n".join(moved) + "\n")
    assert all(inside(p) for p in gone), (tag, sorted(p for p in gone if not inside(p)))
    assert files <= gone, (tag, sorted(files - gone))
    assert not added and not moved, (tag, added, moved)


# ---- 9. A loop image with a second name in an overlay's upper layer ----
# safety.md, In use, Loop devices (TASK-305): an overlay shows a file of its
# upper layer under its own device or the upper layer's anonymous one, so a
# loop device opened by a plain name of the file identifies it by an
# identity the walk through the overlay never sees, and deleting through
# the overlay unlinks the upper name. The walk reads each regular file's
# upper entry (upperdir= plus its path below the overlay) and the in-use
# check matches that identity too. First the kernel facts the rule rests
# on are probed on scratch overlays: the identity the overlay shows (with
# and without xino), an unlink of an upper name, and an unlink of a
# lower-only name (TASK-305 item 4: a whiteout, the lower name survives).
def scenario_overlay_upper_loop() -> None:
    loop = run("""
install -d -m 0755 /work/s9 /mnt/ovb /mnt/ov /mnt/ov2 /mnt/ovx /mnt/ovxi
truncate -s 300M /work/s9/upper.img
L=$(losetup -f --show /work/s9/upper.img)
mkfs.btrfs -q -f "$L" >/dev/null
mount "$L" /mnt/ovb
for d in upper work upper2 work2 upperx workx upperxi workxi; do install -d -m 0755 /mnt/ovb/$d; done
for d in /mnt/ovb/images /mnt/ovb/upper/cache /mnt/ovb/upper/cache2 /mnt/ovb/upper2/cache \
    /mnt/ovb/upper/cache3 /mnt/ovb/upper2/cache3 \
    /mnt/ovb/upperx/p /mnt/ovb/upperxi/p /work/s9/images /work/s9/lower/cache /work/s9/lower/cache2 \
    /work/s9/lower2/cache /work/s9/lowerx /work/s9/lowerx/p /work/s9/lowerxi; do
  install -d -m 0755 -o 1000 -g 1000 $d
done
ln -s /mnt/ovb /work/s9/ovlink
echo "$L"
""").strip()
    # Fixtures before the overlays are mounted (a layer changed under a
    # mounted overlay is undefined). held.img has three names on the btrfs:
    # its own and one in each of the upper directories cache and cache2.
    run("""
truncate -s 16M /mnt/ovb/images/held.img
U=/mnt/ovb/upper
mkdir $U/cache/scratch-held $U/cache2/scratch-held
ln /mnt/ovb/images/held.img $U/cache/scratch-held/disk.img
ln /mnt/ovb/images/held.img $U/cache2/scratch-held/disk.img
mkdir $U/cache/scratch-plain $U/cache2/scratch-plain
echo p > $U/cache/scratch-plain/f && echo p > $U/cache2/scratch-plain/f
truncate -s 16M /work/s9/images/low.img
mkdir /work/s9/lower/cache/scratch-lower && ln /work/s9/images/low.img /work/s9/lower/cache/scratch-lower/disk.img
mkdir /work/s9/lower/cache2/scratch-lowonly && echo l > /work/s9/lower/cache2/scratch-lowonly/f
mkdir /mnt/ovb/upper2/cache/scratch-file && echo f > /mnt/ovb/upper2/cache/scratch-file/f
mkdir -p /mnt/ovb/upper2/cache/scratch-dirs/a/b
for x in x xi; do
  echo u > /mnt/ovb/upper$x/p/pure && ln /mnt/ovb/upper$x/p/pure /mnt/ovb/upper$x/p/pure-link
done
echo l > /work/s9/lowerx/p/low && ln /work/s9/lowerx/p/low /work/s9/lowerx/p/low-link
mkdir -p $U/cache/scratch-fd $U/cache/scratch-cwd/sub /work/s9/lower/cache/scratch-map
echo f > $U/cache/scratch-fd/f && echo c > $U/cache/scratch-cwd/sub/f
cp "$(dirname "$(awk '/libc.so/{print $6; exit}' /proc/self/maps)")/libm.so.6" /work/s9/lower/cache/scratch-map/lib.so
# A Cargo-target-shaped tree for the walk's cost (TASK-305 review, mped
# S2): the same 20000 files in a reached and an unreached upper layer.
for u in $U/cache3 /mnt/ovb/upper2/cache3; do
  mkdir -p $u/scratch-target/debug
  for d in $(seq 1 200); do
    mkdir -p $u/scratch-target/debug/deps/u$d
    for f in $(seq 1 100); do : > $u/scratch-target/debug/deps/u$d/f$f.o; done
  done
done
""", TESTER)
    run("mount -t overlay overlay -o lowerdir=/work/s9/lower,upperdir=/mnt/ovb/upper,workdir=/mnt/ovb/work /mnt/ov && "
        "mount -t overlay overlay -o lowerdir=/work/s9/lower2,upperdir=/work/s9/ovlink/upper2,workdir=/work/s9/ovlink/work2 /mnt/ov2 && "
        "mount -t overlay overlay -o lowerdir=/work/s9/lowerx,upperdir=/mnt/ovb/upperx,workdir=/mnt/ovb/workx /mnt/ovx && "
        "mount -t overlay overlay -o lowerdir=/work/s9/lowerxi,upperdir=/mnt/ovb/upperxi,workdir=/mnt/ovb/workxi,xino=on /mnt/ovxi")
    save("s9-mounts.txt", run("grep ' overlay ' /proc/self/mountinfo; cat /sys/module/overlay/parameters/xino_auto"))

    # The premises, on this kernel. stat: device (decimal), inode, links.
    def ident(path: str) -> list[str]:
        return run(f"stat -c '%d %i %h' {path}").split()
    facts: list[str] = []

    def note(line: str) -> None:
        facts.append(line)
        save("s9-premises.txt", "\n".join(facts) + "\n")
    for x in ("x", "xi"):
        shown, upper = ident(f"/mnt/ov{x}/p/pure"), ident(f"/mnt/ovb/upper{x}/p/pure")
        note(f"(a) ov{x}: overlay shows {shown}, upper name {upper}")
        # (a) the overlay never shows the upper filesystem's device for it.
        assert shown[0] != upper[0], facts
    # (b) an unlink through the overlay removes the upper name.
    run("rm /mnt/ovx/p/pure", TESTER)
    gone = run("test -e /mnt/ovb/upperx/p/pure && echo present || echo gone").strip()
    left = ident("/mnt/ovb/upperx/p/pure-link")
    note(f"(b) after rm /mnt/ovx/p/pure: upper name {gone}, its other link {left}")
    assert gone == "gone" and left[2] == "1", facts
    # (4) an unlink through the overlay of a lower-only name writes a
    # whiteout over it and leaves the lower name.
    before_low = ident("/work/s9/lowerx/p/low")
    run("rm /mnt/ovx/p/low", TESTER)
    after_low = ident("/work/s9/lowerx/p/low")
    whiteout = run("stat -c '%F %t:%T' /mnt/ovb/upperx/p/low").strip()
    note(f"(4) after rm /mnt/ovx/p/low: lower name {before_low} -> {after_low}, upper entry {whiteout!r}")
    assert after_low == before_low and before_low[2] == "2", facts
    assert whiteout == "character special file 0:0", facts

    # Processes that use files by their upper-layer names (TASK-305
    # review, Codex 1): one holds an upper file open, one works in an
    # upper directory, one maps a copied-up library by its upper name. The
    # overlay shows that library under its lower origin's inode, so the
    # walk's identity and the mapping's inode differ.
    run("touch /mnt/ov/cache/scratch-map/lib.so", TESTER)  # copy-up
    copied = (ident("/mnt/ov/cache/scratch-map/lib.so"), ident("/mnt/ovb/upper/cache/scratch-map/lib.so"))
    note(f"(c) copied-up lib.so: overlay shows {copied[0]}, upper name {copied[1]}")
    assert copied[0][:2] != copied[1][:2] and copied[0][1] != copied[1][1], facts
    holders = {
        "fd": "exec 3< /mnt/ovb/upper/cache/scratch-fd/f; exec sleep 900",
        "cwd": "cd /mnt/ovb/upper/cache/scratch-cwd/sub && exec sleep 900",
        "map": "LD_PRELOAD=/mnt/ovb/upper/cache/scratch-map/lib.so exec sleep 900",
    }
    for kind, command in holders.items():
        cx(f"echo $$ > {EV}/s9-{kind}.pid; {command}", TESTER, detach=True)
    machine.wait_until_succeeds(
        f"podman exec {CTR} /bin/sh -c 'for k in fd cwd map; do test -s {EV}/s9-$k.pid || exit 1; done; "
        f"grep -q scratch-map/lib.so /proc/$(cat {EV}/s9-map.pid)/maps'", timeout=30)
    save("s9-holders.txt", run(
        f"for k in fd cwd map; do p=$(cat {EV}/s9-$k.pid); echo \"$k pid $p: $(ls -l /proc/$p/fd | grep scratch || true) "
        f"cwd $(readlink /proc/$p/cwd) $(grep scratch /proc/$p/maps | head -1)\"; done"))

    def stop_holders() -> None:
        cx(f"for k in fd cwd map; do test -s {EV}/s9-$k.pid && kill $(cat {EV}/s9-$k.pid); done; true")

    held = run("losetup -f --show /mnt/ovb/images/held.img").strip()
    low = run("losetup -f --show /work/s9/images/low.img").strip()
    save("s9-loops.txt", run("losetup -l"))

    # Root opens the devices: the upper name of held.img is its identity,
    # in_use; the candidate with an upper file of no loop's, and the
    # lower-only ones (a lower name of low.img among them), reclaim.
    cfg = config("s9-root", ["/mnt/ov/cache"], owners=True)
    code, out, _ = ensure("s9-root-dry", ROOT, cfg, "/mnt/ov", "--dry-run")
    plan = plan_of(report(out, 0, "dry_run"))
    assert plan == {
        "/mnt/ov/cache/scratch-held": "in_use",
        "/mnt/ov/cache/scratch-plain": None,
        "/mnt/ov/cache/scratch-lower": None,
        "/mnt/ov/cache/scratch-fd": "in_use",
        "/mnt/ov/cache/scratch-cwd": "in_use",
        "/mnt/ov/cache/scratch-map": "in_use",
    }, plan

    # A decoy over the printed upperdir= (Codex 2): root binds an empty
    # directory over /mnt/ovb/upper after the overlay was made. Lookups
    # there find nothing; the table lists the bind after the overlay, so
    # the layer is not trusted and scratch-held is in doubt.
    run("install -d -m 0755 /work/s9/decoy && mount --bind /work/s9/decoy /mnt/ovb/upper")
    order = run("awk '$5==\"/mnt/ov\" || $5==\"/mnt/ovb/upper\" {print NR, $1, $5}' /proc/self/mountinfo")
    save("s9-decoy-order.txt", order)
    code, out, _ = ensure("s9-decoy-dry", ROOT, cfg, "/mnt/ov", "--dry-run")
    decoy = report(out, 0, "dry_run")
    run("umount /mnt/ovb/upper")
    assert plan_of(decoy)["/mnt/ov/cache/scratch-held"] == "in_use_unknown", decoy["plan"]
    passed("9d overlay upper decoy (root)", f"bind over upperdir= listed after the overlay ({order.strip()!r}): "
           "scratch-held in_use_unknown")
    roots = ("/mnt/ov/cache", "/mnt/ovb/images", "/mnt/ovb/upper/cache/scratch-held",
             "/mnt/ovb/upper/cache/scratch-fd", "/mnt/ovb/upper/cache/scratch-cwd",
             "/mnt/ovb/upper/cache/scratch-map", "/work/s9/lower", "/work/s9/images")
    before = tree(*roots)
    code, out, _ = ensure("s9-root", ROOT, cfg, "/mnt/ov")
    result = report(out, 1, "exhausted")
    stop_holders()
    assert acted(result) == {"/mnt/ov/cache/scratch-plain", "/mnt/ov/cache/scratch-lower"}, (result["actions"], result["skipped"])
    evidence = {k: [b["evidence"] for b in row_of(result, f"/mnt/ov/cache/scratch-{k}")["process_blockers"] or []]
                for k in ("fd", "cwd", "map")}
    # A map shows the device of the inode's superblock, which on btrfs need
    # not be the one statx shows: the exact match (map) or the upper
    # inode number (inode_map); never the overlay's view, whose inode is
    # the lower origin's.
    assert evidence["fd"] == ["fd"] and evidence["cwd"] == ["cwd"], evidence
    assert evidence["map"] in (["map"], ["inode_map"]), evidence
    passed("9a processes by upper-layer names (root)", f"scratch-fd/cwd/map in_use by {evidence}: "
           f"{row_of(result, '/mnt/ov/cache/scratch-map')['detail']}")
    overlay_oracle("s9-root", before, tree(*roots), acted(result), roots[1:])
    run("test ! -e /mnt/ov/cache/scratch-plain")  # upper-only: removed whole
    row = row_of(result, "/mnt/ov/cache/scratch-held")
    assert row["reason"] == "in_use", row
    assert "upper layer, /mnt/ovb/upper/cache/scratch-held/disk.img" in (row["detail"] or ""), row
    held_names = run("stat -c '%h' /mnt/ovb/images/held.img").strip()
    assert held_names == "3", held_names
    # Item 4 through diskgc: the lower name of low.img survives the
    # candidate's deletion, its device still reads it.
    low_after = ident("/work/s9/lower/cache/scratch-lower/disk.img")
    assert low_after == ident("/work/s9/images/low.img") and low_after[2] == "2", low_after
    assert loop_backing(low) == "/work/s9/images/low.img", loop_backing(low)
    run(f"dd if={low} of=/dev/null bs=4096 count=1 status=none")
    passed("9a overlay upper alias (root)", f"{held}: scratch-held in_use ({row['detail']}); scratch-plain and the "
           f"lower-only scratch-lower reclaimed; low.img's lower name kept ({low_after}), {low} still reads")

    # A user outside disk, the plain name deleted: the doubt covers the
    # btrfs of the deleted name's directory, and so every candidate with an
    # upper file there; a lower-only candidate (ext4) reclaims.
    run(f"losetup -d {low}")
    run("rm /mnt/ovb/images/held.img", TESTER)
    backing = loop_backing(held)
    assert backing == "/mnt/ovb/images/held.img (deleted)", backing
    cfg = config("s9-user", ["/mnt/ov/cache2"])
    roots = ("/mnt/ov/cache2", "/mnt/ovb/upper/cache2/scratch-held", "/mnt/ovb/upper/cache2/scratch-plain",
             "/work/s9/lower/cache2")
    before = tree(*roots)
    code, out, _ = ensure("s9-user", TESTER, cfg, "/mnt/ov")
    result = report(out, 1, "exhausted")
    assert acted(result) == {"/mnt/ov/cache2/scratch-lowonly"}, (result["actions"], result["skipped"])
    overlay_oracle("s9-user", before, tree(*roots), acted(result), roots[1:])
    kept = {c: r for c, r in outcomes(result).items() if c != "/mnt/ov/cache2/scratch-lowonly"}
    assert kept == {
        "/mnt/ov/cache2/scratch-held": ("in_use_unknown", "loop_device"),
        "/mnt/ov/cache2/scratch-plain": ("in_use_unknown", "loop_device"),
    }, result["skipped"]
    passed("9b overlay upper alias (tester, deleted name)", f"'{backing}': the candidates with upper files on its "
           f"btrfs in doubt ({row_of(result, '/mnt/ov/cache2/scratch-held')['detail']}); the lower-only one reclaimed")

    # An upper layer diskgc cannot reach by its upperdir= (through a
    # symlink): a candidate holding a regular file is in doubt while a
    # device has an identity; one holding only directories reclaims.
    cfg = config("s9-unreached", ["/mnt/ov2/cache"], owners=True)
    roots = ("/mnt/ov2/cache", "/mnt/ovb/upper2/cache/scratch-file", "/work/s9/lower2")
    before = tree(*roots)
    code, out, _ = ensure("s9-unreached", ROOT, cfg, "/mnt/ov2")
    result = report(out, 1, "exhausted")
    assert acted(result) == {"/mnt/ov2/cache/scratch-dirs"}, (result["actions"], result["skipped"])
    overlay_oracle("s9-unreached", before, tree(*roots), acted(result), roots[1:])
    row = row_of(result, "/mnt/ov2/cache/scratch-file")
    assert (row["reason"], row["unknown_cause"]) == ("in_use_unknown", "loop_device"), row
    assert "upper layer cannot be read" in (row["detail"] or ""), row
    run("test -f /mnt/ovb/upper2/cache/scratch-file/f")
    passed("9c overlay upper alias (unreached upper layer)", f"scratch-file in doubt ({row['detail']}); "
           "scratch-dirs reclaimed")
    # The cost of reading upper names (mped S2): dry runs over the same
    # 20000-file tree through the reached overlay (an upper lookup per
    # entry) and the unreached one (none), three each; recorded, not
    # asserted.
    run(f"losetup -d {held}")
    timing: dict[str, list[int]] = {}
    for name, mnt in (("reached", "/mnt/ov"), ("unreached", "/mnt/ov2")):
        cfg_t = config(f"s9-cost-{name}", [f"{mnt}/cache3"])
        for i in range(3):
            code, out, _ = ensure(f"s9-cost-{name}-{i}", TESTER, cfg_t, mnt, "--dry-run")
            result = report(out, 0, "dry_run")
            assert plan_of(result) == {f"{mnt}/cache3/scratch-target": None}, result["plan"]
            timing.setdefault(name, []).append(result["duration_ms"])
    save("s9-cost.txt", f"20000 files, 201 directories; dry-run duration_ms: {timing}\n")
    passed("9e upper-name cost (measured)", f"20000-file tree, dry-run ms: {timing}")
    run(f"umount /mnt/ov /mnt/ov2 /mnt/ovx /mnt/ovxi && umount /mnt/ovb && losetup -d {loop}")


# ---- 5. udisksctl loop-setup by a user outside disk ----
def scenario_udisks() -> None:
    # D-Bus activated; started here as the first udisksctl call would.
    machine.succeed("systemctl start udisks2.service")
    machine.succeed(
        "install -d -o alice -g users /home/alice/Downloads && "
        f"install -m 0644 -o alice -g users {LIVE_ISO} /home/alice/Downloads/live.iso"
    )
    run("mkdir -p /home/alice/scratch-a && echo a > /home/alice/scratch-a/f", ALICE)
    attached = machine.succeed(
        "su - alice -c 'udisksctl loop-setup --no-user-interaction -r -f /home/alice/Downloads/live.iso'"
    )
    loop = attached.strip().rsplit(" ", 1)[-1].rstrip(".")
    assert loop.startswith("/dev/loop"), attached
    device = run(f"stat -c '%u:%g %a' {loop}").strip()
    backing = loop_backing(loop)
    save("s5-udisks.txt", f"{attached}device {loop}: {device}\nbacking: {backing}\n"
         + machine.succeed(f"udisksctl info -b {loop} | grep -E 'SetupByUID|BackingFile' || true"))
    cfg = config("s5", ["/home/alice"])
    code, out, _ = ensure("s5-dry", ALICE, cfg, "/home/alice", "--dry-run")
    dry = plan_of(report(out, 0, "dry_run"))
    before = tree("/home/alice/scratch-a", "/home/alice/Downloads")
    code, out, _ = ensure("s5-user", ALICE, cfg, "/home/alice")
    result = report(out, 1, "exhausted")
    require_removed(before, tree("/home/alice/scratch-a", "/home/alice/Downloads"), acted(result), "s5-user")
    assert acted(result) == set(), result["actions"]
    row = row_of(result, "/home/alice/scratch-a")
    machine.succeed(f"su - alice -c 'udisksctl loop-delete --no-user-interaction -b {loop}'")
    machine.wait_until_succeeds(f"podman exec {CTR} /bin/sh -c 'test -z \"$({TOOLS}/bin/losetup -a)\"'", timeout=30)
    before = tree("/home/alice/scratch-a", "/home/alice/Downloads")
    code, out, _ = ensure("s5-after-detach", ALICE, cfg, "/home/alice")
    after = report(out, 1, "exhausted")
    require_removed(before, tree("/home/alice/Downloads"), acted(after), "s5-after-detach")
    assert acted(after) == {"/home/alice/scratch-a"}, (after["actions"], after["skipped"])
    passed("5 udisks loop-setup (measured cost)", f"{loop} mode {device}, backing {backing}; dry run {dry}; "
           f"reclaim kept scratch-a {row['reason']}/{row['unknown_cause']}: {row['detail']}; "
           "after udisksctl loop-delete it is reclaimed")


# ---- 4. hidepid=invisible on the held /proc ----
def scenario_hidepid() -> None:
    run("install -d -m 0755 -o 1000 -g 1000 /work/s4")
    run("mkdir -p /work/s4/scratch-h && echo h > /work/s4/scratch-h/f", TESTER)
    cfg = config("s4", ["/work/s4"])
    guest_proc = machine.succeed("findmnt -n -o OPTIONS /proc")
    run("mount -o remount,hidepid=invisible /proc")
    options = run("findmnt -n -o OPTIONS /proc")
    assert "hidepid=invisible" in options, options
    # The guest's own /proc is another procfs instance, unchanged.
    assert machine.succeed("findmnt -n -o OPTIONS /proc") == guest_proc
    before = tree("/work/s4")
    # safety.md, In use, Visibility: a process list that cannot be complete.
    # Below --min-free: exit 2, cause safety.
    code, out, _ = ensure("s4-below", TESTER, cfg, "/work/s4")
    below = report(out, 2, "usage_error")
    assert [e["cause"] for e in below["errors"]] == ["safety"], below["errors"]
    assert "hidepid" in below["errors"][0]["message"], below["errors"]
    # Met: the usual no-op, exit 0, one stderr line that a reclaim here
    # would be refused (TASK-298); the JSON report is unchanged.
    code, out, met_err = ensure("s4-met", TESTER, cfg, "/work/s4", min_free="1M")
    report(out, 0, "met")
    warning = [line for line in met_err.splitlines() if "hidepid" in line]
    assert len(warning) == 1, met_err
    # The dry run: exit 2 whatever the space, naming where to run instead.
    code, out, _ = ensure("s4-dry", TESTER, cfg, "/work/s4", "--dry-run", min_free="1M")
    dry = report(out, 2, "usage_error")
    assert [e["cause"] for e in dry["errors"]] == ["safety"], dry["errors"]
    require_removed(before, tree("/work/s4"), set(), "s4")
    passed("4 hidepid", f"below goal exit 2 safety ({below['errors'][0]['message']}); met exit 0, "
           f"stderr {warning[0]!r}; dry run exit 2 ({dry['errors'][0]['message']})")


failures: list[str] = []
for scenario in (scenario_btrfs, scenario_loop_in_candidate, scenario_loop_deleted_name,
                 scenario_loop_partition, scenario_idmapped, scenario_live_overlay,
                 scenario_bcachefs, scenario_overlay_upper, scenario_overlay_upper_loop,
                 scenario_udisks, scenario_hidepid,
                 scenario_root_scan):
    name = scenario.__name__.removeprefix("scenario_")
    began = time.monotonic()
    try:
        no_loop_devices(name)
        scenario()
    except Exception as error:
        line = f"FAIL {name}: {type(error).__name__}: {error}"
        failures.append(line[:4000])
        summary.append(line[:4000])
        print(line)
    finally:
        cleanup(name)
    summary.append(f"  ({name} {time.monotonic() - began:.1f}s)")

machine.succeed(f"podman rm -f {CTR}")
summary.append(f"total {time.monotonic() - started_monotonic:.1f}s")
save("summary.txt", "\n".join(summary) + "\n")
assert not failures, "\n".join(failures)
