# The NixOS module, `nixosModules.default` (docs/architecture.md, NixOS
# module: the option schema, the instance mapping and the build-time check
# are specified there, and that section is their one home).
#
# The module ships no deletion logic and no unit text of its own: each
# instance is the packaged `diskgc-watch@.service` template of its scope
# plus a drop-in that replaces ExecStart= with the full command line,
# resets EnvironmentFile= and adds User= or ConditionUser=. Restart and
# hardening lines stay the template's, so generic Linux and NixOS run one
# unit text.
# `defaultPackage` (a function of pkgs) and its text: the flake's choice by
# version (flake.nix, moduleDefaultAttr): a release's package at a release
# version, the gated development package on master.
{ defaultPackage, defaultPackageText }:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.diskgc;
  yaml = pkgs.formats.yaml { };

  # An absolute path as a string: a Nix path literal is refused by the
  # type, so it can never be copied into the store by interpolation.
  absolutePathString = types.strMatching "/.*";

  # A literal account name, as it goes into User= or ConditionUser=: no
  # whitespace or control character (a newline would start a directive of
  # its own), no systemd specifier (%), no special value (@system) or
  # UID (a leading digit), not empty.
  accountName = types.strMatching "[A-Za-z_][A-Za-z0-9_.-]*[$]?";

  # Root's capability set for --allow-root (docs/architecture.md, The
  # `watch` units, Root). CAP_SYS_PTRACE is mandatory; diskgc refuses to
  # start without it.
  allowRootCapabilities = "CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_PTRACE CAP_CHECKPOINT_RESTORE";

  # The rendered YAML, copied to $out only when `diskgc check` accepts it,
  # so an invalid configuration fails the system build with check's own
  # message. check needs nothing from the machine: roots absent from the
  # build sandbox only warn, owners: names resolve at run time.
  checkedConfig =
    name: settings:
    let
      rendered = yaml.generate "diskgc-${name}.yaml" settings;
    in
    pkgs.runCommand "diskgc-${name}-checked.yaml" { } ''
      echo "diskgc check --config /etc/diskgc/${name}.yaml (rendered as ${rendered})" >&2
      if ! ${lib.getExe cfg.package} check --config ${rendered}; then
        echo "error: diskgc check rejected services.diskgc configuration '${name}'; nothing was deployed" >&2
        exit 1
      fi
      cp ${rendered} $out
    '';

  globalConfigPath = "/etc/diskgc/config.yaml";
  instanceConfigPath = name: "/etc/diskgc/watch-${name}.yaml";

  instanceOptions =
    { name, ... }:
    {
      options = {
        scope = mkOption {
          type = types.enum [
            "user"
            "system"
          ];
          default = "user";
          description = "Which systemd manager runs the instance: the `user`'s own manager, or the system manager.";
        };
        user = mkOption {
          type = accountName;
          description = "The account in `users.users`: user scope `ConditionUser=`, system scope `User=`.";
        };
        path = mkOption {
          type = absolutePathString;
          description = "The watched `PATH`; it names the unit (`diskgc-watch@<escaped path>.service`).";
        };
        minFree = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "30G";
          description = "`--min-free`, when set.";
        };
        targetFree = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "60G";
          description = "`--target-free`, when set.";
        };
        tallyOnly = mkOption {
          type = types.bool;
          default = false;
          description = "`--tally-only`: keep the index warm, never delete.";
        };
        extraArgs = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "--select"
            "profile:cargo-target && filter:idle-7d"
          ];
          description = "Every other `watch` flag, one argument per element, passed as given; no mode flag (--tally-only, --min-free, --target-free, --dry-run: use the typed options) (docs/architecture.md, NixOS module).";
        };
        allowRoot = mkOption {
          type = types.bool;
          default = false;
          description = "`--allow-root` and root's capability set; system scope and `user = \"root\"` only.";
        };
        settings = mkOption {
          type = types.nullOr yaml.type;
          default = null;
          description = "This instance's configuration, rendered to `/etc/diskgc/watch-${name}.yaml` and checked at build time.";
        };
        configFile = mkOption {
          type = types.nullOr absolutePathString;
          default = null;
          description = "A configuration file the module neither manages nor checks (a string: never copied into the store).";
        };
      };
    };

  instances = lib.mapAttrsToList (name: inst: inst // { inherit name; }) cfg.instances;

  # What the package says it runs and ships: its row of the release
  # descriptor (crates/diskgc-core/src/releases.json), which the diskgc
  # flake's packages carry as passthru.diskgcBuild (flake.nix,
  # buildPassthru; release-candidate-package and deployment-units check
  # the files are there). null for a package that does not say. A claim,
  # not a proof: an overrideAttrs that swaps the source keeps the old
  # passthru (the binary still refuses what its own row does not run).
  packageBuild = cfg.package.diskgcBuild or null;
  packageName = cfg.package.name or "the package";
  # The templates every instance's drop-in extends (systemd.packages), as
  # the descriptor names them.
  watchTemplates = (lib.importJSON ../../crates/diskgc-core/src/releases.json).watch_units;
  # Flags that choose what `watch` does: only the typed options set them
  # (tallyOnly, minFree, targetFree), so the module knows the mode it
  # asserts on. In extraArgs one could also hide as another flag's value
  # (`--config --tally-only`), which diskgc reads differently (TASK-289
  # review). `--dry-run` is not a `watch` flag; refused here too.
  modeFlags = [
    "--tally-only"
    "--min-free"
    "--target-free"
    "--dry-run"
  ];
  isModeFlag = arg: lib.any (f: arg == f || lib.hasPrefix "${f}=" arg) modeFlags;
  unitName = inst: "diskgc-watch@${utils.escapeSystemdPath inst.path}";

  # --config: configFile, else the instance's settings file; otherwise a
  # system instance names the module's file (asserted to exist: global
  # settings) and a user instance passes none (null), so its lookup finds
  # the same file as the user's agents.
  configPath =
    inst:
    if inst.configFile != null then
      inst.configFile
    else if inst.settings != null then
      instanceConfigPath inst.name
    else if inst.scope == "system" then
      globalConfigPath
    else
      null;

  execStart =
    inst:
    utils.escapeSystemdExecArgs (
      [
        (lib.getExe cfg.package)
        "watch"
      ]
      ++ lib.optionals (configPath inst != null) [
        "--config"
        (configPath inst)
      ]
      ++ lib.optionals (inst.minFree != null) [
        "--min-free"
        inst.minFree
      ]
      ++ lib.optionals (inst.targetFree != null) [
        "--target-free"
        inst.targetFree
      ]
      ++ lib.optional inst.tallyOnly "--tally-only"
      ++ lib.optional inst.allowRoot "--allow-root"
      ++ inst.extraArgs
      ++ [ inst.path ]
    );

  checkedGlobal = lib.optional (cfg.settings != null) (checkedConfig "config" cfg.settings);
  checkedInstance =
    inst: lib.optional (inst.settings != null) (checkedConfig "watch-${inst.name}" inst.settings);

  # The drop-in common to both scopes.
  dropIn = inst: {
    overrideStrategy = "asDropin";
    serviceConfig = {
      # Empty first: reset the template's line, then the full command.
      ExecStart = [
        ""
        (execStart inst)
      ];
      # Reset only: a stale generic-Linux .env file cannot inject flags.
      EnvironmentFile = "";
    };
  };

  systemDropIn =
    inst:
    lib.recursiveUpdate (dropIn inst) {
      wantedBy = [ "multi-user.target" ];
      # Only the file it reads: the global one (also as configFile) or its own.
      restartTriggers =
        lib.optionals (configPath inst == globalConfigPath) checkedGlobal ++ checkedInstance inst;
      # The system profile's tools, as a user instance has them from its
      # manager's PATH (userDropIn): `docker`, `nix` and what command
      # actions call.
      path = [ "/run/current-system/sw" ];
      serviceConfig = {
        User = inst.user;
      }
      // lib.optionalAttrs inst.allowRoot {
        CapabilityBoundingSet = allowRootCapabilities;
      };
    };

  # NixOS gives every service a PATH of coreutils and a few tools. A user
  # instance drops it, so it runs with the user manager's PATH (the
  # session's, from /etc/environment.d) and finds `docker`, `nix` and
  # what command actions call as the user's agents do, like the user
  # template on generic Linux.
  userDropIn =
    inst:
    lib.recursiveUpdate (dropIn inst) {
      wantedBy = [ "default.target" ];
      unitConfig.ConditionUser = inst.user;
      environment.PATH = lib.mkForce null;
    };

  instancesOf = scope: lib.filter (inst: inst.scope == scope) instances;
  unitsOf =
    scope: mkDropIn:
    lib.listToAttrs (map (inst: lib.nameValuePair (unitName inst) (mkDropIn inst)) (instancesOf scope));

  # One assertion per unit name and scope: two instances of one unit name
  # would silently merge into one drop-in.
  duplicateUnitAssertions =
    scope:
    let
      byUnit = lib.groupBy unitName (instancesOf scope);
    in
    lib.mapAttrsToList (unit: insts: {
      assertion = lib.length insts == 1;
      message = "services.diskgc.instances: ${
        lib.concatMapStringsSep ", " (i: i.name) insts
      } all map to the ${scope} unit ${unit}.service; one instance per watched path and scope (user units are shared by every user).";
    }) byUnit;

  # The package must ship both templates (TASK-289: the release candidate
  # shipped none, so the drop-ins sat on a missing unit). Only with
  # instances: without one, nothing extends them.
  packageAssertions = lib.optional (instances != [ ]) {
    assertion = packageBuild != null && lib.all (t: lib.elem t packageBuild.ships) watchTemplates;
    message =
      if packageBuild == null then
        "services.diskgc.package (${packageName}) does not say which files it ships (passthru.diskgcBuild, as the diskgc flake's packages do); the instances need its diskgc-watch@.service templates."
      else
        "services.diskgc.package (${packageName}) does not ship the diskgc-watch@.service templates (${lib.concatStringsSep ", " watchTemplates}) that the instances' drop-ins extend.";
  };

  # An instance runs only what the package's build runs: a reclaiming
  # instance of a package whose release refuses a reclaiming `watch`
  # (exit 2) would stay failed.
  commandAssertions =
    inst:
    let
      needs = if inst.tallyOnly then "watch --tally-only" else "watch";
      runs = packageBuild.commands;
    in
    lib.optional (packageBuild != null) {
      assertion = lib.elem needs runs || (needs == "watch --tally-only" && lib.elem "watch" runs);
      message = "services.diskgc.instances.${inst.name}: ${packageName} does not run `${needs}` (its commands: ${lib.concatStringsSep ", " runs})${lib.optionalString (needs == "watch") "; set tallyOnly = true, or use a package whose release runs a reclaiming watch"}.";
    };

  instanceAssertions =
    inst:
    commandAssertions inst
    ++ [
      {
        assertion = !lib.any isModeFlag inst.extraArgs;
        message = "services.diskgc.instances.${inst.name}: extraArgs holds ${
          lib.concatStringsSep ", " (lib.filter isModeFlag inst.extraArgs)
        }; set the mode with tallyOnly, minFree and targetFree instead.";
      }
    {
      assertion = !inst.allowRoot || (inst.scope == "system" && inst.user == "root");
      message = "services.diskgc.instances.${inst.name}: allowRoot needs scope = \"system\" and user = \"root\" (it has scope = \"${inst.scope}\", user = \"${inst.user}\").";
    }
    {
      assertion =
        inst.scope != "system" || inst.configFile != null || inst.settings != null || cfg.settings != null;
      message = "services.diskgc.instances.${inst.name}: a system instance needs a checked configuration (its settings, or services.diskgc.settings) or an explicit configFile; to use a hand-placed /etc/diskgc/config.yaml, set configFile = \"/etc/diskgc/config.yaml\".";
    }
    {
      assertion = inst.settings == null || inst.configFile == null;
      message = "services.diskgc.instances.${inst.name}: set at most one of settings and configFile.";
    }
    {
      assertion = config.users.users ? ${inst.user};
      message = "services.diskgc.instances.${inst.name}: user \"${inst.user}\" is not in users.users; ${
        if inst.scope == "user" then "a user-scope instance runs in that user's manager" else "it is the unit's User="
      }.";
    }
  ];

  # users.users.<u>.linger is null (unmanaged), false or true.
  lingerWarnings = map (
    inst:
    "services.diskgc.instances.${inst.name}: user \"${inst.user}\" does not linger (users.users.${inst.user}.linger is not true), so the watcher of ${inst.path} runs only while that user has a session, unless lingering was enabled with loginctl."
  ) (lib.filter (inst: (config.users.users.${inst.user}.linger or null) != true) (instancesOf "user"));
