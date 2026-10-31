{ pkgs, release, versionLine }:

# TASK-156: the release package (the 0.1 catalog), installed in a NixOS guest as a user
# installs it (the static binary on PATH, its tmpfiles.d drop-in, its example
# config), reclaims real pytest sessions and nix rc directories in /tmp that
# are genuinely days old. The release binary has no test clock offset, and
# a rootless container cannot backdate ctime, so the guest's wall clock is
# held five days back while the pinned producers make the cohorts
# (vm-historical-ctime's technique), then restored before diskgc runs.
# Everything destructive happens inside this disposable guest.
let
  cohort = pkgs.writeText "diskgc-vm-release-tmp-cohort.sh"
    (builtins.readFile ./vm-release-tmp-reclaim-cohort.sh);
in
pkgs.testers.nixosTest {
  name = "diskgc-release-tmp-reclaim";

  nodes.machine = { ... }: {
    boot.kernelParams = [ "diskgc.e2e.release_tmp_reclaim=1" ];
    # tmpfs /tmp, as on many laptops; Owner PID accepts tmpfs.
    boot.tmp.useTmpfs = true;
    boot.tmp.tmpfsSize = "1G";
    virtualisation.memorySize = 2048;
    # The way a NixOS user installs the package: the binary on PATH and its
    # tmpfiles.d drop-in, which provisions /run/diskgc/lock at boot.
    environment.systemPackages = [ release ];
    systemd.tmpfiles.packages = [ release ];
    users.users.alice.isNormalUser = true;
    users.users.bob.isNormalUser = true;
  };

  testScript = ''
    RELEASE = "${release}"
    VERSION = "${release.version}"
    VERSION_LINE = "${versionLine}"
    COHORT = "${cohort}"
    PYTEST = "${pkgs.python3Packages.pytest}/bin/pytest"
    NIX = "${pkgs.nix}/bin/nix"
    BASH = "${pkgs.bash}/bin/bash"
  '' + builtins.readFile ./vm-release-tmp-reclaim.py;
}
