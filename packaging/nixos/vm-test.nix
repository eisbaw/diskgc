# The NixOS module in a booted guest (docs/testing.md, Deployment units,
# 5), with the release candidate as its package, the package 0.2 ships the
# module with (TASK-362): a reclaiming user instance reclaims a filled
# filesystem down to its --target-free and keeps a held candidate,
# tally-only instances run, refused ones exit 2 and stay failed. The
# development package's sandbox gate is shown on its own binary.
# Everything destructive happens inside this disposable guest.
#
# The base system has no diskgc; the `diskgc` specialisation enables the
# module, so switching to it is a first switch.
# `package` and `devPackage` are the unstamped test builds (flake.nix,
# mkDiskgc, mkReleasePackage): the guest stays cached across commits that
# change no source.
{
  pkgs,
  module,
  package,
  devPackage,
}:
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
  # 0.2's pytest-cache: a project-tree cache that needs no idle time.
  pytestCaches = roots: {
    version = 1;
    groups = [
      {
        name = "caches";
        tiers = [
          {
            name = "pytest-caches";
            select = "profile:pytest-cache";
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
      # mark's reclaiming instance watches a small ext4 of its own.
      virtualisation.emptyDiskImages = [ 256 ];
      virtualisation.fileSystems."/srv/mark" = {
        device = "/dev/vdb";
        fsType = "ext4";
        autoFormat = true;
      };
      environment.systemPackages = [
        pkgs.python3
        pkgs.python3Packages.pytest
      ];
      # /tmp a tmpfs, as on many laptops; no instance watches it.
      boot.tmp.useTmpfs = true;
      users.users.mark = {
        isNormalUser = true;
        linger = true;
      };
      users.users.runner.isNormalUser = true;
      systemd.tmpfiles.rules = [
        "d /home/mark/src 0755 mark users -"
        "d /home/runner/src 0755 runner users -"
        # mark's reclaiming instance's filesystem and root (a supplied
        # root must exist at start, or the instance exits 2).
        "z /srv/mark 0755 mark users -"
        "d /srv/mark/src 0755 mark users -"
      ];

      specialisation.diskgc.configuration = {
        services.diskgc = {
          enable = true;
          inherit package;
          settings = policy [ "/home/runner/src" ];
          instances = {
            # The laptop owner's warm index of /home (tally-only).
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
            # The laptop owner's reclaiming watcher (0.2), on a
            # filesystem of its own: a fill below 40 % reclaims to 50 %.
            reclaim = {
              user = "mark";
              path = "/srv/mark";
              minFree = "40%";
              targetFree = "50%";
              extraArgs = [
                "--interval"
                "2s"
                "--json"
              ];
              settings = pytestCaches [ "/srv/mark/src" ];
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

  testScript = ''
    DEV = "${devPackage}"
  '' + builtins.readFile ./vm-test.py;
}
