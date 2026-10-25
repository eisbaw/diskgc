#!/usr/bin/env bash
# Prints "SLOT FIRST COUNT" for walk environment NAME: a block of COUNT
# subordinate uids and gids from FIRST on that no other running walk
# container holds, so parallel walk containers never share a host uid for
# any non-root container user (container uids 1..COUNT map there).
# Why: the in-use scan cannot read a process of the user's own uid that lives
# in another user namespace and keeps every candidate such a process could
# use (fail-closed); two containers whose tester maps to one host uid and
# that see each other's processes (--pid=host) then reclaim nothing
# (TASK-65 walk, attempt1).
#
# Allocation: the rootless user namespace (podman unshare's uid_map, which
# newuidmap built from /etc/subuid) maps id 0 to the invoking user and ids
# 1..N to the subordinate range. Slot k covers ids k*SLOT_SIZE+1 ..
# k*SLOT_SIZE+SLOT_SIZE-1, container uid u maps to k*SLOT_SIZE+u. Slot 0 is
# the name "podman" and equals Podman's default rootless map for uids below
# SLOT_SIZE (so today's walk-diskgc-podman keeps its host uids). Any other
# name starts at slot 1 + cksum(name) mod (slots - 1) and probes upwards to
# the first slot no running container labelled diskgc.walk.uid-slot holds.
# Container root (uid 0) stays the invoking user in every slot, so the
# container can read the bind-mounted checkout under a 0700 home.
# Usage: uid-slot.sh NAME
set -euo pipefail
name=$1
# The image's users: root 0, tester 1000, other 1001; nothing in the image is
# owned by another uid (checked 2026-10-25). 2048 leaves headroom and gives
# 32 slots in the usual 65536-id subuid range.
SLOT_SIZE=2048

count_ids() { awk '{ n += $3 } END { print n - 1 }' <<<"$1"; }
uid_ids=$(count_ids "$(podman unshare cat /proc/self/uid_map)")
gid_ids=$(count_ids "$(podman unshare cat /proc/self/gid_map)")
ids=$(( uid_ids < gid_ids ? uid_ids : gid_ids ))
slots=$(( (ids + 1) / SLOT_SIZE ))
(( slots >= 2 )) || { echo "uid-slot: $ids subordinate ids (/etc/subuid, /etc/subgid) give $slots slot(s) of $SLOT_SIZE; need 2 or more" >&2; exit 2; }

print_slot() { echo "$1 $(( $1 * SLOT_SIZE + 1 )) $(( SLOT_SIZE - 1 ))"; }
if [ "$name" = podman ]; then
  print_slot 0
  exit 0
fi
taken=" $(podman ps --filter label=diskgc.walk.uid-slot --format '{{index .Labels "diskgc.walk.uid-slot"}}' | tr '\n' ' ') "
start=$(( $(printf %s "$name" | cksum | cut -d' ' -f1) % (slots - 1) ))
for (( i = 0; i < slots - 1; i++ )); do
  slot=$(( 1 + (start + i) % (slots - 1) ))
  if [[ $taken != *" $slot "* ]]; then
    print_slot "$slot"
    exit 0
  fi
done
echo "uid-slot: all $((slots - 1)) named slots are held by running walk containers:$taken" >&2
exit 3
