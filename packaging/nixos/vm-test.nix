# The NixOS module in a booted guest (docs/testing.md, Deployment units,
# 5), with the development package: tally-only instances run, reclaiming
# and refused ones exit 2 and stay failed. Nothing is deleted: the sandbox
# gate refuses deletion in a VM, which is not a test container.
#
# The base system has no diskgc; the `diskgc` specialisation enables the
# module, so switching to it is a first switch.
# `package` is the unstamped test build (flake.nix, mkDiskgc): the guest
# stays cached across commits that change no source.
{ pkgs, module, package }:
let
  policy = roots: {
    version = 1;
    groups = [
      {
        name = "stale";
        tiers = [
          {
            name = "cargo-old";
            select = "profile:cargo-target && filter:idle-30d";
            inherit roots;
          }
        ];
      }
    ];
  };
in
pkgs.testers.nixosTest {
  name = "diskgc-nixos-module";

  nodes.machine =
    { lib, ... }:
    {
      imports = [ module ];
      virtualisation.memorySize = 1536;
      # /tmp is a filesystem of its own, so mark's two user instances
      # watch different filesystems (one watcher per filesystem and user).
      boot.tmp.useTmpfs = true;
      users.users.mark = {
        isNormalUser = true;
        linger = true;
      };
      users.users.runner.isNormalUser = true;
      systemd.tmpfiles.rules = [
        "d /home/mark/src 0755 mark users -"
        "d /home/runner/src 0755 runner users -"
      ];

      specialisation.diskgc.configuration = {
        services.diskgc = {
          enable = true;
          inherit package;
          settings = policy [ "/home/runner/src" ];
          instances = {
            # The laptop owner's watcher; tally-only, since the development
            # package refuses to reclaim here.
            home = {
              user = "mark";
              path = "/home";
              tallyOnly = true;
              settings = policy [ "/home/mark/src" ];
            };
            # mark's second watcher of the root filesystem: one of the
            # two exits 2.
            second = {
              user = "mark";
              path = "/var/tmp";
              tallyOnly = true;
              settings = policy [ "/home/mark/src" ];
            };
            # A reclaiming instance on the development package: exit 2,
            # naming the sandbox gate.
            reclaim = {
              user = "mark";
              path = "/tmp";
              minFree = "1M";
              targetFree = "2M";
              settings = policy [ "/home/mark/src" ];
            };
            # The CI operator's warm index.
            runner = {
              scope = "system";
              user = "runner";
              path = "/home/runner";
              tallyOnly = true;
            };
            # Root with --allow-root and the module's capability set.
            root = {
              scope = "system";
              user = "root";
              path = "/";
              allowRoot = true;
              tallyOnly = true;
              settings = policy [ "/home/runner/src" ] // {
                owners = [ "runner" ];
              };
            };
            # Root without --allow-root: exit 2.
            rootNoAllow = {
              scope = "system";
              user = "root";
              path = "/nix/store";
              tallyOnly = true;
            };
            # --allow-root without CAP_SYS_PTRACE (below): exit 2.
            rootNoPtrace = {
              scope = "system";
              user = "root";
              path = "/var";
              allowRoot = true;
              tallyOnly = true;
              settings = policy [ "/home/runner/src" ] // {
                owners = [ "runner" ];
              };
            };
          };
        };
        systemd.services."diskgc-watch@var".serviceConfig.CapabilityBoundingSet =
          lib.mkForce "CAP_DAC_OVERRIDE CAP_FOWNER CAP_CHECKPOINT_RESTORE";
      };
    };

  testScript = builtins.readFile ./vm-test.py;
}
