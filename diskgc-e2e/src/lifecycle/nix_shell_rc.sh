# nix_shell_rc_lifecycle_is_pinned (TASK-112 step 1): evidence only, no
# diskgc run. Runs as tester; $1 is the pinned bash. Prints one
# "FACT key=value" line per observed fact; lifecycle_suite.rs compares the
# whole set with what docs/safety.md (nix-shell rc directories) relies on.
#
# The image's /nix/store is read-only and has no Nix database, so both
# commands use a chroot store under the tester's home: legacy nix-shell of
# a derivation without inputs needs no build, and nix develop is given a
# ready dev environment (the JSON get-env would write), so it builds
# nothing either. Each case gets its own TMPDIR, which is where Nix makes
# the rc directory, so one case's leftovers never mix with another's.
# Every wait is a bounded poll.
set -u
BASH_BIN=$1
export HOME=/home/tester NIX_BUILD_SHELL=$BASH_BIN
STORE=/home/tester/nix-root
mkdir -p $STORE /home/tester/lc
printf 'derivation { name = "s"; system = "x86_64-linux"; builder = "/bin/sh"; }\n' > /home/tester/lc/s.nix
printf '{"bashFunctions":{},"variables":{"name":{"type":"exported","value":"s"},"out":{"type":"exported","value":"/home/tester/lc/out"},"outputs":{"type":"exported","value":"out"}}}' > /home/tester/lc/s-env
NIXOPT="--extra-experimental-features nix-command --offline --option build-users-group '' --option substituters '' --store local?root=$STORE"
ENV_PATH=$(eval nix $NIXOPT store add-file /home/tester/lc/s-env) || { echo "FAIL add-file"; exit 1; }
echo "FACT nix_version=$(nix --version | awk '{print $3}')"
LEG="nix-shell --store local?root=$STORE /home/tester/lc/s.nix"
DEV="PATH=$(dirname $BASH_BIN):\$PATH nix $NIXOPT develop $ENV_PATH"
# wait_for FILE: bounded poll (10 s).
wait_for() { i=0; while [ ! -s "$1" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; [ -s "$1" ]; }
# one rc directory in $T: print its name, or nothing
rcdir() { for d in "$T"/nix-shell-[0-9]*-* "$T"/nix-develop-[0-9]*-*; do [ -d "$d" ] && echo "$d"; done; }
namepid() { basename "$1" | awk -F- '{print $3}'; }
contents() { ls -A "$1" 2>/dev/null | tr '\n' ',' ; }
refs() { # does process $1 hold fd/cwd/root inside $2, or name it in its environment?
  for l in /proc/$1/fd/* /proc/$1/cwd /proc/$1/root; do case "$(readlink $l 2>/dev/null)" in "$2"|"$2"/*) echo fd; return;; esac; done
  tr '\0' '\n' < /proc/$1/environ 2>/dev/null | grep -q "=$2\(/\|$\)" && { echo environ; return; }
  echo none; }
# findpid ARGV: bounded poll for the one process whose command line is ARGV
findpid() { i=0; while [ $i -lt 100 ]; do for p in /proc/[0-9]*; do [ "$(tr '\0' ' ' < $p/cmdline 2>/dev/null)" = "$1 " ] && { echo ${p#/proc/}; return; }; done; sleep 0.1; i=$((i+1)); done; echo none; }
envval() { tr '\0' '\n' < /proc/$1/environ | sed -n "s/^$2=//p"; }
gone() { i=0; while [ -d /proc/$1 ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; [ ! -d /proc/$1 ]; }
n=0
for kind in legacy develop; do
  if [ $kind = legacy ]; then CMD=$LEG; else CMD=$DEV; fi
  # 1. non-interactive -c / --run, normal exit
  n=$((n+1)); export T=/tmp/lc$n; mkdir -p $T; export TMPDIR=$T
  if [ $kind = legacy ]; then
    eval "$CMD --run 'echo \$\$ > $T/shellpid; sleep 1'" >/dev/null 2>&1 &
  else
    eval "$CMD -c sh -c 'echo \$\$ > $T/shellpid; sleep 1'" >/dev/null 2>&1 &
  fi
  wait_for $T/shellpid; d=$(rcdir); sp=$(cat $T/shellpid)
  echo "FACT $kind.c.pid_equal=$([ "$(namepid $d)" = "$sp" ] && echo yes || echo no)"
  echo "FACT $kind.c.contents=$(contents $d)"
  wait
  echo "FACT $kind.c.after_exit=$([ -d "$d" ] && echo kept || echo removed)"
  # 2. interactive (pty), setsid child, then exit
  n=$((n+1)); export T=/tmp/lc$n; mkdir -p $T; export TMPDIR=$T; mkfifo $T/in
  script -qc "$CMD" /dev/null < $T/in > /dev/null 2>&1 &
  exec 3> $T/in
  echo 'echo $$ > '$T'/shellpid; setsid sleep 3'$n' </dev/null >/dev/null 2>&1 &' >&3
  wait_for $T/shellpid; cp=$(findpid "sleep 3$n")
  d=$(rcdir); sp=$(cat $T/shellpid)
  echo "FACT $kind.i.pid_equal=$([ "$(namepid $d)" = "$sp" ] && echo yes || echo no)"
  echo "FACT $kind.i.contents=$(contents $d)"
  echo "FACT $kind.i.shell_refs=$(refs $sp $d)"
  echo "FACT $kind.i.child_refs=$(refs $cp $d)"
  echo "FACT $kind.i.child_tmpdir_is_rcdir=$([ "$(envval $cp TMPDIR)" = "$d" ] && echo yes || echo no)"
  echo "FACT $kind.i.shell_tmpdir_is_rcdir=$([ "$(envval $sp TMPDIR)" = "$d" ] && echo yes || echo no)"
  echo 'exit' >&3; exec 3>&-
  gone $sp; sleep 0.3
  echo "FACT $kind.i.after_exit=$([ -d "$d" ] && echo kept || echo removed)"
  echo "FACT $kind.i.child_alive_after_exit=$([ -d /proc/$cp ] && echo yes || echo no)"
  kill $cp 2>/dev/null; wait
  # 3./4. interactive, killed by SIGKILL / SIGHUP
  for sig in KILL HUP; do
    n=$((n+1)); export T=/tmp/lc$n; mkdir -p $T; export TMPDIR=$T; mkfifo $T/in
    script -qc "$CMD" /dev/null < $T/in > /dev/null 2>&1 &
    exec 3> $T/in
    echo 'echo $$ > '$T'/shellpid' >&3
    wait_for $T/shellpid; sp=$(cat $T/shellpid); d=$(rcdir)
    kill -$sig $sp; gone $sp; sleep 0.3
    echo "FACT $kind.$sig.after=$([ -d "$d" ] && echo "kept:$(contents $d)" || echo removed)"
    exec 3>&-; wait
  done
done
echo "FACT done=yes"
