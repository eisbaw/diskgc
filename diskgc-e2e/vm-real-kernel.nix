{ pkgs, image, underTest }:

# TASK-304: the Linux side of the loop identity, btrfs membership, idmapped,
# hidepid and overlay rules on a real kernel (docs/testing.md, VM checks).
# Rootless containers can create none of these. The guest is root, so it
# makes loop devices, btrfs on two loop members, an idmapped bind, a hidepid
# /proc, an overlay over a loop-mounted squashfs and (out-of-tree module)
# bcachefs; udisks attaches an ISO for an ordinary user.
#
# Every diskgc run, the development package, is inside one privileged
# rootful Podman container of the interlocked VM image, so the sandbox gate
# admits its reclaims (both markers). The container shares the guest's PID
# namespace (--pid=host), so the development package's in-use check sees
# every guest process and its PID-namespace rule passes as on a host; it is
# privileged and shares the guest's /dev so it can attach loop devices and
# see their partitions. All fixtures and mounts are made inside that
# container's mount namespace, on a guest ext4 directory bound at /work, so
# the backing names the kernel prints are the container's own paths.
let
  # Every tool a scenario runs inside the container, from the guest's
  # read-only Nix store (the image itself has only BusyBox and strace).
  tools = pkgs.buildEnv {
    name = "diskgc-vm-real-kernel-tools";
    paths = [
      pkgs.util-linux # mount --map-users (2.39+), losetup -P, sfdisk
      pkgs.btrfs-progs
      pkgs.bcachefs-tools
      pkgs.squashfsTools
      pkgs.e2fsprogs
      pkgs.coreutils
      pkgs.findutils # find -printf for the deletion oracle
    ];
    pathsToLink = [ "/bin" "/sbin" ];
  };
  # A casper-like live medium: the root squashfs under casper/, on an
  # ISO 9660 image the guest sees as a CD-ROM (/dev/sr0), as an Ubuntu live
  # session sees its boot medium. Also the ISO an ordinary user attaches
  # with udisksctl (scenario 5).
  liveIso = pkgs.runCommand "diskgc-vm-live.iso"
    {
      nativeBuildInputs = [ pkgs.squashfsTools pkgs.xorriso ];
    } ''
    mkdir -p root/home/tester root/var/lib/snapd/snaps root/snap/core/1 root/etc iso/casper
    echo diskgc-live > root/etc/hostname
    mksquashfs root iso/casper/filesystem.squashfs -quiet -noappend -all-root
    xorriso -as mkisofs -quiet -r -V DISKGC_LIVE -o $out iso
  '';
in
pkgs.testers.nixosTest {
  name = "diskgc-real-kernel";

  nodes.machine = { ... }: {
    boot.kernelParams = [ "diskgc.e2e.real_kernel=1" ];
    # bcachefs left mainline in 6.18; NixOS builds the out-of-tree module
    # for the guest's kernel (scenario 7 says which it ran on).
    boot.supportedFilesystems = {
      btrfs = true;
      bcachefs = true;
    };
    boot.kernelModules = [ "loop" "btrfs" "squashfs" "overlay" "isofs" "bcachefs" ];
    virtualisation.cores = 2;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 3072;
    virtualisation.podman.enable = true;
    virtualisation.qemu.options = [ "-cdrom ${liveIso}" ];
    services.udisks2.enable = true;
    # udisks lets an active local session attach a loop device
    # (org.freedesktop.udisks2.loop-setup: allow_active yes); the test's
    # `su` has no seat session, so this grants alice the same, nothing more.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if ((action.id == "org.freedesktop.udisks2.loop-setup" ||
             action.id == "org.freedesktop.udisks2.loop-delete-others") &&
            subject.user == "alice") {
          return polkit.Result.YES;
        }
      });
    '';
    environment.systemPackages = [ pkgs.udisks2 pkgs.util-linux ];
    # tester matches the image's tester (uid 1000); alice is the desktop
    # user of scenario 5, outside the disk group like tester.
    users.groups.tester.gid = 1000;
    users.users.tester = {
      isNormalUser = true;
      uid = 1000;
      group = "tester";
    };
    users.users.alice = {
      isNormalUser = true;
      uid = 1002;
    };
  };

  testScript = ''
    IMAGE = "${image}"
    IMAGE_REF = "${image.imageName}:${image.imageTag}"
    UNDER_TEST = "${underTest}"
    TOOLS = "${tools}"
    LIVE_ISO = "${liveIso}"
    BCACHEFS_TOOLS_VERSION = "${pkgs.bcachefs-tools.version}"
  '' + builtins.readFile ./vm-real-kernel.py;
}
