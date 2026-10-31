# Evaluation checks of the NixOS module (docs/testing.md, Deployment
# units, 4). No VM: each example is a NixOS evaluation, and what is
# asserted is what the module generates (the unit drop-ins, /etc entries,
# the tmpfiles packages, assertions and warnings), plus the drop-ins as
# systemd reads them on top of the packaged templates.
{
  pkgs,
  lib,
  nixpkgs,
  module,
  package,
  # verify MANAGER DIR FILE...: systemd-analyze verify of FILEs with only DIR
  # and systemd's own units on the unit path; output to DIR.log (flake.nix).
  systemdVerify,
}:
let
  evalFull =
    extra:
    nixpkgs.lib.nixosSystem {
      modules = [
        module
        {
          nixpkgs.pkgs = pkgs;
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          system.stateVersion = "26.05";
        }
        extra
      ];
    };
  evalSystem = extra: (evalFull extra).config;

  # A small valid policy (the shape of docs/example.yaml).
  policy = roots: {
    version = 1;
    groups = [
      {
        name = "stale";
        tiers = [
          {
            name = "tmp-pytest-sessions";
            select = "profile:pytest-tmp && filter:idle-1d";
          }
          {
            name = "cargo-old";
            select = "profile:cargo-target && filter:idle-30d";
            inherit roots;
          }
        ];
      }
    ];
  };

  # The laptop owner's story, plus a user instance with its own settings
  # and a quoted --select.
  laptop = evalSystem {
    users.users.mark = {
      isNormalUser = true;
      linger = true;
    };
    services.diskgc = {
      enable = true;
      settings = policy [ "/home/mark/src" ];
      instances.home = {
        path = "/home/mark";
        minFree = "30G";
        targetFree = "60G";
        user = "mark";
      };
      instances.scratch = {
        path = "/scratch";
        user = "mark";
        minFree = "10G";
        targetFree = "20G";
        settings = policy [ "/scratch/src" ];
        extraArgs = [
          "--select"
          "profile:cargo-target && filter:idle-7d"
          "--roots"
          "/scratch/src"
        ];
      };
      # systemd's specifiers, variables, quotes and backslashes in one
      # argument reach diskgc literally.
      instances.escape = {
        path = "/srv/escape";
        user = "mark";
        tallyOnly = true;
        extraArgs = [
          "--select"
          "%h $HOME \"q\" back\\slash"
        ];
      };
    };
  };

  # The CI operator: a system tally-only instance, and a root instance.
  ci = evalSystem {
    users.users.gitlab-runner = {
      isSystemUser = true;
      group = "gitlab-runner";
      home = "/home/gitlab-runner";
      createHome = true;
    };
    users.groups.gitlab-runner = { };
    services.diskgc = {
      enable = true;
      settings = policy [ "/home/gitlab-runner/builds" ];
      instances.runner = {
        scope = "system";
        user = "gitlab-runner";
        path = "/srv/ci-builds";
        tallyOnly = true;
        minFree = "50G";
      };
      instances.root = {
        scope = "system";
        user = "root";
        path = "/";
        allowRoot = true;
        tallyOnly = true;
        settings = policy [ "/home/gitlab-runner/builds" ] // {
          owners = [ "gitlab-runner" ];
        };
      };
      # The global file named explicitly: it restarts on its change too.
      instances.optin = {
        scope = "system";
        user = "gitlab-runner";
        path = "/srv/other";
        tallyOnly = true;
        configFile = "/etc/diskgc/config.yaml";
      };
    };
  };

  # A user who does not linger, and no global settings.
  noLinger = evalSystem {
    users.users.mark.isNormalUser = true;
    services.diskgc = {
      enable = true;
      instances.home = {
        path = "/home";
        user = "mark";
        tallyOnly = true;
      };
    };
  };

  # One evaluation per assertion, each wrong in exactly one way.
  failedAssertions =
    extra:
    let
      config = evalSystem (
        lib.recursiveUpdate {
          users.users.mark.isNormalUser = true;
          services.diskgc.enable = true;
        } extra
      );
    in
    map (a: a.message) (lib.filter (a: !a.assertion) config.assertions);

  userUnit = config: name: config.systemd.user.units."${name}.service";
  systemUnit = config: name: config.systemd.units."${name}.service";
  lines = unit: lib.splitString "\n" unit.text;
  linesWith = prefix: unit: lib.filter (lib.hasPrefix prefix) (lines unit);

  exe = lib.getExe package;
  q = s: "\"${s}\"";
  execLine = args: "ExecStart=" + lib.concatMapStringsSep " " q ([ exe "watch" ] ++ args);

  # The user option's type, as the module declares it.
  userType =
    ((evalFull { }).options.services.diskgc.instances.type.getSubOptions [ ]).user.type;
  # Whether a whole evaluation with a system instance of that user fails.
  systemUnitEvaluates =
    user:
    (builtins.tryEval
      (systemUnit (evalSystem {
        services.diskgc = {
          enable = true;
          instances.x = {
            scope = "system";
            inherit user;
            path = "/srv";
            configFile = "/etc/diskgc/x.yaml";
          };
        };
      }) "diskgc-watch@srv").text
    ).success;

  homeUnit = userUnit laptop "diskgc-watch@home-mark";
  escapeUnit = userUnit laptop "diskgc-watch@srv-escape";
  scratchUnit = userUnit laptop "diskgc-watch@scratch";
  runnerUnit = systemUnit ci "diskgc-watch@srv-ci\\x2dbuilds";
  rootUnit = systemUnit ci "diskgc-watch@-";

  etcEntry = config: name: {
    inherit (config.environment.etc.${name}) mode;
    source = config.environment.etc.${name}.source.name;
  };

  cases = {
    # The user story: no --config (the usual lookup finds the module's
    # /etc/diskgc/config.yaml), the stale .env reset, ConditionUser=.
    home-exec = {
      got = linesWith "ExecStart=" homeUnit;
      want = [
        "ExecStart="
        (execLine [
          "--min-free"
          "30G"
          "--target-free"
          "60G"
          "/home/mark"
        ])
      ];
    };
    home-environment-file = {
      got = linesWith "EnvironmentFile" homeUnit;
      want = [ "EnvironmentFile=" ];
    };
    home-condition-user = {
      got = linesWith "ConditionUser=" homeUnit ++ linesWith "User=" homeUnit;
      want = [ "ConditionUser=mark" ];
    };
    home-no-path-override = {
      got = linesWith "Environment=\"PATH=" homeUnit;
      want = [ ];
    };
    # The positive control of home-no-path-override, and the system
    # instances' parity: the system profile's tools on PATH.
    runner-path = {
      got = map (l: lib.hasInfix "PATH=/run/current-system/sw/bin:" l) (linesWith "Environment=\"PATH=" runnerUnit);
      want = [ true ];
    };
    home-wanted-by = {
      got = homeUnit.wantedBy;
      want = [ "default.target" ];
    };
    home-drop-in = {
      got = homeUnit.overrideStrategy;
      want = "asDropin";
    };
    # The instance's own settings file, and --select with spaces as one
    # argument.
    scratch-exec = {
      got = linesWith "ExecStart=" scratchUnit;
      want = [
        "ExecStart="
        (execLine [
          "--config"
          "/etc/diskgc/watch-scratch.yaml"
          "--min-free"
          "10G"
          "--target-free"
          "20G"
          "--select"
          "profile:cargo-target && filter:idle-7d"
          "--roots"
          "/scratch/src"
          "/scratch"
        ])
      ];
    };
    escape-exec = {
      got = linesWith "ExecStart=" escapeUnit;
      want = [
        "ExecStart="
        ''ExecStart="${exe}" "watch" "--tally-only" "--select" "%%h $$HOME \"q\" back\\slash" "/srv/escape"''
      ];
    };
    # The account name is literal: a newline (a directive of its own),
    # empty (User= is root), a special value, a specifier or a UID is
    # refused by the option's type; ordinary names pass.
    user-type-refuses = {
      got = lib.filter userType.check [
        "runner\nProtectProc=invisible"
        ""
        "@system"
        "%u"
        "1000"
        "mark "
        "-mark"
        "a:b"
      ];
      want = [ ];
    };
    user-type-accepts = {
      got = lib.all userType.check [
        "mark"
        "gitlab-runner"
        "root"
        "_svc"
        "first.last"
        "machine$"
      ];
      want = true;
    };
    user-newline-fails-evaluation = {
      got = map systemUnitEvaluates [
        "runner\nProtectProc=invisible"
        "mark"
      ];
      want = [
        false
        true
      ];
    };
    laptop-etc = {
      got = map (etcEntry laptop) [
        "diskgc/config.yaml"
        "diskgc/watch-scratch.yaml"
      ];
      want = [
        {
          mode = "0644";
          source = "diskgc-config-checked.yaml";
        }
        {
          mode = "0644";
          source = "diskgc-watch-scratch-checked.yaml";
        }
      ];
    };
    laptop-tmpfiles = {
      got = lib.elem package laptop.systemd.tmpfiles.packages;
      want = true;
    };
    laptop-units-package = {
      got = lib.elem package laptop.systemd.packages;
      want = true;
    };
    laptop-no-warnings = {
      got = laptop.warnings;
      want = [ ];
    };
    # System scope: --config names the module's file, User=, an escaped
    # instance name, restart on a configuration change.
    runner-exec = {
      got = linesWith "ExecStart=" runnerUnit;
      want = [
        "ExecStart="
        (execLine [
          "--config"
          "/etc/diskgc/config.yaml"
          "--min-free"
          "50G"
          "--tally-only"
          "/srv/ci-builds"
        ])
      ];
    };
    runner-user = {
      got =
        linesWith "User=" runnerUnit
        ++ linesWith "ConditionUser=" runnerUnit
        ++ linesWith "EnvironmentFile" runnerUnit
        ++ linesWith "CapabilityBoundingSet" runnerUnit;
      want = [
        "User=gitlab-runner"
        "EnvironmentFile="
      ];
    };
    runner-wanted-by = {
      got = runnerUnit.wantedBy;
      want = [ "multi-user.target" ];
    };
    runner-restart-triggers = {
      got = map (d: d.name) ci.systemd.services."diskgc-watch@srv-ci\\x2dbuilds".restartTriggers;
      want = [ "diskgc-config-checked.yaml" ];
    };
    root-exec = {
      got = linesWith "ExecStart=" rootUnit;
      want = [
        "ExecStart="
        (execLine [
          "--config"
          "/etc/diskgc/watch-root.yaml"
          "--tally-only"
          "--allow-root"
          "/"
        ])
      ];
    };
    root-capabilities = {
      got = linesWith "CapabilityBoundingSet=" rootUnit ++ linesWith "User=" rootUnit;
      want = [
        "CapabilityBoundingSet=CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_PTRACE CAP_CHECKPOINT_RESTORE"
        "User=root"
      ];
    };
    # Only the file an instance reads restarts it.
    root-restart-triggers = {
      got = map (d: d.name) ci.systemd.services."diskgc-watch@-".restartTriggers;
      want = [ "diskgc-watch-root-checked.yaml" ];
    };
    optin-restart-triggers = {
      got = map (d: d.name) ci.systemd.services."diskgc-watch@srv-other".restartTriggers;
      want = [ "diskgc-config-checked.yaml" ];
    };
    optin-exec = {
      got = linesWith "ExecStart=" (systemUnit ci "diskgc-watch@srv-other");
      want = [
        "ExecStart="
        (execLine [
          "--config"
          "/etc/diskgc/config.yaml"
          "--tally-only"
          "/srv/other"
        ])
      ];
    };
    ci-tmpfiles = {
      got = lib.elem package ci.systemd.tmpfiles.packages;
      want = true;
    };
    # The linger warning: linger unset (null) warns.
    linger-warning = {
      got = noLinger.warnings;
      want = [
        "services.diskgc.instances.home: user \"mark\" does not linger (users.users.mark.linger is not true), so the watcher of /home runs only while that user has a session, unless lingering was enabled with loginctl."
      ];
    };
    # Each assertion on its own example.
    assert-unknown-user = {
      got = failedAssertions {
        services.diskgc.instances.home = {
          path = "/home";
          user = "mrak";
        };
      };
      want = [
        "services.diskgc.instances.home: user \"mrak\" is not in users.users; a user-scope instance runs in that user's manager."
      ];
    };
    assert-allow-root-user-scope = {
      got = failedAssertions {
        users.users.root = { };
        services.diskgc.instances.r = {
          path = "/";
          user = "root";
          allowRoot = true;
        };
      };
      want = [
        "services.diskgc.instances.r: allowRoot needs scope = \"system\" and user = \"root\" (it has scope = \"user\", user = \"root\")."
      ];
    };
    assert-allow-root-not-root = {
      got = failedAssertions {
        services.diskgc.instances.r = {
          scope = "system";
          path = "/";
          user = "mark";
          allowRoot = true;
          configFile = "/etc/diskgc/r.yaml";
        };
      };
      want = [
        "services.diskgc.instances.r: allowRoot needs scope = \"system\" and user = \"root\" (it has scope = \"system\", user = \"mark\")."
      ];
    };
    assert-settings-and-config-file = {
      got = failedAssertions {
        services.diskgc.instances.home = {
          path = "/home";
          user = "mark";
          settings = policy [ "/home/mark/src" ];
          configFile = "/etc/diskgc/mine.yaml";
        };
      };
      want = [ "services.diskgc.instances.home: set at most one of settings and configFile." ];
    };
    assert-duplicate-unit = {
      got = failedAssertions {
        users.users.anna.isNormalUser = true;
        services.diskgc.instances = {
          a = {
            path = "/home";
            user = "mark";
          };
          b = {
            path = "/home/";
            user = "anna";
          };
        };
      };
      want = [
        "services.diskgc.instances: a, b all map to the user unit diskgc-watch@home.service; one instance per watched path and scope (user units are shared by every user)."
      ];
    };
    assert-unknown-system-user = {
      got = failedAssertions {
        services.diskgc.instances.ci = {
          scope = "system";
          path = "/srv";
          user = "gitlab-runer";
          configFile = "/etc/diskgc/ci.yaml";
        };
      };
      want = [
        "services.diskgc.instances.ci: user \"gitlab-runer\" is not in users.users; it is the unit's User=."
      ];
    };
    # No settings anywhere: a stale hand-placed /etc/diskgc/config.yaml
    # would be read unchecked.
    assert-system-needs-config = {
      got = failedAssertions {
        services.diskgc.instances.ci = {
          scope = "system";
          path = "/srv";
          user = "mark";
        };
      };
      want = [
        "services.diskgc.instances.ci: a system instance needs a checked configuration (its settings, or services.diskgc.settings) or an explicit configFile; to use a hand-placed /etc/diskgc/config.yaml, set configFile = \"/etc/diskgc/config.yaml\"."
      ];
    };
  };

  failures = lib.filterAttrs (_: c: c.got != c.want) cases;

  # What the shell half reads: each drop-in under its unit name.
  dropIns = {
    user = [
      homeUnit
      scratchUnit
      escapeUnit
    ];
    system = [
      runnerUnit
      rootUnit
    ];
  };
  stageDropIns =
    manager:
    lib.concatMapStrings (u: ''
      mkdir -p "$TMPDIR/${manager}/${u.name}.d"
      cp ${u.unit}/${lib.escapeShellArg u.name} "$TMPDIR/${manager}/${u.name}.d/overrides.conf"
      ln -s diskgc-watch@.service "$TMPDIR/${manager}/${u.name}"
      files+=("$TMPDIR/${manager}/${u.name}")
    '') dropIns.${manager};
