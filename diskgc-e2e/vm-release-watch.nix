{ pkgs, release }:

# TASK-362: the release package's reclaiming `watch` on the service path.
# The package is installed in a NixOS guest the way docs/reference.md
# (Watch as a systemd service) tells a generic-Linux user to install it:
# the static binary at /usr/local/bin/diskgc (the templates' ExecStart),
# the user template copied by the user into ~/.config/systemd/user, the
# system template copied by root, an environment file per instance, and
# a configuration. Two deviations, both forced by NixOS keeping /etc's
# systemd and tmpfiles directories read-only (generated): the system
# template and its User= drop-in go to /run/systemd/system (a unit
# directory systemd also reads; the file is the package's, compared byte
# for byte), and the package's tmpfiles rule is provisioned at boot from
# the package (systemd.tmpfiles.packages), as `systemd-tmpfiles --create`
# at every boot would. No NixOS module: vm-nixos-module runs that path.
#
# Each watched filesystem is a small ext4 disk of its own, so the fill
# scenarios move only its available space. Everything destructive happens
# inside this disposable guest; the script is vm-release-watch.py.
pkgs.testers.nixosTest {
  name = "diskgc-release-watch";

  nodes.machine =
    { ... }:
    {
      virtualisation.memorySize = 2048;
      virtualisation.cores = 4;
      # /dev/vdb for alice's user instance, /dev/vdc for the system one.
      virtualisation.emptyDiskImages = [
        256
        192
      ];
      systemd.tmpfiles.packages = [ release ];
      environment.systemPackages = [
        pkgs.e2fsprogs
        pkgs.python3
        pkgs.python3Packages.pytest
        pkgs.util-linux
      ];
      users.users.alice = {
        isNormalUser = true;
        linger = true;
      };
      users.users.runner = {
        isNormalUser = true;
      };
    };

  testScript = ''
    RELEASE = "${release}"
    VERSION = "${release.version}"
  '' + builtins.readFile ./vm-release-watch.py;
}
