#!/usr/bin/env bash
# One walk step: type COMMAND into the walk session's shell as a user would,
# wait for the prompt to come back, and save what the screen showed (the
# command, its output and its exit code) to DUMP. Clears the pane first so
# each dump holds one step.
# Usage: step.sh DUMP COMMAND   (WALK_ENV, WALK_STEP_TIMEOUT_S optional)
set -euo pipefail
dump=$1
command=$2
session=walk-diskgc-${WALK_ENV:-podman}
timeout_s=${WALK_STEP_TIMEOUT_S:-600}
prompt='^tester@walk:[^$]*\$ ?$'

last_line() { tmux capture-pane -p -t "$session" | sed '/^$/d' | tail -n 1; }

tmux send-keys -t "$session" C-l
tmux clear-history -t "$session"
tmux send-keys -t "$session" -l "$command"
tmux send-keys -t "$session" Enter
started=$SECONDS
sleep 0.3
until last_line | grep -Eq "$prompt"; do
  if (( SECONDS - started > timeout_s )); then
    echo "step: no prompt after ${timeout_s}s; pane saved to $dump" >&2
    tmux capture-pane -p -J -S - -t "$session" > "$dump"
    exit 3
  fi
  sleep 0.2
done
tmux send-keys -t "$session" -l 'echo "[exit $?]"'
tmux send-keys -t "$session" Enter
until tmux capture-pane -p -t "$session" | grep -Eq '^\[exit [0-9]+\]$' && last_line | grep -Eq "$prompt"; do
  (( SECONDS - started <= timeout_s + 10 )) || { echo "step: exit code never shown" >&2; exit 3; }
  sleep 0.1
done
mkdir -p "$(dirname "$dump")"
tmux capture-pane -p -J -S - -t "$session" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' > "$dump"
echo "step: $(( SECONDS - started ))s -> $dump"
cat "$dump"
