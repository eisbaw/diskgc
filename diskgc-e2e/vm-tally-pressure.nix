{ pkgs, image, underTest }:

# TASK-29: the background tally backs off under the guest's own io pressure
# (docs/testing.md `background_tally_backs_off_under_io_pressure`). Two
# private disks throttled by QEMU: vdb holds the fixture (moderate limit, so
# sizing it stays busy on cold metadata), vdc takes a direct-I/O writer that
# saturates its tight limit. Nothing is deleted: the tally is read-only.
let
  probe = pkgs.writeText "diskgc-vm-tally-pressure.py" (builtins.readFile ./vm-tally-pressure.py);
in
pkgs.testers.nixosTest {
  name = "diskgc-tally-pressure";

  nodes.machine = { ... }: {
    boot.kernelParams = [ "diskgc.e2e.tally_pressure=1" ];
    virtualisation.cores = 2;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
    virtualisation.podman.enable = true;
    virtualisation.emptyDiskImages = [
      {
        size = 2048;
        driveConfig.driveExtraOpts."throttling.iops-total" = "200";
      }
      {
        size = 2048;
        driveConfig.driveExtraOpts."throttling.iops-total" = "20";
        driveConfig.driveExtraOpts."throttling.bps-total" = "4194304";
      }
    ];
    environment.systemPackages = [ pkgs.python3 pkgs.util-linux pkgs.e2fsprogs ];
    users.groups.diskgc_tally.gid = 1000;
    users.users.diskgc_tally = {
      isNormalUser = true;
      uid = 1000;
      group = "diskgc_tally";
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    assert machine.execute("${underTest}/bin/diskgc-e2e-canary")[0] == 86
    assert machine.succeed("cat /proc/pressure/io").startswith("some ")
    machine.succeed("${image} | podman load")
    result = machine.succeed(
      "python3 ${probe} ${image.imageName}:${image.imageTag} ${underTest}",
      timeout=1800,
    )
    print(result)
  '';
}
