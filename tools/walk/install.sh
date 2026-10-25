#!/usr/bin/env bash
# Host side of `just walk-install`: copy a diskgc binary built elsewhere (a
# worktree's build, a candidate artifact) into walk container NAME as
# /usr/local/bin/diskgc-TAG, next to the release package's diskgc, so a walk
# can compare builds without a new walk-up. Prints the installed version.
# Usage: install.sh NAME BIN TAG
set -euo pipefail
(( $# == 3 )) || { echo "usage: walk-install NAME BIN TAG" >&2; exit 2; }
name=walk-diskgc-$1
# result/bin/diskgc is a symlink into the store.
bin=$(readlink -f -- "$2") || { echo "walk-install: cannot resolve $2" >&2; exit 2; }
tag=$3
[[ $tag =~ ^[A-Za-z0-9._-]{1,40}$ ]] || { echo "walk-install: TAG '$tag' must be 1-40 of A-Z a-z 0-9 . _ -" >&2; exit 2; }
[ -f "$bin" ] && [ -x "$bin" ] || { echo "walk-install: $2 is not an executable file" >&2; exit 2; }
podman container exists "$name" || { echo "walk-install: no container $name (just walk-up $1)" >&2; exit 2; }
# Streamed rather than `podman cp`, which maps the source's owner into the
# container and fails on a slot map for store files (owned by the overflow
# uid 65534, which a slot does not map). Rename so a walk never runs half a binary.
dest=/usr/local/bin/diskgc-$tag
podman exec -i "$name" sh -c "cat > '$dest.part' && chmod 0755 '$dest.part' && mv '$dest.part' '$dest'" < "$bin"
echo "walk-install: $name:$dest <- $bin"
podman exec "$name" "$dest" --version
