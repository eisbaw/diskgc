#!/bin/sh
# Inside the walk container, as root, once per walk-up: install the release
# package as a user would (binary on PATH, tmpfiles drop-in applied, example
# config in its doc dir), start the host-shaped process load, then generate
# the data (reset.sh).
set -eu
pkg=/opt/diskgc-pkg
mkdir -p /usr/local/bin /usr/local/share/doc/diskgc/examples /etc/tmpfiles.d /run/walk
install -m 0755 "$pkg/bin/diskgc" /usr/local/bin/diskgc
install -m 0644 "$pkg/lib/tmpfiles.d/diskgc.conf" /etc/tmpfiles.d/diskgc.conf
install -m 0644 "$pkg/share/doc/diskgc/examples/example.yaml" /usr/local/share/doc/diskgc/examples/example.yaml
# The image ships /run/diskgc as an empty mount point; on a booted host it
# exists only once tmpfiles has run, so start from that state.
rmdir /run/diskgc
systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf
# The image's Python (pytest's interpreter) as the user's python3.
py=$(grep -o '/nix/store/[^/"]*-python3-3[.0-9]*' /bin/pytest | head -n 1)
test -x "$py/bin/python3" || { echo "container-setup: no python3 found via /bin/pytest" >&2; exit 2; }
ln -sf "$py/bin/python3" /usr/local/bin/python3
chown tester:tester /home/tester /run/walk
chmod 1777 /tmp

# Host-shaped load as tester (whose processes diskgc must judge) plus a few
# busy loops of another user for CPU pressure, niced so a session left
# running does not starve the host.
su -s /bin/sh tester -c "setsid /usr/local/bin/python3 /opt/walk/load.py /run/walk/load-ready /run/walk/load-scratch >/dev/null 2>&1 &"
b=0
while [ "$b" -lt "${WALK_BURNERS:-2}" ]; do
  su -s /bin/sh other -c "setsid nice -n 19 sh -c 'while :; do :; done' >/dev/null 2>&1 &"
  b=$((b + 1))
done
n=0
until [ -s /run/walk/load-ready ]; do
  sleep 1
  n=$((n + 1))
  [ "$n" -lt 180 ] || { echo "container-setup: load not ready after 180 s" >&2; exit 3; }
done
echo "load: $(cat /run/walk/load-ready) processes, $(ls -d /proc/[0-9]*/task/* | wc -l) tasks"
sh /opt/walk/reset.sh