in
{
  # AC2: the generated drop-ins, /etc entries, tmpfiles packages,
  # assertions and warnings, then each drop-in on its packaged template
  # through systemd-analyze verify (any output fails, as in
  # deployment-units). Building it also builds the checked configs, the
  # positive control of nixos-module-bad-config.
  nixos-module-eval =
    pkgs.runCommand "diskgc-nixos-module-eval"
      {
        nativeBuildInputs = [
          pkgs.systemd
          pkgs.util-linux
        ];
        failures = builtins.toJSON failures;
        passAsFile = [ "failures" ];
        checked = map (e: e.source) [
          laptop.environment.etc."diskgc/config.yaml"
          laptop.environment.etc."diskgc/watch-scratch.yaml"
          ci.environment.etc."diskgc/watch-root.yaml"
        ];
      }
      ''
        if [ "$(cat "$failuresPath")" != '{}' ]; then
          echo "error: module evaluation cases failed (got vs want):" >&2
          cat "$failuresPath" >&2
          echo >&2
          exit 1
        fi
        echo "${toString (lib.length (lib.attrNames cases))} evaluation cases passed"
        grep -qxF 'f /run/diskgc/lock 0644 root root -' ${package}/lib/tmpfiles.d/diskgc.conf
        export HOME=$TMPDIR/home XDG_RUNTIME_DIR=$TMPDIR/run
        mkdir -p $HOME $XDG_RUNTIME_DIR $TMPDIR/root
        ${systemdVerify}
        for manager in user system; do
          mkdir -p $TMPDIR/$manager
          cp ${package}/lib/systemd/$manager/diskgc-watch@.service $TMPDIR/$manager/
          files=()
          if [ $manager = user ]; then
            ${stageDropIns "user"}
          else
            ${stageDropIns "system"}
          fi
          if ! verify $manager $TMPDIR/$manager "''${files[@]}" || [ -s $TMPDIR/$manager.log ]; then
            cat $TMPDIR/$manager.log >&2
            echo "error: systemd-analyze verify rejected or warned about the module's $manager drop-ins" >&2
            exit 1
          fi
          echo "$manager drop-ins on the packaged template: verified, no output"
        done
        mkdir -p $out
        cp ${homeUnit.unit}/* ${scratchUnit.unit}/* ${escapeUnit.unit}/* ${runnerUnit.unit}/* ${rootUnit.unit}/* $out/
        for c in $checked; do cp $c $out/; done
      '';

  # AC1: an invalid configuration fails the build of what /etc/diskgc is
  # made from, with diskgc check's own message.
  nixos-module-bad-config =
    let
      bad = evalSystem {
        services.diskgc = {
          enable = true;
          settings = {
            version = 1;
            groups = [
              {
                name = "stale";
                tiers = [
                  {
                    name = "typo";
                    select = "profile:cargo-taget";
                  }
                ];
              }
            ];
          };
        };
      };
      failed = pkgs.testers.testBuildFailure bad.environment.etc."diskgc/config.yaml".source;
    in
    pkgs.runCommand "diskgc-nixos-module-bad-config" { } ''
      log=${failed}/testBuildFailure.log
      cat $log
      test "$(cat ${failed}/testBuildFailure.exit)" = 1
      grep -F "no profile or filter is named \`cargo-taget\`" $log
      grep -F "error: diskgc check rejected services.diskgc configuration 'config'; nothing was deployed" $log
      touch $out
    '';
}
