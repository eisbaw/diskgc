#!/usr/bin/env bash
# Host side of `just walk-down`: end the walk's tmux session and remove its
# container (and with it every byte the walk wrote: all of it is inside).
set -uo pipefail
name=walk-diskgc-${1:-podman}
tmux kill-session -t "=$name" 2>/dev/null
podman rm --force --time 5 "$name" >/dev/null 2>&1
if tmux has-session -t "=$name" 2>/dev/null || podman container exists "$name"; then
  echo "walk-down: $name is still present" >&2
  exit 1
fi
echo "walk-down: $name removed"
