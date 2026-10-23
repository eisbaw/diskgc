{ pkgs, image, underTest }:

# TASK-70: the first plan observes a dropped guest VFS/page cache on a private
# ext4 disk. The second plan is a warm control. Neither invocation deletes.
let
  probe = pkgs.writeText "diskgc-vm-cold-scale.py" (builtins.readFile ./vm-cold-scale.py);
in
pkgs.testers.nixosTest {
  name = "diskgc-cold-ext4-scale";

  nodes.machine = { ... }: {
    boot.kernelParams = [ "diskgc.e2e.cold_scale=1" ];
    virtualisation.cores = 2;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 4096;
    virtualisation.podman.enable = true;
    environment.systemPackages = [ pkgs.python3 pkgs.util-linux ];
    users.groups.diskgc_scale.gid = 1000;
    users.users.diskgc_scale = {
      isNormalUser = true;
      uid = 1000;
      group = "diskgc_scale";
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    assert machine.execute("${underTest}/bin/diskgc-e2e-canary")[0] == 86
    machine.succeed("${image} | podman load")
    result = machine.succeed(
      "python3 ${probe} ${image.imageName}:${image.imageTag} ${underTest} ${pkgs.time}/bin/time",
      timeout=1500,
    )
    print(result)
  '';
}
