# diskgc

diskgc keeps enough free space on a filesystem for unattended builds by
deleting rebuildable cache that, as far as it can tell, nothing is using:
Cargo `target/` directories, and the leftovers of ended pytest sessions and
nix-shell runs in `/tmp`. `ensure` brings one filesystem up to a free-space
goal once and exits with a code a CI hook or an agent script can branch on.

## This package

- `bin/diskgc`: a static binary with no run-time dependencies.
- `lib/tmpfiles.d/diskgc.conf`: creates the host lock `/run/diskgc/lock`,
  through which the reclaims of every user take turns. Root needs it; a
  user's reclaim without it takes a per-user lock of its own instead.
- `share/doc/diskgc/`: this README; `RELEASE-NOTES.md` (what this version
  does, install, known limits, the JSON contract); `reference.md` (commands,
  exit codes, JSON output, configuration, profiles); `safety.md` (every
  check made before anything is deleted); `examples/example.yaml`.

## Install

As root, from the unpacked package (a minimal system may have no
`/etc/tmpfiles.d` yet; `install -D` creates it):

    install -Dm0755 bin/diskgc /usr/local/bin/diskgc
    install -Dm0644 lib/tmpfiles.d/diskgc.conf /etc/tmpfiles.d/diskgc.conf
    systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf

Without systemd-tmpfiles, create what `diskgc.conf` describes at every
boot. The tmpfiles steps are needed only for root and system services.
Without them, and without root, put the binary anywhere on your `PATH`:
a reclaim then takes a per-user lock in `$XDG_RUNTIME_DIR/diskgc`
(otherwise in `~/.local/state/diskgc`), which serialises your own
reclaims but not other users' (`safety.md`, Host lock).

## First run

As the user whose files diskgc may delete (it never needs root):

    mkdir -p ~/.config/diskgc
    cp share/doc/diskgc/examples/example.yaml ~/.config/diskgc/config.yaml
    # replace the example's Cargo root with directories you own, then:
    diskgc check
    diskgc ensure --dry-run --min-free 20G ~

A dry run reads only: it lists what a reclaim would take and why the rest
is kept. `--min-free` must be below the filesystem's size. Without
`--dry-run`, `ensure` deletes what the configuration selects and the safety
checks allow, until the goal is met. Read "Known limits" in
`RELEASE-NOTES.md` first: in particular, diskgc running in its own PID
namespace (most containers) keeps nearly every `/tmp` candidate, and without root
it sees only your own processes.

`diskgc --help` lists the commands, sizes, durations, exit codes and the
built-in profiles; `diskgc profiles show NAME` explains one profile.
