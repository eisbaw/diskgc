#!/usr/bin/env python3
"""Seeded generator of a developer's home and /tmp for walkthroughs.

Lays out, under HOME and TMP, what overnight coding agents and a developer
leave behind: Cargo projects with target/ dirs of mixed size and age (agent
worktrees, a triple layout, a project marked .diskgc-keep), a detached
CARGO_TARGET_DIR under /tmp, pytest sessions, nix-shell and nix-develop rc
dirs (owners live and dead) and unrelated /tmp cruft. Same seed, same tree:
names and sizes come from one random.Random(seed).

Targets are fabricated in Cargo's on-disk shape (CACHEDIR.TAG, profile dirs
holding .cargo-lock, .fingerprint/, deps/, build/, incremental/) rather than
built, so sizes are realistic without compiling for minutes. Live processes
(a running build, a running binary, a live pytest) are reset.sh's job.

Limit: the base tree's dead owner pids are redrawn while /proc shows them
alive, so in the host PID namespace (WALK_PID_HOST=1, where pids wrap) the
same seed can name a few rc dirs differently.

Limit: ages are mtimes only. The kernel stamps ctime at creation and a
rootless container cannot set the clock, and diskgc's idle measure takes the
newer of mtime and ctime, so every generated entry is as old as the run
that made it (an hour after walk-reset, idle-1h holds; idle-1d does not).

With --tmp-bulk N, /tmp also gets N leftovers in the shapes of the TASK-65
release-catalog walk: nix-shell and nix-develop rc dirs of dead owners, and
ended pytest sessions under ended nix-shell.XXXXXX TMPDIR roots, for volume.
Fake owner pids are below /proc/sys/kernel/pid_max: diskgc filters a name
whose pid no process could have, so a pid above it tests nothing (TASK-158).
That walk's real producers (real nix develop, nix-shell and pytest runs,
some SIGKILLed) are not folded in; live producers are reset.sh's job.

Usage: gen.py --home DIR --tmp DIR [--seed N] [--scale F] [--tmp-bulk N] [--live-pid PID]...
"""
import argparse
import os
import random
import time

MIB = 1024 * 1024
DAY_S = 86400
# Bounds so a typo in --scale cannot fill the host: the walk filesystems are
# small tmpfs mounts, and this generator never writes more than this in total.
TOTAL_BYTES_MAX = 4 * 1024 * MIB
FILES_PER_TARGET_MAX = 8000

# (path below HOME/src, MiB, idle days, profiles, keep marker)
PROJECTS = [
    ("ripgrep-fork", 380, 21, ["debug", "release"], False),
    ("blog-engine", 240, 9, ["debug"], False),
    ("diskgc-clone", 160, 2, ["debug"], False),
    ("important-lib", 180, 45, ["debug"], True),
    ("fw-nrf", 140, 14, ["debug", "thumbv7em-none-eabihf/debug"], False),
]
AGENT_WORKTREES = 4  # under HOME/src/app/.claude/worktrees, 60-140 MiB each
TMP_BULK_MAX = 8_000  # the TASK-65 walk used 4,000; 8,000 adds 300 MiB to the 1 GiB walk /tmp
BULK_PER_ROOT = 80  # one ended nix-shell.XXXXXX root (3 sessions) per 80 leftovers
DEAD_PID_TRIES_MAX = 1000


class Writer:
    """Allocates files, counting bytes against TOTAL_BYTES_MAX."""

    def __init__(self, rng, scale):
        self.rng, self.scale, self.total = rng, scale, 0

    def file(self, path, size_bytes, text=None):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            if text is not None:
                f.write(text.encode())
            elif size_bytes > 0:
                os.posix_fallocate(f.fileno(), 0, size_bytes)
        self.total += size_bytes if text is None else len(text)
        assert self.total <= TOTAL_BYTES_MAX, f"generator exceeded {TOTAL_BYTES_MAX} bytes at {path}"

    def hexid(self, n=16):
        return "".join(self.rng.choice("0123456789abcdef") for _ in range(n))


def backdate(root, days):
    """Sets every mtime and atime under root (root included) to `days` ago."""
    t = time.time() - days * DAY_S
    for dirpath, dirnames, filenames in os.walk(root, topdown=False):
        for name in filenames:
            os.utime(os.path.join(dirpath, name), (t, t), follow_symlinks=False)
        os.utime(dirpath, (t, t))


