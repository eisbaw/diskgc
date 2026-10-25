# vm-release-tmp-reclaim (TASK-156): one user's /tmp cohort, made by the
# real pinned producers while the guest clock is held days in the past, so
# every inode below gets a genuinely old kernel ctime. Runs as that user,
# from the VM test script (under sh -x, so the log shows each step); its
# "COHORT key=value" lines are what the test script reads.
#
# $1 pytest, $2 nix, $3 the bash nix-shell starts (NIX_BUILD_SHELL).
#
# pytest sessions in /tmp/pytest-of-$USER, in this order:
#   pytest-0, pytest-1  killed with SIGKILL mid-test: .lock left, its pid dead
#   pytest-2..4         ended normally: no .lock (pytest keeps its newest 3)
#   pytest-5            live: a test waits for $HOME/release, .lock = its pid
# nix rc directories directly in /tmp:
#   legacy nix-shell killed with SIGKILL (rc left, owner dead)
#   nix develop -c, normal exit (directory left, owner dead)
#   legacy nix-shell and nix develop, both alive (owner pid live)
# plus unrelated files diskgc must never touch.
#
# The store is a chroot store under $HOME, as in lifecycle/nix_shell_rc.sh:
# a derivation without inputs needs no build, and nix develop gets a ready
# dev environment, so neither needs the network or the guest's daemon.
# Every wait is a bounded poll. Every background process is started in a
# subshell that exits at once, so init adopts and reaps it: a zombie still
# carries its pid, and Owner PID rightly counts zombies as alive. Live ones
# are detached (setsid, all fds redirected) so the calling `su` returns.
set -eu
PYTEST=$1 NIX=$2 BASH_BIN=$3
NIX_BIN=$(dirname "$NIX")
unset TMPDIR TEMP TMP
export NIX_BUILD_SHELL=$BASH_BIN
cd "$HOME"
mkdir -p proj lc
cat > proj/test_session.py <<'EOF'
import os
import pathlib
import time


def test_session(tmp_path):
    # Some bytes, so a reclaim's avail_delta_bytes is visible.
    (tmp_path / "payload.bin").write_bytes(os.urandom(1 << 20))
    if os.environ.get("COHORT_MODE", "end") == "end":
        return
    pathlib.Path(os.environ["COHORT_READY"]).write_text(str(os.getpid()))
    release = pathlib.Path(os.environ["COHORT_RELEASE"])
    for _ in range(36000):  # 2 h at 0.2 s; far beyond the test's budget
        if release.exists():
            return
        time.sleep(0.2)
    raise TimeoutError("release file never appeared")
EOF

