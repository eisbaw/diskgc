#!/usr/bin/env bash
# Host side of `just walk-up`: one disposable rootless Podman container from
# the e2e base image with the release-candidate package installed, seeded data on
# two size-limited tmpfs mounts (so --min-free bites) and a host-shaped
# process load; then a long-lived tmux session with the tester's shell in it.
# Nothing here touches host data: every write is inside the container.
# Each NAME is its own container walk-diskgc-NAME, tmux session of the same
# name, and uid/gid slot (uid-slot.sh), so walks can run side by side.
# Usage: up.sh [NAME]   (default podman)
set -euo pipefail
env_name=${1:-podman}
if ! [[ $env_name =~ ^[a-z0-9][a-z0-9-]{0,30}$ ]]; then
  echo "walk-up: NAME '$env_name' must be 1-31 of a-z 0-9 - (pass it positionally: just walk-up NAME)" >&2
  exit 2
fi
name=walk-diskgc-$env_name
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
home_size=${WALK_HOME_SIZE:-2560m}
tmp_size=${WALK_TMP_SIZE:-1024m}

if tmux has-session -t "=$name" 2>/dev/null || podman container exists "$name"; then
  echo "walk-up: $name is already up; attach with: tmux attach -t $name (or just walk-down)" >&2
  exit 2
fi

pkg=$(nix build --no-link --print-out-paths "$repo#release-candidate")
stream=$(nix build --no-link --print-out-paths "$repo#e2e-base-image")
image=localhost/diskgc-e2e-base:$(nix eval --raw "$repo#e2e-base-image.imageTag")
# Same load-once rule as the e2e runner: the tag is the image's Nix hash.
if ! podman image exists "$image"; then
  "$stream" | podman load --quiet
fi

slot_line=$(bash "$here/uid-slot.sh" "$env_name")
read -r slot first count <<<"$slot_line"
idmap=(--uidmap 0:0:1 --uidmap "1:$first:$count" --gidmap 0:0:1 --gidmap "1:$first:$count")

# --init: PID 1 reaps, so the holders walk-reset kills do not stay zombies
# (TASK-175: 306 -> 443 processes over ~24 resets without it).
# WALK_PID_HOST=1 joins the host's (initial) PID namespace instead, where
# diskgc's Owner PID checks apply (nix-shell/nix-develop rc dirs, pytest
# locks; docs/safety.md); Podman refuses --init there, and the container
# then sees, and its uid slot matters against, other walk containers.
if [ "${WALK_PID_HOST:-0}" = 1 ]; then pid_mode=(--pid=host); else pid_mode=(--init); fi
# seccomp=unconfined and no pids limit: a host has no seccomp filter (the
# default one refuses kcmp, which the in-use scan uses) and the load alone
# has ~2,800 threads. The container ends by itself after a week.
podman run -d "${pid_mode[@]}" --name "$name" --hostname walk --network=none --pull=never \
  "${idmap[@]}" --label "diskgc.walk.uid-slot=$slot" \
  --pids-limit=-1 --security-opt seccomp=unconfined \
  --mount "type=tmpfs,destination=/home/tester,tmpfs-size=$home_size,tmpfs-mode=0755" \
  --mount "type=tmpfs,destination=/tmp,tmpfs-size=$tmp_size,tmpfs-mode=1777" \
  -v "$pkg:/opt/diskgc-pkg:ro" -v "$here:/opt/walk:ro" -v "$repo/docs:/opt/diskgc-src/docs:ro" \
  -e WALK_SEED="${WALK_SEED:-1}" -e WALK_TMP_BULK="${WALK_TMP_BULK:-0}" -e WALK_TMP_LAPTOP="${WALK_TMP_LAPTOP:-0,0,0}" -e WALK_BURNERS="${WALK_BURNERS:-2}" \
  "$image" sleep 604800 >/dev/null
# uid-slot.sh read the labels before this run; a walk-up started meanwhile
# may have taken the same slot. Both racers then see each other and both
# back out, so a plain retry works.
holders=$(podman ps --filter "label=diskgc.walk.uid-slot=$slot" --format '{{.Names}}' | tr '\n' ' ')
if [ "$holders" != "$name " ]; then
  bash "$here/down.sh" "$env_name" >&2
  echo "walk-up: uid slot $slot was taken meanwhile by: $holders; removed $name, run walk-up again" >&2
  exit 3
fi
podman exec "$name" sh /opt/walk/container-setup.sh

tmux new-session -d -s "$name" -x 200 -y 50 \
  "podman exec -it -u tester -w /home/tester -e HOME=/home/tester -e USER=tester $name /opt/nix-eval/bash --rcfile /opt/walk/bashrc -i"
echo "walk-up: $name ready (uid slot $slot: ${idmap[*]}; release package $pkg); attach with: tmux attach -t $name"
