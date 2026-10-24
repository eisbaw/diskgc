#!/usr/bin/env bash
# Host side of `just walk-up`: one disposable rootless Podman container from
# the e2e base image with the 0.1 release package installed, seeded data on
# two size-limited tmpfs mounts (so --min-free bites) and a host-shaped
# process load; then a long-lived tmux session with the tester's shell in it.
# Nothing here touches host data: every write is inside the container.
set -euo pipefail
env_name=${1:-podman}
name=walk-diskgc-$env_name
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
home_size=${WALK_HOME_SIZE:-2560m}
tmp_size=${WALK_TMP_SIZE:-1024m}

if tmux has-session -t "=$name" 2>/dev/null || podman container exists "$name"; then
  echo "walk-up: $name is already up; attach with: tmux attach -t $name (or just walk-down)" >&2
  exit 2
fi

pkg=$(nix build --no-link --print-out-paths "$repo#release-01-candidate")
stream=$(nix build --no-link --print-out-paths "$repo#e2e-base-image")
image=localhost/diskgc-e2e-base:$(nix eval --raw "$repo#e2e-base-image.imageTag")
# Same load-once rule as the e2e runner: the tag is the image's Nix hash.
if ! podman image exists "$image"; then
  "$stream" | podman load --quiet
fi

# seccomp=unconfined and no pids limit: a host has no seccomp filter (the
# default one refuses kcmp, which the in-use scan uses) and the load alone
# has ~2,800 threads. The container ends by itself after a week.
podman run -d --name "$name" --hostname walk --network=none --pull=never \
  --pids-limit=-1 --security-opt seccomp=unconfined \
  --mount "type=tmpfs,destination=/home/tester,tmpfs-size=$home_size,tmpfs-mode=0755" \
  --mount "type=tmpfs,destination=/tmp,tmpfs-size=$tmp_size,tmpfs-mode=1777" \
  -v "$pkg:/opt/diskgc-pkg:ro" -v "$here:/opt/walk:ro" -v "$repo/docs:/opt/diskgc-src/docs:ro" \
  -e WALK_SEED="${WALK_SEED:-1}" -e WALK_BURNERS="${WALK_BURNERS:-2}" \
  "$image" sleep 604800 >/dev/null
podman exec "$name" sh /opt/walk/container-setup.sh

tmux new-session -d -s "$name" -x 200 -y 50 \
  "podman exec -it -u tester -w /home/tester -e HOME=/home/tester -e USER=tester $name /opt/nix-eval/bash --rcfile /opt/walk/bashrc -i"
echo "walk-up: $name ready (release package $pkg); attach with: tmux attach -t $name"