# wait_for FILE: bounded poll, 30 s.
wait_for() { i=0; while [ ! -s "$1" ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; [ -s "$1" ] || { echo "timeout waiting for $1" >&2; exit 1; }; }
# gone PID: bounded poll, 10 s.
gone() { i=0; while [ -d /proc/$1 ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; [ ! -d /proc/$1 ] || { echo "pid $1 did not end" >&2; exit 1; }; }
run_pytest() { (cd proj && "$PYTEST" -q -p no:cacheprovider test_session.py); }

echo "COHORT pytest_version=$("$PYTEST" --version 2>&1 | awk '{print $2}')"
echo "COHORT nix_version=$("$NIX" --version | awk '{print $3}')"

# Two killed sessions: their locks stay, naming dead pids.
for n in 0 1; do
  rm -f ready$n
  ( COHORT_MODE=wait COHORT_READY=$HOME/ready$n COHORT_RELEASE=$HOME/never \
    setsid sh -c 'cd proj && exec "$0" -q -p no:cacheprovider test_session.py' "$PYTEST" \
    </dev/null >/dev/null 2>&1 & )
  wait_for ready$n; pid=$(cat ready$n)
  kill -KILL "$pid"; gone "$pid"
  echo "COHORT killed_pytest_$n=$pid"
done
# Three ended sessions.
for n in 2 3 4; do run_pytest >/dev/null; done
# The live session.
( COHORT_MODE=wait COHORT_READY=$HOME/ready5 COHORT_RELEASE=$HOME/release \
  setsid sh -c 'cd proj && "$0" -q -p no:cacheprovider test_session.py > ../live-pytest.out 2>&1; echo $? > ../live-pytest.exit' "$PYTEST" \
  </dev/null >/dev/null 2>&1 & )
wait_for ready5
echo "COHORT live_pytest=$(cat ready5)"

# Nix rc directories.
STORE=$HOME/nix-root
printf 'derivation { name = "s"; system = "x86_64-linux"; builder = "/bin/sh"; }\n' > lc/s.nix
printf '{"bashFunctions":{},"variables":{"name":{"type":"exported","value":"s"},"out":{"type":"exported","value":"%s/lc/out"},"outputs":{"type":"exported","value":"out"}}}' "$HOME" > lc/s-env
nixopt() { "$NIX" --extra-experimental-features nix-command --offline --option build-users-group '' --option substituters '' --store "local?root=$STORE" "$@"; }
ENV_PATH=$(nixopt store add-file "$HOME/lc/s-env")
legacy() { "$NIX_BIN/nix-shell" --store "local?root=$STORE" "$HOME/lc/s.nix" "$@"; }
develop() { PATH=$(dirname "$BASH_BIN"):$PATH nixopt develop "$ENV_PATH" "$@"; }
# rcdir_of PID: the rc directory named after PID, or nothing (status 0).
rcdir_of() { for d in /tmp/nix-shell-"$1"-* /tmp/nix-develop-"$1"-*; do if [ -d "$d" ]; then echo "$d"; fi; done; }

# Legacy nix-shell killed by SIGKILL: its EXIT trap never runs.
rm -f lc/killed
( legacy --run "echo \$\$ > $HOME/lc/killed; exec sleep 100000" </dev/null >/dev/null 2>&1 & )
wait_for lc/killed; pid=$(cat lc/killed)
d=$(rcdir_of "$pid"); [ -n "$d" ] || { echo "no rc dir for killed nix-shell $pid" >&2; exit 1; }
kill -KILL "$pid"; gone "$pid"
echo "COHORT dead_nix_shell=$d"
# nix develop, normal exit: Nix leaves the directory behind.
develop -c sh -c "echo \$\$ > $HOME/lc/ended" </dev/null >/dev/null 2>&1
pid=$(cat lc/ended); d=$(rcdir_of "$pid")
[ -n "$d" ] || { echo "no rc dir for ended nix develop $pid" >&2; exit 1; }
echo "COHORT dead_nix_develop=$d"
# Live ones.
( setsid "$NIX_BIN/nix-shell" --store "local?root=$STORE" "$HOME/lc/s.nix" \
    --run "echo \$\$ > $HOME/lc/live-legacy; sleep 100000" </dev/null >/dev/null 2>&1 & )
wait_for lc/live-legacy; pid=$(cat lc/live-legacy)
d=$(rcdir_of "$pid"); [ -n "$d" ] || { echo "no rc dir for live nix-shell $pid" >&2; exit 1; }
echo "COHORT live_nix_shell=$d:$pid"
( PATH=$(dirname "$BASH_BIN"):$PATH setsid "$NIX" --extra-experimental-features nix-command --offline \
    --option build-users-group '' --option substituters '' --store "local?root=$STORE" \
    develop "$ENV_PATH" -c sh -c "echo \$\$ > $HOME/lc/live-develop; exec sleep 100000" \
    </dev/null >/dev/null 2>&1 & )
wait_for lc/live-develop; pid=$(cat lc/live-develop)
d=$(rcdir_of "$pid"); [ -n "$d" ] || { echo "no rc dir for live nix develop $pid" >&2; exit 1; }
echo "COHORT live_nix_develop=$d:$pid"

# Unrelated /tmp entries of this user: a file, a work directory, names
# beside each profile's pattern. None is a candidate of either profile.
U=$(id -un)
echo notes > "/tmp/$U-notes.txt"
mkdir -p "/tmp/$U-work" "/tmp/nix-shell-notapid-$U" "/tmp/nix-shell.$U" "/tmp/pytest-of-$U/notes"
echo data > "/tmp/$U-work/data.txt"
echo rc > "/tmp/nix-shell-notapid-$U/rc"
echo rc > "/tmp/nix-shell.$U/rc"
echo kept > "/tmp/pytest-of-$U/notes/kept.txt"
echo "COHORT sessions=$(cd "/tmp/pytest-of-$U" && ls -A | tr '\n' ',')"
for n in 0 1 2 3 4 5; do
  echo "COHORT lock_$n=$(cat "/tmp/pytest-of-$U/pytest-$n/.lock" 2>/dev/null || echo none)"
done
echo "COHORT done=yes"
