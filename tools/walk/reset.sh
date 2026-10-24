#!/bin/sh
# Inside the walk container, as root: stop the data's live processes, wipe
# the tester's home and /tmp (both container-private tmpfs mounts), and
# generate them again from the seed (WALK_SEED, default 1). Then start the
# live work a developer's machine has: a Cargo build in progress (holding
# its target's locks), a binary running from its own target, a live pytest
# session, and the owner shells of one nix-shell and one nix-develop dir.
# Each holder waits at most HOLD_S, so nothing outlives a forgotten walk.
set -eu
HOLD_S=43200
holders=/run/walk/holders
cargo_path=/opt/cargo-under-test/1.99.1/bin:/opt/cargo-under-test/cc/bin:/bin

# Every holder is a session leader (setsid) whose pid is in $holders.
if [ -f "$holders" ]; then
  while read -r pid; do kill -TERM -- "-$pid" 2>/dev/null || true; done < "$holders"
  : > "$holders"
fi
sleep 1
find /home/tester /tmp -mindepth 1 -delete
chown tester:tester /run/walk

as_tester() { su -s /bin/sh tester -c "$1"; }
# Starts $1 as tester in a session of its own and records the session
# leader's pid (busybox setsid and su exec in place, so with `exec` in $1
# the pid is the command's own).
hold() {
  setsid su -s /bin/sh tester -c "cd /home/tester && export PATH=$cargo_path CARGO_NET_OFFLINE=true && $1" >/dev/null 2>&1 &
  echo $! >> "$holders"
}

hold "exec sleep $HOLD_S"   # a nix-shell's shell
hold "exec sleep $HOLD_S"   # a nix develop's shell
sleep 1
set -- $(cat "$holders")
as_tester "/usr/local/bin/python3 /opt/walk/gen.py --home /home/tester --tmp /tmp --seed ${WALK_SEED:-1} --live-pid $1 --live-pid $2"

# Real Cargo for the live cases: a web app built and running from its
# target, and an agent's build stuck in a build script, holding the locks.
mkdir -p /home/tester/src/webapp/src /home/tester/src/agent-slow/src
cat > /home/tester/src/webapp/Cargo.toml <<'T'
[package]
name = "webapp"
version = "0.1.0"
edition = "2021"
T
cat > /home/tester/src/webapp/src/main.rs <<T
fn main() {
    // A dev server: runs until the walk's holder bound.
    std::thread::sleep(std::time::Duration::from_secs($HOLD_S));
}
T
cat > /home/tester/src/agent-slow/Cargo.toml <<'T'
[package]
name = "agent-slow"
version = "0.1.0"
edition = "2021"
build = "build.rs"
T
echo 'fn main() {}' > /home/tester/src/agent-slow/src/main.rs
cat > /home/tester/src/agent-slow/build.rs <<T
fn main() {
    // A build that is still running: Cargo holds the target's locks.
    std::thread::sleep(std::time::Duration::from_secs($HOLD_S));
}
T
chown -R tester:tester /home/tester/src/webapp /home/tester/src/agent-slow
as_tester "cd /home/tester/src/webapp && PATH=$cargo_path CARGO_NET_OFFLINE=true cargo build -q"
hold "exec /home/tester/src/webapp/target/debug/webapp"
hold "cd src/agent-slow && exec cargo build -q"

# A live pytest session in /tmp/pytest-of-tester.
mkdir -p /home/tester/src/pyproj
cat > /home/tester/src/pyproj/test_slow.py <<T
import time


def test_long_running(tmp_path):
    (tmp_path / "fixture.txt").write_text("in use")
    time.sleep($HOLD_S)
T
chown -R tester:tester /home/tester/src/pyproj
hold "cd src/pyproj && exec pytest -q"

sleep 3
echo "reset: seed ${WALK_SEED:-1}; holders: $(tr '\n' ' ' < "$holders")"
df -h /home/tester /tmp | sed 's/^/reset: /'