def checkout(w, project, name):
    w.file(f"{project}/Cargo.toml", 0, f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n')
    w.file(f"{project}/src/main.rs", 0, 'fn main() {\n    println!("hello");\n}\n')
    w.file(f"{project}/.git/HEAD", 0, "ref: refs/heads/main\n")
    os.makedirs(f"{project}/.git/objects", exist_ok=True)
    os.makedirs(f"{project}/.git/refs/heads", exist_ok=True)


def target(w, path, crate, mib, profiles):
    """A Cargo target dir in 1.90's classic layout, mib MiB in total."""
    w.file(f"{path}/CACHEDIR.TAG", 0, "Signature: 8a477f597d28d172789f06886806bc55\n"
           "# This file is a cache directory tag created by cargo.\n")
    w.file(f"{path}/.rustc_info.json", 0, '{"rustc_fingerprint":1,"outputs":{},"successes":{}}\n')
    budget = int(mib * w.scale * MIB) // len(profiles)
    for profile in profiles:
        p = f"{path}/{profile}"
        w.file(f"{p}/.cargo-lock", 0)
        deps = [f"{w.rng.choice(['serde', 'tokio', 'syn', 'regex', 'clap', 'libc', 'rand', 'hyper'])}"
                f"{i}-{w.hexid()}" for i in range(w.rng.randint(12, 40))]
        files = 0
        for dep in deps:
            w.file(f"{p}/.fingerprint/{dep}/lib-{dep.split('-')[0]}", 0, w.hexid(16) + "\n")
            files += 1
        spent = 0
        while spent < budget // 2 and files < FILES_PER_TARGET_MAX:  # deps: rlibs, rmetas, dep-infos
            size = min(w.rng.randint(MIB // 10, 8 * MIB), budget // 2 - spent)
            dep = w.rng.choice(deps)
            w.file(f"{p}/deps/lib{dep}.{w.rng.choice(['rlib', 'rmeta'])}", size)
            w.file(f"{p}/deps/{dep}.d", 0, f"{p}/deps/lib{dep}.rlib: src/lib.rs\n")
            spent, files = spent + size, files + 2
        for dep in deps:  # build scripts and their output
            w.file(f"{p}/build/{dep}/build-script-build", 0, "#!/bin/false\n")
            w.file(f"{p}/build/{dep}/output", 0, "cargo:rerun-if-changed=build.rs\n")
            files += 2
        sessions = [f"{p}/incremental/{crate}-{w.hexid(13)}/s-{w.hexid(10)}-{w.hexid(6)}" for _ in range(3)]
        while spent < budget * 95 // 100 and files < FILES_PER_TARGET_MAX:  # incremental: many small objects
            size = min(w.rng.randint(4 * 1024, 128 * 1024), budget - spent)
            w.file(f"{w.rng.choice(sessions)}/{w.hexid(8)}.o", size)
            spent, files = spent + size, files + 1
        w.file(f"{p}/build/{deps[0]}/out/generated.rs", max(budget - spent, 0))
        w.file(f"{p}/{crate}", 4 * MIB)
        w.file(f"{p}/{crate}.d", 0, f"{p}/{crate}: src/main.rs\n")


def home(w, root):
    src = f"{root}/src"
    for name, mib, days, profiles, keep in PROJECTS:
        project = f"{src}/{name}"
        checkout(w, project, name)
        target(w, f"{project}/target", name.replace("-", "_"), mib * w.rng.uniform(0.8, 1.2), profiles)
        if keep:
            w.file(f"{project}/.diskgc-keep", 0, "release builds for the demo, do not clean\n")
        backdate(project, days + w.rng.random())
    checkout(w, f"{src}/app", "app")
    for i in range(AGENT_WORKTREES):
        tree = f"{src}/app/.claude/worktrees/agent-{w.hexid(8)}"
        checkout(w, tree, "app")
        target(w, f"{tree}/target", "app", w.rng.randint(60, 140), ["debug"])
        backdate(tree, i * 0.9 + w.rng.random() * 0.5)
    w.file(f"{root}/.cache/pip/http-v2/{w.hexid(2)}/{w.hexid(40)}", 30 * MIB)
    backdate(f"{root}/.cache", 12)


def read_pid_max():
    with open("/proc/sys/kernel/pid_max") as f:
        return int(f.read())


def dead_pid(w, lo, hi):
    """A pid in [lo, hi] that no process in this PID namespace has now."""
    assert 1 < lo < hi, f"empty pid range [{lo}, {hi}]"
    for _ in range(DEAD_PID_TRIES_MAX):
        pid = w.rng.randint(lo, hi)
        if not os.path.exists(f"/proc/{pid}"):
            return pid
    raise AssertionError(f"no dead pid in [{lo}, {hi}] after {DEAD_PID_TRIES_MAX} tries")


def tmp(w, root, live_pids, pid_max):
    user = os.environ.get("USER", "tester")
    hi = min(400_000, pid_max - 1)  # the range before TASK-158, so seeds keep their names
    for n in range(37, 40):  # pytest keeps the newest three sessions
        session = f"{root}/pytest-of-{user}/pytest-{n}"
        for t in range(w.rng.randint(2, 6)):
            w.file(f"{session}/test_{w.rng.choice(['parse', 'cli', 'io'])}{t}0/data.bin",
                   int(w.rng.randint(1, 40) * w.scale * MIB))
        backdate(session, (39 - n) * 0.7 + 0.1)
    for pid in [dead_pid(w, 200_000, hi) for _ in range(4)] + live_pids[:1]:
        w.file(f"{root}/nix-shell-{pid}-{w.rng.randint(10**9, 4 * 10**9)}/rc", 0, "# nix-shell rc\n")
    for pid in [dead_pid(w, 200_000, hi) for _ in range(2)] + live_pids[1:2]:
        w.file(f"{root}/nix-develop-{pid}-{w.rng.randint(10**9, 4 * 10**9)}/rc", 0, "# nix develop rc\n")
    detached = f"{root}/review-target-{w.hexid(6)}"
    target(w, detached, "review", 120, ["debug"])
    backdate(detached, 5)
    w.file(f"{root}/claude-1000/-home-{user}-src-app/{w.hexid(8)}/tasks/b{w.hexid(6)}.output", 0, "ok\n")
    for _ in range(5):
        w.file(f"{root}/org.chromium.Chromium.{w.hexid(6)}/scratch", 64 * 1024)
    for _ in range(6):
        w.file(f"{root}/tmp.{w.hexid(10)}/out.log", int(w.rng.randint(1, 8) * MIB))
    w.file(f"{root}/ubuntu-26.04-desktop-amd64.iso.part", int(150 * w.scale * MIB))


def tmp_bulk(root, seed, count, pid_max, scale, written):
    """count leftovers: 3/4 nix-shell rc dirs, 1/4 nix-develop dirs, all of
    dead owners, plus one ended nix-shell.XXXXXX root holding three ended
    pytest sessions per BULK_PER_ROOT leftovers.

    Its own Random, so the base tree stays what it was before TASK-158.
    Owner pids are drawn below pid_max (a pid above it is filtered, which
    tests nothing) from its top half, which a walk container's own PID
    namespace never reaches; a drawn pid that is alive (possible in the host
    namespace, where pids wrap) is skipped rather than redrawn, and each
    name's suffix is a function of its pid, so the rng stream, and every
    name, depends on the seed alone except for the skipped live ones."""
    w = Writer(random.Random(seed ^ 0x7B0C), scale)
    w.total = written  # TOTAL_BYTES_MAX bounds both writers together
    session_bytes = int(scale * MIB)
    need = count // BULK_PER_ROOT * 3 * session_bytes
    st = os.statvfs(root)
    assert need < st.f_bavail * st.f_frsize, (
        f"--tmp-bulk {count} needs {need / MIB:.0f} MiB, {root} has {st.f_bavail * st.f_frsize / MIB:.0f} MiB free")
    user = os.environ.get("USER", "tester")
    alnum = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    drawn = w.rng.sample(range(pid_max // 2, pid_max), count + count // 8 + 16)
    pids = [p for p in drawn if not os.path.exists(f"/proc/{p}")][:count]
    assert len(pids) == count, f"only {len(pids)} of {len(drawn)} drawn owner pids are dead"
    for i, pid in enumerate(pids):
        suffix = pid * 2654435761 % 4294967291  # a fixed scramble, like nix-shell's random part
        if i % 4:
            d = f"{root}/nix-shell-{pid}-{suffix}"
            w.file(f"{d}/rc", 0, "export x=1\n")
            w.file(f"{d}/env-vars", 0, "")
        else:
            os.makedirs(f"{root}/nix-develop-{pid}-{suffix}")
    for _ in range(count // BULK_PER_ROOT):
        r = f"{root}/nix-shell.{''.join(w.rng.choice(alnum) for _ in range(6))}"
        for session in range(3):
            w.file(f"{r}/pytest-of-{user}/pytest-{session}/test_a0/blob", session_bytes)
        backdate(r, 2 + w.rng.random())
    return w.total


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--home", required=True)
    ap.add_argument("--tmp", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--scale", type=float, default=1.0)
    ap.add_argument("--tmp-bulk", type=int, default=0)
    ap.add_argument("--live-pid", type=int, action="append", default=[])
    a = ap.parse_args()
    assert 0 < a.scale <= 2, "--scale must be in (0, 2]"
    assert len(a.live_pid) == 2, "two live owner pids: one nix-shell, one nix-develop"
    assert 0 <= a.tmp_bulk <= TMP_BULK_MAX, f"--tmp-bulk must be in [0, {TMP_BULK_MAX}]"
    pid_max = read_pid_max()
    assert all(0 < p < pid_max for p in a.live_pid), f"live pids must be below pid_max {pid_max}"
    w = Writer(random.Random(a.seed), a.scale)
    home(w, a.home)
    tmp(w, a.tmp, a.live_pid, pid_max)
    total = tmp_bulk(a.tmp, a.seed, a.tmp_bulk, pid_max, a.scale, w.total)
    print(f"gen: seed {a.seed}, {total / MIB:.0f} MiB written under {a.home} and {a.tmp}")


if __name__ == "__main__":
    main()
