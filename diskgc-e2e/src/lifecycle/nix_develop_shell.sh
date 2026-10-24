# nix_shell_rc_shell_in_private_pid_namespace_is_kept (TASK-112 step 4):
# starts one pinned `nix develop -c` shell as tester and leaves it running,
# holding nothing in its rc directory (nix_shell_rc_lifecycle_is_pinned:
# develop.i.shell_refs=none). $1 is the pinned bash, $2 the file the shell
# writes its pid to, as its own PID namespace sees it ($$, which is the
# pid in the rc directory's name). The chroot store and the ready dev
# environment are those of nix_shell_rc.sh, so nothing is built.
# TMPDIR is unset: Nix makes the rc directory directly in /tmp.
set -eu
BASH_BIN=$1
PIDFILE=$2
export HOME=/home/tester NIX_BUILD_SHELL=$BASH_BIN
unset TMPDIR
STORE=/home/tester/nix-root
mkdir -p $STORE /home/tester/lc
printf '{"bashFunctions":{},"variables":{"name":{"type":"exported","value":"s"},"out":{"type":"exported","value":"/home/tester/lc/out"},"outputs":{"type":"exported","value":"out"}}}' > /home/tester/lc/s-env
NIXOPT="--extra-experimental-features nix-command --offline --option build-users-group '' --option substituters '' --store local?root=$STORE"
ENV_PATH=$(eval nix $NIXOPT store add-file /home/tester/lc/s-env)
PATH=$(dirname "$BASH_BIN"):$PATH
export PATH
# exec: this process becomes nix, which makes the directory and execs the
# shell; the shell's `-c` command execs sleep. One pid throughout.
eval "exec nix $NIXOPT develop $ENV_PATH -c /bin/sh -c 'echo \$\$ > $PIDFILE; exec sleep 600'"
