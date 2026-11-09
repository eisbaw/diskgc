# diskgc

diskgc keeps enough free space on a filesystem for unattended builds by
deleting rebuildable cache that, as far as it can tell, nothing is using:
Cargo `target/` directories, the leftovers of ended pytest sessions and
nix-shell runs in `/tmp`, the dead paths of a multi-user Nix store
(through the system Nix daemon's own garbage collector), and in the
project trees you name, pytest, Python bytecode and mypy caches and
Python virtual environments idle for 30 days, plus Node's compile cache
in `/tmp`, and the pip, uv, pnpm and ccache
caches, emptied by each tool's own cleaner. `ensure` brings one filesystem up to a free-space
goal once and exits with a code a CI hook or an agent script can branch on.
Unlike age-only cleanup (`find -mtime +N -delete`, systemd-tmpfiles age
rules), diskgc deletes only when free space is short, and checks every
candidate just before deleting it: it keeps any candidate a process it can
see has open, mapped or as its working directory, however old (`safety.md`,
In use).

## This package

- `bin/diskgc`: a static binary with no run-time dependencies.
- `lib/tmpfiles.d/diskgc.conf`: creates the host lock `/run/diskgc/lock`,
  through which the reclaims of every user take turns. Root needs it; a
  user's reclaim without it takes a per-user lock of its own instead.
- `lib/systemd/user/diskgc-watch@.service` and
  `lib/systemd/system/diskgc-watch@.service`: the `watch` service
  templates, one instance per filesystem (`reference.md`, Watch as a
  systemd service). The first lines of `RELEASE-NOTES.md` say which
  `watch` this package runs: 0.2 runs a reclaiming one (below
  `--min-free` it reclaims as `ensure` does, up to `--target-free`) and
  `--tally-only`, which keeps the index warm and never deletes. The
  thresholds go in the instance's `DISKGC_WATCH_ARGS`; a dry run with
  `ensure --dry-run` and the same `--min-free` shows what a cycle would
  take. On NixOS, use the flake's `nixosModules.default` at this
  release's tag instead (`RELEASE-NOTES.md`, Install).
- `share/doc/diskgc/`: this README; `RELEASE-NOTES.md` (what this version
  does, install, known limits, the JSON contract); `reference.md` (commands,
  exit codes, JSON output, configuration, profiles); `safety.md` (every
  check made before anything is deleted); `examples/example.yaml`;
  `LICENSE`.

## Install

As root, from the unpacked package (a minimal system may have no
`/etc/tmpfiles.d` yet; `install -D` creates it):

    install -Dm0755 bin/diskgc /usr/local/bin/diskgc
    install -Dm0644 lib/tmpfiles.d/diskgc.conf /etc/tmpfiles.d/diskgc.conf
    systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf

Without systemd-tmpfiles, create what `diskgc.conf` describes at every
boot. The tmpfiles steps are needed only for root and system services.
Without them, and without root, put the binary anywhere on your `PATH`:
a reclaim then takes a per-user lock of its own, which serialises your
reclaims but not other users' (`safety.md`, Host lock).

## First run

As the user whose files diskgc may delete (it never needs root):

    mkdir -p ~/.config/diskgc
    cp share/doc/diskgc/examples/example.yaml ~/.config/diskgc/config.yaml
    # replace the example's Cargo root with directories you own, then:
    diskgc check
    diskgc ensure --dry-run --min-free 20G ~

A dry run reads only: it lists what a reclaim would take and why the rest
is kept. `--min-free` must be below the filesystem's size.

A real dry run of the two `/tmp` built-ins on a development laptop
(release candidate 0.2.0-dev, 2026-11-04, about 217,000 entries directly
in `/tmp`; `--min-free` above the free space, so that it plans everything;
trimmed at `…`). The `diskgc:` lines are stderr; the rest is the plan:

    $ diskgc ensure --dry-run --min-free 200G --select 'profile:pytest-tmp || profile:nix-shell-rc' /tmp
    diskgc: note: the host lock /run/diskgc/lock is not provisioned: a reclaim by this user takes the per-user lock /run/user/1000/diskgc/lock instead, which does not serialise it with other users' reclaims (docs/safety.md, Host lock)
    diskgc: select: `profile:pytest-tmp || profile:nix-shell-rc`: 146976 distinct filesystem candidate(s) from 2 source term(s)
    diskgc: process scan: 302 processes of other users not inspected (their use of the user's files is invisible without root)
    …
    dry run: 30.0 GiB available on / (min-free 200.0 GiB, target-free 200.0 GiB); would reclaim 146455 of 146455 eligible, est. 974.5 MiB of 170.0 GiB needed: short of the goal by 169.1 GiB by estimate; 521 kept (took 174.349 s):
      profile pytest-tmp: 387 found · 52 eligible est. 113.5 MiB (would reclaim 52) · kept 331 future_time est. 3.5 GiB, 3 contains_special unsized, 1 in_use est. 14.9 MiB
      profile nix-shell-rc: 146589 found · 146403 eligible est. 861.0 MiB (would reclaim 146403) · kept 100 not_idle est. 784.0 KiB, 16 in_use est. 88.0 KiB, 69 producer_unknown est. 348.0 KiB, 1 contains_ipc unsized
      would reclaim /tmp/nix-develop-3984307-1139244087: est. 4.0 KiB
      …
      146445 more would reclaim: est. 974.4 MiB (--json lists every candidate)
      …
      100 kept: not_idle; examples: /tmp/nix-develop-109870-3793584568, /tmp/nix-develop-113998-4201987292, /tmp/nix-develop-11572-3741847121, /tmp/nix-develop-118551-346712868, /tmp/nix-develop-119955-2853012467 (95 more); e.g. newest change 25 m ago; idle_for 1h required
      …
      kept /tmp/nix-shell-1648100-3410698510: contains_ipc, a Socket (any socket or FIFO keeps its candidate)
      operations: nix-shell-rc: delete; pytest-tmp: delete
    …

Without `--dry-run`, `ensure` deletes what the configuration selects and
the safety checks allow, until the goal is met. A reclaim on a large `/tmp`
can outlast the default `--timeout 10m` and stop with exit 124 before the
goal is met; give it a longer `--timeout`. Read "Known limits" in
`RELEASE-NOTES.md` first: in particular, diskgc in its own PID namespace
(most containers) refuses `ensure` and its dry run with exit 2 (run it on
the host), and without root it sees only your own processes.

`diskgc --help` lists the commands, sizes, durations, exit codes and the
built-in profiles; `diskgc profiles show NAME` explains one profile.

## License

MIT; see `LICENSE`.
