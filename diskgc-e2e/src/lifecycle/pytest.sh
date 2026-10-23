# pytest_lifecycle_is_pinned (TASK-112 step 1): evidence only, no diskgc
# run. Runs as tester with the image's pinned pytest. Prints one
# "FACT key=value" line per observed fact; lifecycle_suite.rs compares the
# whole set with what docs/safety.md (pytest sessions) relies on: the lock
# by retention, the exit cleanup by rank (N <= M - 3), pytest's own
# three-day dead-lock rule, keep 0 deleting a concurrent lockless session,
# and --basetemp making no lock yet being cleaned like a session.
# Every wait is a bounded poll.
set -u
PYTEST=/opt/pytest-under-test
export HOME=/home/tester PYTEST_DISABLE_PLUGIN_AUTOLOAD=1
W=/home/tester/lc; mkdir -p $W; cd $W
# The test records its session's entries and the lock's bytes, then sleeps.
cat > test_s.py <<'PY'
import os, time
def test_s(tmp_path):
    s = tmp_path.parent
    lock = s / ".lock"
    rec = "%d|%s|%s" % (os.getpid(), lock.read_bytes().hex() if lock.exists() else "none", ",".join(sorted(os.listdir(s))))
    with open(os.environ["REC"], "w") as f:
        f.write(rec)
    time.sleep(float(os.environ.get("SLEEP", "0")))
PY
run() { tag=$1; shift; REC=$W/rec-$tag $PYTEST -q -p no:cacheprovider "$@" test_s.py >/dev/null 2>&1; }
runbg() { tag=$1; shift; REC=$W/rec-$tag $PYTEST -q -p no:cacheprovider "$@" test_s.py >/dev/null 2>&1 & }
wait_for() { i=0; while [ ! -s "$1" ] && [ $i -lt 150 ]; do sleep 0.1; i=$((i+1)); done; [ -s "$1" ]; }
sessions() { ls -A "$1" | tr '\n' ',' ; }
hexpid() { printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
echo "FACT pytest_version=$($PYTEST --version 2>&1 | awk '{print $2}')"
# 1. default retention: the lock holds the pid as text, no newline; removed at exit; cleanup by rank N <= M - 3
export TMPDIR=/tmp/p1; mkdir -p $TMPDIR; P=$TMPDIR/pytest-of-tester
SLEEP=2 runbg a0; wait_for $W/rec-a0
rec=$(cat $W/rec-a0); pid=${rec%%|*}; rest=${rec#*|}; lockhex=${rest%%|*}
echo "FACT default.lock_is_pid_text=$([ "$lockhex" = "$(hexpid $pid)" ] && echo yes || echo no)"
wait
echo "FACT default.lock_after_exit=$([ -e $P/pytest-0/.lock ] && echo present || echo absent)"
for i in 1 2 3 4; do run d$i; done
echo "FACT default.sessions_after_5=$(sessions $P)"
# 2. a killed session keeps its lock; below the rank a young lock is kept, a lock older than 3 days is not
SLEEP=30 runbg k; wait_for $W/rec-k; kpid=$(cut -d'|' -f1 $W/rec-k); K=$(ls -d $P/pytest-* | grep -v current | sort -t- -k3 -n | tail -1)
kill -9 $kpid; sleep 0.3
echo "FACT killed.lock_left=$([ -f $K/.lock ] && echo yes || echo no)"
for i in 1 2 3; do run e$i; done
echo "FACT killed.young_lock_below_rank=$([ -d $K ] && echo kept || echo removed)"
touch -d "@$(( $(date +%s) - 4*86400 - 60 ))" $K/.lock
run e4
echo "FACT killed.old_lock_below_rank=$([ -d $K ] && echo kept || echo removed)"
# 3. keep 0 (retention policy none): no lock, and its exit cleanup deletes a concurrent lockless live session
export TMPDIR=/tmp/p2; mkdir -p $TMPDIR; P=$TMPDIR/pytest-of-tester
run f1
# The live session sleeps far longer than the second run takes, and is
# killed once observed, so the overlap does not depend on timing.
SLEEP=60 runbg live -o tmp_path_retention_policy=none; wait_for $W/rec-live
lpid=$(cut -d'|' -f1 $W/rec-live)
echo "FACT keep0.lock=$(cut -d'|' -f2 $W/rec-live)"
run k0 -o tmp_path_retention_policy=none
echo "FACT keep0.after=$(sessions $P)"
echo "FACT keep0.live_session_survived=$([ -d $P/pytest-1 ] && echo yes || echo no)"
echo "FACT keep0.live_still_running=$([ -d /proc/$lpid ] && echo yes || echo no)"
kill $lpid; wait
# 4. --basetemp: no lock, no number; shaped like a session at N <= M - 3, a later session's cleanup deletes it
export TMPDIR=/tmp/p3; mkdir -p $TMPDIR; P=$TMPDIR/pytest-of-tester
for i in 1 2 3 4; do run g$i; done
echo "FACT basetemp.before=$(sessions $P)"
SLEEP=1 runbg bt --basetemp=$P/pytest-0; wait_for $W/rec-bt
echo "FACT basetemp.lock=$(cut -d'|' -f2 $W/rec-bt)"
wait
echo "FACT basetemp.after_run=$(sessions $P)"
run g5
echo "FACT basetemp.after_next_session=$(sessions $P)"
echo "FACT done=yes"