in
{
  options.services.diskgc = {
    enable = mkOption {
      type = types.bool;
      default = false;
      description = "diskgc on PATH, the host lock, both `watch` templates and the `instances` (docs/architecture.md, NixOS module).";
    };
    package = mkOption {
      type = types.package;
      default = defaultPackage pkgs;
      defaultText = lib.literalExpression defaultPackageText;
      description = "The diskgc package. From a release's tag (v0.2.0 onwards) the default is that release's package, which runs a reclaiming `watch`; from master (a -dev version) it is the development package, which refuses to delete outside test containers, so a reclaiming instance there exits 2. Its commands and `watch` templates are asserted from its passthru.diskgcBuild.";
    };
    settings = mkOption {
      type = types.nullOr yaml.type;
      default = null;
      description = "The configuration, rendered to `/etc/diskgc/config.yaml` (copied, mode 0644) and checked at build time.";
    };
    instances = mkOption {
      type = types.attrsOf (types.submodule instanceOptions);
      default = { };
      description = "One `watch` service each.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    systemd.tmpfiles.packages = [ cfg.package ];
    systemd.packages = [ cfg.package ];

    # Copied into /etc (mode set), not linked into the store, so that
    # --allow-root's ownership rule holds for them.
    environment.etc =
      lib.optionalAttrs (cfg.settings != null) {
        "diskgc/config.yaml" = {
          source = lib.head checkedGlobal;
          mode = "0644";
        };
      }
      // lib.listToAttrs (
        map (
          inst:
          lib.nameValuePair "diskgc/watch-${inst.name}.yaml" {
            source = lib.head (checkedInstance inst);
            mode = "0644";
          }
        ) (lib.filter (inst: inst.settings != null) instances)
      );

    systemd.services = unitsOf "system" systemDropIn;
    systemd.user.services = unitsOf "user" userDropIn;

    assertions =
      packageAssertions
      ++ duplicateUnitAssertions "system"
      ++ duplicateUnitAssertions "user"
      ++ lib.concatMap instanceAssertions instances;
    warnings = lingerWarnings;
  };
}
