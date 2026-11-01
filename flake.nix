{
  description = "diskgc: keeps enough disk space free by reclaiming rebuildable caches";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Test-only NixOS package set for the pinned Linux 5.15 VM.
    nixpkgs515.url = "github:NixOS/nixpkgs/6f3b13fcf904691a274955cc80c9f2ed655ff6a9";
    # Historical test-only package sets for the Linux 5.8 and 6.8 VM points.
    nixpkgs58 = {
      url = "github:NixOS/nixpkgs/cd63096d6d887d689543a0b97743d28995bc9bc3";
      flake = false;
    };
    nixpkgs68.url = "github:NixOS/nixpkgs/63dacb46bf939521bdc93981b4cbb7ecb58427a0";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Real West-generated build output in the disposable TASK-101 fixture.
    zephyr-src = {
      url = "github:zephyrproject-rtos/zephyr/v4.3.0";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs515,
      nixpkgs58,
      nixpkgs68,
      crane,
      rust-overlay,
      zephyr-src,
    }:
    let
      # diskgc is Linux-only. Only x86_64 is built and tested; another system
      # needs its own musl triple below and a run of the gates on it.
      system = "x86_64-linux";
      muslTarget = "x86_64-unknown-linux-musl";

      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
      pkgs515 = import nixpkgs515 { inherit system; };
      pkgs58 = import nixpkgs58 { inherit system; };
      pkgs68 = import nixpkgs68 { inherit system; };
      inherit (pkgs) lib;

      # The product toolchain, pinned by rust-toolchain.toml (single source).
      craneLib = (crane.mkLib pkgs).overrideToolchain (
        p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
      );

      # libFuzzer needs nightly and rust-src, but the product remains on the
      # stable toolchain in rust-toolchain.toml.
      fuzzToolchain = pkgs.rust-bin.nightly."2026-10-01".minimal.override {
        extensions = [ "rust-src" ];
      };

      devShellHook = ''
        diskgc_worktree_root="$(git rev-parse --show-toplevel 2>/dev/null)"
        if [ -z "$diskgc_worktree_root" ] || [ ! -f "$diskgc_worktree_root/justfile" ]; then
          echo "diskgc dev shell must start inside a diskgc worktree" >&2
          return 1
        fi
        mkdir -p "$diskgc_worktree_root/tmp/system"
        export CARGO_TARGET_DIR="$diskgc_worktree_root/tmp/target"
        export TMPDIR="$diskgc_worktree_root/tmp/system"
        export PYTHONPYCACHEPREFIX="$diskgc_worktree_root/tmp/pycache"
        unset diskgc_worktree_root
      '';

      # Single source of the version: [workspace.package] in Cargo.toml.
      # Master carries the next release's version with a pre-release suffix
      # (0.2.0-dev); only the commit a release tag points at has a plain one.
      version = (lib.importTOML ./Cargo.toml).workspace.package.version;
      # A development version (one with a pre-release suffix) names its
      # commit in `diskgc --version`; a release version never does
      # (crates/diskgc/src/main.rs, version_line; docs/reference.md,
      # `diskgc --version`; docs/architecture.md, Release procedure).
      isDevVersion = lib.hasInfix "-" version;
      # The commit the shipped packages are built from: a dirty tree has
      # only dirtyShortRev ("<rev>-dirty", the base commit), a source
      # without Git (a tarball, a path: input) neither.
      gitRev = self.shortRev or self.dirtyShortRev or "unknown";
      # The stamp, compiled in as DISKGC_GIT_REV. It changes with every
      # commit, so only packages.default, packages.release-candidate (and
      # through it the NixOS module's default) and the version-stamp check
      # carry it. Every other check and test consumer uses the unstamped
      # `tested` build of the same source and features, which stays cached
      # across commits that change no source. Empty for a release version,
      # so a release's shipped build is its tested build.
      stampArgs = lib.optionalAttrs isDevVersion { DISKGC_GIT_REV = gitRev; };
      # The `--version` lines of the two builds, for the checks below.
      testedVersionLine = if isDevVersion then "diskgc ${version} (unknown)" else "diskgc ${version}";
      shippedVersionLine = if isDevVersion then "diskgc ${version} (${gitRev})" else "diskgc ${version}";

      # The test-sandbox marker constant (docs/testing.md, Safety interlock):
      # compiled into the binaries by include_bytes! and copied byte for byte
      # into the e2e base image. As a path of its own it changes the image
      # only when the marker itself changes.
      sandboxMarker = ./crates/diskgc-linux/src/sandbox-marker;

      # Source allowlist: only Cargo manifests, the lock file, Rust/TOML
      # files under the crate directories and the crates' listed data files
      # reach the Rust build. Most docs, backlog files, flake.nix and the
      # justfile are excluded from that source set; docs/example.yaml is a
      # separate direct input to the package's installed example, and
      # workspaceTestSrc below adds two docs only for workspace tests.
      workspaceFiles = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        (craneLib.fileset.commonCargoSources ./crates)
        # Non-Rust inputs of the crates, listed one by one.
        sandboxMarker
        # YAML read by tests (host-model fixtures; later built-in profiles and
        # golden sets) and the committed host-model JSON Schema, which a
        # diskgc-model test compares with the generated one.
        (lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates)
        ./crates/diskgc-model/host-model.schema.json
      ];
      workspaceSrc = lib.fileset.toSource {
        root = ./.;
        fileset = workspaceFiles;
      };
      # The workspace tests also read docs/reference.md, docs/safety.md,
      # docs/example.yaml and PRD.md:
      # the selection grammar's examples and messages, the JSON report's
      # fields and enums and the exit-code table are tested from
      # reference.md, the host lock's tmpfiles lines from safety.md, the
      # configuration examples (reference.md's and example.yaml) against
      # the catalog they claim, and the composition examples from PRD.md
      # (diskgc-core's tests), so the docs and the code cannot drift. Only
      # this check sees them; the package and clippy do not.
      # The committed proptest seeds (*.proptest-regressions beside a test)
      # rerun first here as under `just check` (TASK-279).
      workspaceTestSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          workspaceFiles
          (lib.fileset.fileFilter (f: f.hasExt "proptest-regressions") ./crates)
          ./docs/reference.md
          ./docs/safety.md
          ./docs/example.yaml
          ./PRD.md
        ];
      };
      # diskgc-e2e path-depends on workspace crates, which inherit fields from
      # the root manifest, so its source is the workspace plus its directory,
      # and the shipped pre-job hooks, which the CI operator and agent
      # journeys run as written.
      e2eSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          workspaceFiles
          (craneLib.fileset.commonCargoSources ./diskgc-e2e)
          # Its Clippy rules (e2e-clippy): fixture writes only through Scratch.
          ./diskgc-e2e/clippy.toml
          # Producer lifecycle scenarios, embedded by lifecycle_suite.rs.
          ./diskgc-e2e/src/lifecycle
          ./docs/ci-pre-job.sh
          ./docs/agent-pre-job.py
        ];
      };

      # Every flake derivation builds for static musl, so package, clippy and
      # tests share one dependency build and the tests exercise the shipped
      # target. The target is set here and nowhere else: no
      # .cargo/config.toml, so cargo in the dev shell builds for the host
      # (gnu) target and cargo-fuzz keeps working there.
      targetArgs = {
        strictDeps = true;
        CARGO_BUILD_TARGET = muslTarget;
        # Rust's musl targets link statically by default; stated explicitly
        # because "runs on any distribution" is part of the product contract.
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C target-feature=+crt-static";
      };

      workspaceArgs = targetArgs // {
        src = workspaceSrc;
        pname = "diskgc-workspace";
        inherit version;
      };
      workspaceDeps = craneLib.buildDepsOnly workspaceArgs;
      installShippedFiles = ''
        install -Dm0644 ${./packaging/diskgc.conf} $out/lib/tmpfiles.d/diskgc.conf
        install -Dm0644 ${./docs/example.yaml} $out/share/doc/diskgc/examples/example.yaml
      '';
      # The `watch` unit templates (docs/architecture.md, Deployment), for
      # the user and the system manager. Only the development package ships
      # them: 0.1 is the `ensure` MVP (PRD, Scope), and the release
      # candidate's contents are TASK-67's. The directory, not each file, is
      # the Nix input: `@` is not allowed in a store path name. stdenv's
      # fixup moves lib/systemd/user to share/systemd/user and links the
      # old path to it, so both paths reach the file.
      watchUnits = ./packaging/systemd;
      installWatchUnits = ''
        install -Dm0644 ${watchUnits}/user/diskgc-watch@.service $out/lib/systemd/user/diskgc-watch@.service
        install -Dm0644 ${watchUnits}/system/diskgc-watch@.service $out/lib/systemd/system/diskgc-watch@.service
      '';
      # Capture complete readelf output before looking for dynamic-linker
      # records. A readelf | grep -q pipeline can make grep stop early and
      # report a misleading result when readelf receives SIGPIPE.
      checkStaticElf = ''
        check_static_elf() {
          local bin="$1"
          if ! program_headers="$(readelf --wide --program-headers "$bin")"; then
            echo "error: cannot inspect ELF program headers: $bin" >&2
            return 1
          fi
          if ! dynamic_section="$(readelf --wide --dynamic "$bin")"; then
            echo "error: cannot inspect ELF dynamic section: $bin" >&2
            return 1
          fi
          case "$program_headers" in
            *INTERP*)
              echo "error: $bin requests a program interpreter; it is not statically linked" >&2
              return 1
              ;;
          esac
          case "$dynamic_section" in
            *NEEDED*)
              echo "error: $bin needs shared libraries" >&2
              return 1
              ;;
          esac
        }
      '';

      # Shared by deployment-units and the NixOS module's nixos-module-eval
      # (docs/testing.md, Deployment units, 2 and 4); the caller sets HOME,
      # XDG_RUNTIME_DIR and makes $TMPDIR/root.
      systemdVerify = ''
        # verify MANAGER DIR FILE...: exit status of systemd-analyze verify
        # of the FILEs; its output in DIR.log. The unit path is DIR and
        # systemd's own units for that manager (the targets default
        # dependencies name), nothing else.
        # --user runs as is. System mode creates /run/systemd, and the
        # sandbox has no /run and a read-only / (`--root=DIR` does not
        # help: measured, it still creates /run/systemd), so it runs
        # chrooted into a tmpfs root (binding /nix, /proc, /dev and
        # $NIX_BUILD_TOP) in a user and mount namespace of its own.
        # Without unprivileged user namespaces only that half fails.
        verify() {
          local units="$2:${pkgs.systemd}/example/systemd/$1"
          local manager="$1" dir="$2"
          shift 2
          local files=("$@")
          if [ "$manager" = user ]; then
            SYSTEMD_UNIT_PATH="$units" systemd-analyze --user verify --man=no "''${files[@]}" \
              > "$dir.log" 2>&1
            return
          fi
          unshare --map-root-user --mount sh -euc '
            root=$1 top=$2; shift 2
            mount -t tmpfs none "$root"
            for d in nix proc dev run tmp; do mkdir "$root/$d"; done
            mkdir -p "$root$top"
            for d in /nix /proc /dev "$top"; do mount --rbind "$d" "$root$d"; done
            exec chroot "$root" "$@"
          ' sh $TMPDIR/root "$NIX_BUILD_TOP" env SYSTEMD_UNIT_PATH="$units" \
            ${pkgs.systemd}/bin/systemd-analyze verify --man=no "''${files[@]}" \
            > "$dir.log" 2>&1
        }
      '';

      # One diskgc build definition, as two packages sharing one compile:
      # `tested` (no stamp) is what every check and test consumer uses;
      # `shipped` adds stampArgs, so only the binary crate is compiled again
      # (DISKGC_GIT_REV is read by option_env! there alone). For a release
      # version stampArgs is empty and the two are one derivation.
      mkDiskgc =
        {
          pname,
          cargoExtraArgs,
          postInstall,
        }:
        let
          args = workspaceArgs // {
            inherit pname cargoExtraArgs;
            # Tests run once, in checks.workspace-tests.
            doCheck = false;
          };
          compiled = craneLib.cargoBuild (args // { cargoArtifacts = workspaceDeps; });
          build =
            stamp:
            craneLib.buildPackage (
              args
              // lib.optionalAttrs stamp stampArgs
              // {
                cargoArtifacts = compiled;
                inherit postInstall;
                meta.mainProgram = "diskgc";
              }
            );
        in
        {
          tested = build false;
          shipped = build true;
        };

      # The development package: gated, the development catalog.
      diskgcBuilds = mkDiskgc {
        pname = "diskgc";
        cargoExtraArgs = "--locked --package diskgc";
        postInstall = installShippedFiles + installWatchUnits;
      };
      diskgcTested = diskgcBuilds.tested;
      diskgc = diskgcBuilds.shipped;

      # The release candidate built from this source: the 0.1 catalog with
      # the gate lifted. Keep the default development package gated; this is
      # the sole build that removes the gate, and it also narrows the
      # embedded catalog. A release only when built from its tag
      # (docs/architecture.md, Release procedure); from any other commit a
      # development version for disposable test containers
      # (docs/reference.md, Built-in profile library).
      diskgcReleaseBuilds = mkDiskgc {
        pname = "diskgc-release-candidate";
        cargoExtraArgs = "--locked --package diskgc --features proposed-release-01-catalog,diskgc-linux/release-without-sandbox-gate";
        postInstall = installShippedFiles;
      };
      # The release package: a build above plus its documentation
      # (TASK-214: a user with only the package had --help and nothing
      # else). Assembled apart from the compile so that a docs edit copies
      # files instead of rebuilding the static binary. The release notes
      # are cut from docs/reference.md (their one home) at build time, and
      # reference.md and safety.md ship beside them so that their links
      # resolve; links into architecture.md and testing.md do not.
      mkReleasePackage =
        build:
        pkgs.runCommand "diskgc-release-candidate-${version}"
          {
            inherit version;
            meta.mainProgram = "diskgc";
          }
          ''
            cp -R ${build}/. $out
            chmod -R u+w $out
            doc=$out/share/doc/diskgc
            install -Dm0644 ${./packaging/README.md} $doc/README.md
            install -Dm0644 ${./docs/reference.md} $doc/reference.md
            install -Dm0644 ${./docs/safety.md} $doc/safety.md
            # From the section's heading to the next heading of its level;
            # its in-page links (#x) point into reference.md.
            awk -v title=${lib.escapeShellArg releaseNotesTitle} \
              '/^### Release notes \(next release, draft\)$/ { on = 1; print title; next }
                 on && /^#{1,3} / { exit }
                 on' ${./docs/reference.md} \
              | sed 's/](#/](reference.md#/g' > $doc/RELEASE-NOTES.md
            # Fail closed on a renamed heading or a cut that lost the body.
            grep -qF '**Known limits.**' $doc/RELEASE-NOTES.md
            grep -qF '**JSON contract.**' $doc/RELEASE-NOTES.md
            if grep -q '^### User profiles' $doc/RELEASE-NOTES.md; then
              echo "release notes ran into the next section" >&2
              exit 1
            fi
          '';
      # The packaged notes' title names the build's version.
      releaseNotesTitle = "# diskgc ${version} release notes";
      diskgcReleaseTested = mkReleasePackage diskgcReleaseBuilds.tested;
      diskgcRelease = mkReleasePackage diskgcReleaseBuilds.shipped;

      # A separate feature-built binary for catalog-policy container tests.
      # This is not the default package and retains the pre-release gate.
      diskgcProposed01 = craneLib.buildPackage (
        workspaceArgs
        // {
          pname = "diskgc-proposed-01";
          cargoArtifacts = workspaceDeps;
          cargoExtraArgs = "--locked --package diskgc --features proposed-release-01-catalog";
          doCheck = false;
        }
      );

      # diskgc-e2e is a separate Cargo project (own Cargo.lock) inside the
      # source tree; crane builds it from its directory (crane FAQ:
      # workspace-not-at-source-root).
      e2eArgs = targetArgs // {
        src = e2eSrc;
        cargoLock = ./diskgc-e2e/Cargo.lock;
        cargoToml = ./diskgc-e2e/Cargo.toml;
        postUnpack = ''
          cd $sourceRoot/diskgc-e2e
          sourceRoot="."
        '';
      };
      e2eDeps = craneLib.buildDepsOnly e2eArgs;

      # Zephyr 4.3.0's compiled West fixture uses the same interpreter
      # for the West entry point and Zephyr's CMake Python scripts.
      westPython = pkgs.python3.withPackages (ps: with ps; [
        west pyelftools pyyaml pykwalify jsonschema packaging
      ]);

      # ---- Container e2e harness (docs/testing.md, Container setup) ----

      # The diskgc-e2e binaries: the in-container harness and tests, the
      # do-nothing interlock canary, the test driver (code under test that can
      # be paused or killed at a chosen port call), the host-side runner and
      # the release check. Static musl, so they run in any image and on the
      # host.
      e2eBins = craneLib.buildPackage (
        e2eArgs
        // {
          cargoArtifacts = e2eDeps;
          cargoExtraArgs = "--locked";
          # Unit tests run once, in checks.e2e-tests.
          doCheck = false;
        }
      );

      # The binaries under test, as real files (not symlinks into the store):
      # the runner bind-mounts $out/bin read-only at /opt/diskgc-e2e/bin. A
      # code change rebuilds this directory, never the base image.
      # The diskgc-linux unit-test executable (static, like every target
      # here), for the kernel VMs: its *_kernel_fact tests pin kernel
      # behaviour that docs/safety.md states per kernel version, so they run
      # on each pinned guest kernel, not only on the build host's.
      diskgcLinuxTests = craneLib.mkCargoDerivation (
        workspaceArgs
        // {
          src = workspaceTestSrc;
          pname = "diskgc-linux-tests";
          cargoArtifacts = workspaceDeps;
          doCheck = false;
          nativeBuildInputs = [ pkgs.jq ];
          buildPhaseCargoCommand = ''
            cargo test --locked --profile release --package diskgc-linux --lib --no-run \
              --message-format json-render-diagnostics > cargo-test.json
          '';
          installPhaseCommand = ''
            jq -r 'select(.reason == "compiler-artifact" and .profile.test
              and .target.name == "diskgc_linux") | .executable' cargo-test.json > exes
            count=$(grep -c . exes || true)
            if [ "$count" -ne 1 ]; then
              echo "error: expected one diskgc-linux lib test executable, got $count" >&2
              exit 1
            fi
            install -Dm0755 "$(cat exes)" $out/bin/diskgc-linux-tests
          '';
        }
      );
      e2eUnderTest = pkgs.runCommand "diskgc-e2e-under-test" { } ''
        mkdir -p $out/bin
        cp ${lib.getExe diskgcTested} $out/bin/diskgc
        cp ${lib.getExe diskgcTested} $out/bin/diskgc-development
        cp ${diskgcProposed01}/bin/diskgc $out/bin/diskgc-proposed-01
        cp ${lib.getExe diskgcReleaseTested} $out/bin/diskgc-release
        cp ${e2eBins}/bin/diskgc-e2e ${e2eBins}/bin/diskgc-e2e-canary \
          ${e2eBins}/bin/diskgc-e2e-driver $out/bin/
        # These copies fail the build if the Nix package omits either
        # shipped file. The test applies the actual drop-in in its container.
        cp ${diskgcTested}/lib/tmpfiles.d/diskgc.conf $out/bin/diskgc-tmpfiles.conf
        cp ${diskgcTested}/share/doc/diskgc/examples/example.yaml $out/bin/diskgc-example.yaml
        cp ${diskgcReleaseTested}/lib/tmpfiles.d/diskgc.conf $out/bin/diskgc-release-tmpfiles.conf
        cp ${diskgcReleaseTested}/share/doc/diskgc/examples/example.yaml $out/bin/diskgc-release-example.yaml
        ln -s ${pkgs.systemd}/bin/systemd-tmpfiles $out/bin/systemd-tmpfiles
        # TASK-97's real Buildah operation runs only in its disposable
        # container. Keep these pinned tools outside the large base image;
        # the runner makes their store closure available read-only there.
        ln -s ${pkgs.buildah}/bin/buildah $out/bin/buildah
        ln -s ${pkgs.shadow}/bin/newuidmap $out/bin/newuidmap
        ln -s ${pkgs.shadow}/bin/newgidmap $out/bin/newgidmap
        # TASK-101 runs a real Meson setup inside a disposable E2E container.
        ln -s ${pkgs.meson}/bin/meson $out/bin/meson
        ln -s ${pkgs.autoconf}/bin/autoconf $out/bin/autoconf
        ln -s ${pkgs.ninja}/bin/ninja $out/bin/ninja
        ln -s ${pkgs.coreutils}/bin/du $out/bin/du
        # The source and tools are immutable Nix inputs; the West build output
        # itself is created under the container's scratch tmpfs.
        ln -s ${zephyr-src} $out/bin/zephyr-src
        ln -s ${westPython}/bin/west $out/bin/west
        ln -s ${westPython}/bin/python3 $out/bin/python3
        # The Nix image already contains this interpreter through pytest;
        # keep the agent hook independent of West's larger Python environment.
        ln -s ${pkgs.python3}/bin/python3 $out/bin/agent-python3
        ln -s ${pkgs.cmake}/bin/cmake $out/bin/cmake
        ln -s ${pkgs.dtc}/bin/dtc $out/bin/dtc
        ln -s ${pkgs.gperf}/bin/gperf $out/bin/gperf
        ln -s ${pkgs.stdenv.cc}/bin/cc $out/bin/cc
        ln -s ${pkgs.stdenv.cc}/bin/gcc $out/bin/gcc
        ln -s ${pkgs.stdenv.cc}/bin/g++ $out/bin/g++
        ln -s ${pkgs.binutils}/bin/ar $out/bin/ar
        ln -s ${pkgs.binutils}/bin/ranlib $out/bin/ranlib
        ln -s ${pkgs.binutils}/bin/ld $out/bin/ld
        ln -s ${pkgs.binutils}/bin/ld.bfd $out/bin/ld.bfd
        ln -s ${pkgs.binutils}/bin/readelf $out/bin/readelf
        ln -s ${pkgs.binutils}/bin/objcopy $out/bin/objcopy
        ln -s ${pkgs.binutils}/bin/objdump $out/bin/objdump
        ln -s ${pkgs.binutils}/bin/nm $out/bin/nm
        ln -s ${pkgs.binutils}/bin/size $out/bin/size
        ln -s ${pkgs.binutils}/bin/strip $out/bin/strip
        ln -s ${pkgs.gnumake}/bin/make $out/bin/make
      '';

      # Identical harness input with the release candidate as the default
      # under-test binary. This makes a runner invocation exercise the
      # package, instead of a separately rebuilt control binary.
      e2eReleaseUnderTest = pkgs.runCommand "diskgc-e2e-release-under-test" { } ''
        cp -a ${e2eUnderTest} $out
        chmod -R u+w $out
        cp ${lib.getExe diskgcReleaseTested} $out/bin/diskgc
        cp ${diskgcReleaseTested}/lib/tmpfiles.d/diskgc.conf $out/bin/diskgc-tmpfiles.conf
        cp ${diskgcReleaseTested}/share/doc/diskgc/examples/example.yaml $out/bin/diskgc-example.yaml
        touch $out/bin/diskgc-release-default
      '';

      # The release package's dependency tree as built (--locked, normal and
      # build edges), one package per line, for the release-excludes-e2e
      # check: the product must never depend on diskgc-e2e.
      releaseDepTree = craneLib.mkCargoDerivation (
        workspaceArgs
        // {
          pname = "diskgc-release-dep-tree";
          cargoArtifacts = null;
          doInstallCargoArtifacts = false;
          buildPhaseCargoCommand = ''
            cargo tree --locked --offline --package diskgc \
              --features proposed-release-01-catalog,diskgc-linux/release-without-sandbox-gate \
              --edges normal,build \
              --prefix none --format '{p}' > dep-tree.txt
          '';
          installPhaseCommand = "cp dep-tree.txt $out";
        }
      );

      # TASK-195: the gate-lift feature without the release catalog must not
      # compile (main.rs's const assert), or an ad-hoc cargo build makes an
      # ungated binary with the development catalog. The release's own
      # feature pair must compile, so the refusal is the assert, not a
      # broken build.
      releaseGateNeedsCatalog = craneLib.mkCargoDerivation (
        workspaceArgs
        // {
          pname = "diskgc-release-gate-needs-catalog";
          cargoArtifacts = workspaceDeps;
          doInstallCargoArtifacts = false;
          buildPhaseCargoCommand = ''
            if cargoWithProfile check --locked --offline --package diskgc \
              --features diskgc-linux/release-without-sandbox-gate 2> refused.log; then
              echo "error: the gate-lift feature compiled without the release catalog" >&2
              exit 1
            fi
            if ! grep -F "needs diskgc's proposed-release-01-catalog" refused.log; then
              cat refused.log >&2
              echo "error: the build failed, but not on the gate/catalog assert" >&2
              exit 1
            fi
            cargoWithProfile check --locked --offline --package diskgc \
              --features proposed-release-01-catalog,diskgc-linux/release-without-sandbox-gate \
              --quiet
          '';
          installPhaseCommand = "touch $out";
        }
      );

      # Cargo under test (docs/testing.md, Cargo toolchains under test): the
      # toolchains the Cargo race tests run against, pinned by the locked
      # rust-overlay input (flake.lock), separate from the product toolchain
      # (rust-toolchain.toml). Minimal profile: rustc, cargo, the host's
      # rust-std; no docs, no rust-src. `tier` is the gate that runs each
      # toolchain's Cargo tests: "fast" (`just e2e` and `just e2e-full`) or
      # "full" (`just e2e-full` only); at least one must be fast. Why each
      # is pinned: docs/testing.md, Cargo toolchains under test.
      cargoUnderTest = [
        {
          name = "1.90.0";
          toolchain = pkgs.rust-bin.stable."1.90.0".minimal;
          tier = "full";
        }
        {
          name = "1.99.1";
          toolchain = pkgs.rust-bin.stable."1.99.1".minimal;
          tier = "fast";
        }
        {
          name = "nightly-2026-10-16";
          toolchain = pkgs.rust-bin.nightly."2026-10-16".minimal;
          tier = "full";
        }
      ];
      # The C toolchain (linker) every toolchain above links with.
      cargoUnderTestCc = pkgs.stdenv.cc;

      # What every e2e image carries, whatever its userland (docs/testing.md,
      # Container setup: Images): the sandbox marker, the users, the
      # overlay-root fixture file, the mount points, and its descriptor
      # (diskgc-e2e/src/image.rs), whose text is the argument. Runs under
      # fakeroot in the new layer's root directory.
      e2eImageCommon = descriptor: ''
        # Interlock marker (docs/testing.md, Safety interlock, layer 2),
        # baked into the image, never bind-mounted at run time.
        cp ${sandboxMarker} .diskgc-test-sandbox
        chmod 0444 .diskgc-test-sandbox

        # The image's descriptor: what it mounts read-only, whether it has
        # Cargo toolchains under test, which tests it runs.
        printf '%s\n' ${lib.escapeShellArgs descriptor} > .diskgc-test-image
        chmod 0444 .diskgc-test-image

        # Users: tester runs the code under test; other owns the
        # "other user's" fixtures (both mapped through the subuid range).
        # These files replace the userland's own.
        mkdir -p etc
        printf '%s\n' \
          'root:x:0:0:root:/root:/bin/sh' \
          'tester:x:1000:1000:diskgc code under test:/home/tester:/bin/sh' \
          'other:x:1001:1001:other owner:/home/other:/bin/sh' \
          'nobody:x:65534:65534:nobody:/nonexistent:/bin/false' > etc/passwd
        printf '%s\n' \
          'root:x:0:' 'tester:x:1000:' 'other:x:1001:' 'nogroup:x:65534:' > etc/group
        printf '%s\n' 'passwd: files' 'group: files' > etc/nsswitch.conf
        mkdir -p root home/tester home/other
        chmod 0700 root
        chown -R 1001:1001 home/other
        chmod 0755 home/tester home/other

        # A tester-owned file on the overlay root, outside every scratch
        # mount (TESTER_OVERLAY_FILE in diskgc-e2e/src/suite.rs): the
        # oracle tests delete it to prove the snapshot covers the overlay.
        mkdir -p srv/tester
        printf 'on the overlay root\n' > srv/tester/on-overlay
        chown -R 1000:1000 srv/tester
        chmod 0755 srv/tester

        # Mount point of the Cargo smoke check's own tmpfs (SMOKE_MOUNT in
        # diskgc-e2e/src/cargo_under_test.rs), unmounted after each check.
        mkdir -p srv/cargo-smoke
        chmod 0755 srv/cargo-smoke

        # Mount points. The harness mounts tmpfs on /tmp and /var/tmp (scratch), on
        # /run/diskgc (the host lock, provisioned there as the tmpfiles
        # lines of docs/safety.md do on a host whose /run is a tmpfs), on
        # /home/tester (tester's home) and on /run/diskgc-e2e (its
        # records); the runner mounts the binaries under test on
        # /opt/diskgc-e2e/bin.
        mkdir -p tmp var/tmp run/diskgc run/diskgc-e2e opt/diskgc-e2e/bin
        chmod 1777 tmp var/tmp
        chmod 0755 run/diskgc
        chmod 0700 run/diskgc-e2e
      '';

      # The pinned pytest with pytest-xdist (and execnet) in one interpreter,
      # for the pytest contract's orphan-worker container test
      # (diskgc-e2e/src/pytest_contract_suite.rs).
      pytestXdistEnv = pkgs.python3.withPackages (ps: [
        ps.pytest
        ps.pytest-xdist
      ]);

      # A pure flake whose evaluation lasts long enough to inspect Nix's
      # naturally opened eval-cache descriptor. Impure evaluation disables
      # that cache in Nix itself.
      nixEvalFixture = pkgs.writeTextDir "flake.nix" ''
        {
          outputs = { self }: {
            held = toString (builtins.foldl' (acc: _: acc + 1) 0
              (builtins.genList (x: x) 10000000));
          };
        }
      '';

      # Real cache cleaners for the shipped command profiles. The image
      # receives their pinned executables and runtime closures, so the
      # profile tests exercise the programs instead of command stubs.
      jestCleaner = pkgs.buildNpmPackage {
        pname = "diskgc-e2e-jest-cleaner";
        version = "30.2.0";
        src = ./diskgc-e2e/npm-jest;
        npmDepsHash = "sha256-QF+jyQKo0gqfzxtaxvIGLtQsFe3vJEonU/IDmpQOQEE=";
        dontNpmBuild = true;
        dontNpmPrune = true;
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postInstall = ''
          mkdir -p $out/bin
          makeWrapper ${pkgs.nodejs}/bin/node $out/bin/jest \
            --add-flags $out/lib/node_modules/diskgc-e2e-jest-cleaner/node_modules/jest/bin/jest.js
        '';
      };
      cacheTools = pkgs.buildEnv {
        name = "diskgc-cache-tools";
        paths = [
          pkgs.python3Packages.pip
          pkgs.uv
          pkgs.go
          pkgs.nodejs
          pkgs.pnpm
          pkgs.yarn
          pkgs.ccache
          pkgs.platformio
          jestCleaner
        ];
        pathsToLink = [ "/bin" ];
      };

      # The Nix base image: busybox, strace, Nix, systemd-tmpfiles, cache cleaners and the
      # Cargo toolchains under test.
      # Nothing here depends on diskgc's or the harness's code, so its tag
      # (the Nix hash of its configuration) changes only when these inputs
      # do, and the runner reuses a loaded image with that tag.
      e2eBaseImage = pkgs.dockerTools.streamLayeredImage {
        name = "localhost/diskgc-e2e-base";
        contents = [
          pkgs.busybox
          pkgs.strace
          pkgs.nix
          pkgs.systemd
          # Pinned real pytest for the live no-lock temp-directory counterexample.
          pkgs.python3Packages.pytest
          nixEvalFixture
          cacheTools
        ];
        fakeRootCommands = ''
          ${e2eImageCommon [
            "name nix"
            "read-only /nix/store"
            "toolchains manifest"
            "tests all"
            "tmpfiles /opt/diskgc-e2e/bin/systemd-tmpfiles"
          ]}
          # Cargo under test (diskgc-e2e/src/cargo_under_test.rs): one
          # symlink per toolchain and one to the C toolchain, into the
          # image's /nix/store, which the harness bind-mounts read-only (the
          # descriptor's read-only) so the deletion oracle's snapshot does
          # not walk it; the manifest lists `NAME VERSION TIER` per
          # toolchain, in the order above. Nothing under /home/tester: the
          # harness mounts a fresh tmpfs there.
          mkdir -p opt/cargo-under-test
          mkdir -p opt/nix-eval
          ln -s ${pkgs.nix}/bin/nix opt/nix-eval/nix
          # The shell nix-shell and nix develop start in the lifecycle
          # tests (diskgc-e2e/src/lifecycle_suite.rs); offline, nix develop
          # cannot fetch bashInteractive.
          ln -s ${pkgs.bash}/bin/bash opt/nix-eval/bash
          ln -s ${pkgs.python3Packages.pytest}/bin/pytest opt/pytest-under-test
          # Reached only through this link (as the Cargo toolchains are), so
          # its bin/pytest does not collide with pytest's in the image root.
          ln -s ${pytestXdistEnv}/bin/pytest opt/pytest-xdist-under-test
          ln -s ${nixEvalFixture} opt/nix-eval/flake
          ln -s ${cargoUnderTestCc} opt/cargo-under-test/cc
          ${lib.concatMapStrings (t: ''
            ln -s ${t.toolchain} opt/cargo-under-test/${t.name}
          '') cargoUnderTest}
          printf '%s\n' ${
            lib.escapeShellArgs (map (t: "${t.name} ${t.toolchain.version} ${t.tier}") cargoUnderTest)
          } > opt/cargo-under-test/toolchains
        '';
        config = {
          Cmd = [ "/bin/sh" ];
          Env = [ "PATH=/bin" ];
          WorkingDir = "/";
        };
      };

      # A small VM-only variant of the same interlocked E2E image. The
      # ordinary image contains several gigabytes of unrelated toolchains;
      # this one mounts the guest's Nix store read-only for the test binaries.
      e2eVmImage = pkgs.dockerTools.streamLayeredImage {
        name = "localhost/diskgc-e2e-vm";
        contents = [ pkgs.busybox pkgs.strace ];
        fakeRootCommands = (e2eImageCommon [
          "name vm"
          "read-only /nix/store"
          "toolchains none"
          "tests all"
          "tmpfiles /opt/diskgc-e2e/bin/systemd-tmpfiles"
        ]) + ''
          # The containers-image vector names Buildah's process. Bake a
          # static fixture executable at that exact path; its probe holds
          # the spool open while LinuxHost inspects /proc.
          mkdir -p usr/bin
          cp ${e2eBins}/bin/diskgc-e2e usr/bin/buildah
          chmod 0755 usr/bin/buildah
        '';
        config = {
          Cmd = [ "/bin/sh" ];
          Env = [ "PATH=/bin" ];
          WorkingDir = "/";
        };
      };

      # The Debian image (docs/testing.md, Container setup: Images): no Nix
      # inside, the static binaries under test only. Its userland is the
      # official Rust image for the fast toolchain's release on Debian 13
      # (trixie), pinned by digest: Debian's dash as /bin/sh, coreutils,
      # gcc as the linker, and a rustup-installed (glibc-dynamic, non-Nix)
      # toolchain whose real binaries the manifest points at, so Cargo runs
      # without rustup's proxies or any variable. strace is the one file
      # added from Nix: the static musl build of the same strace release
      # the Nix image has (Debian's needs shared libraries), copied as a
      # plain file, so no /nix/store exists in the container.
      # The release of the Debian image's Cargo: the image tag, the rustup
      # toolchain's directory and the manifest line follow it. Changing it
      # needs the new tag's digest and hash (nix-prefetch-docker).
      debianRustVersion = "1.99.1";
      debianRustImage = pkgs.dockerTools.pullImage {
        imageName = "library/rust";
        imageDigest = "sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d";
        hash = "sha256-7PLkeybbGZGXoQeSaRrU+4WeLt4UuMseSLi5JHjgu2E=";
        finalImageName = "library/rust";
        finalImageTag = "${debianRustVersion}-slim-trixie";
        os = "linux";
        arch = "amd64";
      };
      debianRustToolchain = "${debianRustVersion}-x86_64-unknown-linux-gnu";
      # Debian's standalone implementation of systemd-tmpfiles, pinned like
      # the Debian base image. Its runtime libraries are already in that
      # image; this keeps the portable test image free of a Nix store.
      debianTmpfilesDeb = pkgs.fetchurl {
        url = "https://deb.debian.org/debian/pool/main/s/systemd/systemd-standalone-tmpfiles_257.13-1~deb13u1_amd64.deb";
        hash = "sha256-zdW13icGIGsDvy1Qu1VvWaJHAyTE3p5a2pS68MDz5bk=";
      };
      debianTmpfiles = pkgs.runCommand "diskgc-debian-systemd-tmpfiles" {
        nativeBuildInputs = [ pkgs.dpkg ];
      } ''
        mkdir unpack
        dpkg-deb -x ${debianTmpfilesDeb} unpack
        install -Dm0755 unpack/usr/bin/systemd-tmpfiles $out/bin/systemd-tmpfiles
      '';
      e2eDebianImage = pkgs.dockerTools.streamLayeredImage {
        name = "localhost/diskgc-e2e-debian";
        fromImage = debianRustImage;
        fakeRootCommands = ''
          ${e2eImageCommon [
            "name debian"
            "read-only /usr"
            "toolchains manifest"
            "tests portable"
            "tmpfiles /usr/bin/systemd-tmpfiles"
          ]}
          # strace where the harness runs it (/bin is Debian's link to
          # /usr/bin; this layer must not make a /bin of its own).
          mkdir -p usr/bin
          cp ${pkgs.pkgsStatic.strace}/bin/strace usr/bin/strace
          chmod 0755 usr/bin/strace
          cp ${debianTmpfiles}/bin/systemd-tmpfiles usr/bin/systemd-tmpfiles
          chmod 0755 usr/bin/systemd-tmpfiles

          # Cargo under test: the image's rustup toolchain (its real cargo
          # and rustc, not rustup's proxies) and Debian's gcc as the C
          # toolchain; /usr is mounted read-only by the harness.
          mkdir -p opt/cargo-under-test
          ln -s /usr/local/rustup/toolchains/${debianRustToolchain} \
            opt/cargo-under-test/debian-${debianRustVersion}
          ln -s /usr opt/cargo-under-test/cc
          printf '%s\n' 'debian-${debianRustVersion} ${debianRustVersion} fast' > opt/cargo-under-test/toolchains
        '';
        config = {
          Cmd = [ "/bin/sh" ];
          Env = [ "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" ];
          WorkingDir = "/";
        };
      };

      # A native Docker Engine, separate from the host's Podman-backed
      # `docker` command. The manifest digest and Nix hash pin the exact
      # upstream dind image; the derived image adds the same test interlock
      # and users as every other e2e image. The daemon starts only inside
      # docker_suite's disposable test container, never in the host runner.
      dockerDindImage = pkgs.dockerTools.pullImage {
        imageName = "library/docker";
        imageDigest = "sha256:1e08cdb63405ca788aea94ef35b792d1e299607c667d33d7bcbcae1fd2611ced";
        hash = "sha256-dM+7He+/ypfWJvPcekbICoyda3XT+xc3r2Ru9kIDi+4=";
        finalImageName = "library/docker";
        finalImageTag = "29.8.2-dind";
        os = "linux";
        arch = "amd64";
      };
      # Alpine's standalone tmpfiles implementation and its two runtime
      # libraries, pinned to the same Alpine 3.24 branch as dockerDindImage.
      # Extract the APKs without executing their install scripts or reaching
      # an APK repository when the disposable test image starts.
      alpineSdToolsApk = pkgs.fetchurl {
        url = "https://dl-cdn.alpinelinux.org/alpine/v3.24/community/x86_64/sd-tools-0.99.0-r3.apk";
        hash = "sha256-k8/HfJwnL30Njn44eb0iF3DD4/BEkMx/uAgNLL9REPM=";
      };
      alpineAclLibsApk = pkgs.fetchurl {
        url = "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/acl-libs-2.3.2-r1.apk";
        hash = "sha256-PwKFHVhsJdl7qLW4RWFarMIJrn6utveEPsp22F2iJJ0=";
      };
      alpineLibcap2Apk = pkgs.fetchurl {
        url = "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/libcap2-2.78-r0.apk";
        hash = "sha256-Qohqe0vN4mNHxxKSXI3H2yArYCnvm4x+6BV3q3vdj60=";
      };
      alpineTmpfiles = pkgs.runCommand "diskgc-alpine-sd-tmpfiles" {
        nativeBuildInputs = [ pkgs.busybox ];
      } ''
        mkdir -p $out
        for archive in ${alpineSdToolsApk} ${alpineAclLibsApk} ${alpineLibcap2Apk}; do
          busybox tar -xzf "$archive" -C $out
        done
        test -x $out/usr/bin/sd-tmpfiles
        test -f $out/usr/lib/libacl.so.1
        test -f $out/usr/lib/libcap.so.2
      '';
      e2eDockerImage = pkgs.dockerTools.streamLayeredImage {
        name = "localhost/diskgc-e2e-dind";
        fromImage = dockerDindImage;
        fakeRootCommands = ''
          ${e2eImageCommon [
            "name docker-dind"
            "toolchains none"
            "tests docker"
            "tmpfiles /usr/bin/sd-tmpfiles"
          ]}
          # The static strace binary is copied as a file. No Nix store is
          # mounted into the test container.
          mkdir -p bin
          cp ${pkgs.pkgsStatic.strace}/bin/strace bin/strace
          chmod 0755 bin/strace
          mkdir -p usr/bin usr/lib
          cp ${alpineTmpfiles}/usr/bin/sd-tmpfiles usr/bin/sd-tmpfiles
          cp -P ${alpineTmpfiles}/usr/lib/libacl.so.1* usr/lib/
          cp -P ${alpineTmpfiles}/usr/lib/libcap.so.2* usr/lib/
          cp -P ${alpineTmpfiles}/usr/lib/libpsx.so.2* usr/lib/
        '';
        config = {
          Cmd = [ "/bin/sh" ];
          Env = [ "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" ];
          WorkingDir = "/";
        };
      };

      # The host-side runner with the image and binaries baked into its
      # arguments: `nix run .#e2e-runner -- run --suite fast`. A compiled
      # wrapper (makeBinaryWrapper), not a shell script.
      mkE2eRunner =
        name: image: runtime: underTest:
        pkgs.runCommand name
          {
            nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
            meta.mainProgram = "diskgc-e2e-runner";
          }
          ''
            makeBinaryWrapper ${e2eBins}/bin/diskgc-e2e-runner $out/bin/diskgc-e2e-runner \
              --add-flags "--image-stream ${image}" \
              --add-flags "--image-ref ${image.imageName}:${image.imageTag}" \
              --add-flags "--runtime ${runtime}" \
              --add-flags "--under-test ${underTest}"
          '';
      e2eRunner = mkE2eRunner "diskgc-e2e-runner" e2eBaseImage "standard" e2eUnderTest;
      # The same runner on the Debian image: `nix run .#e2e-runner-debian`.
      e2eRunnerDebian = mkE2eRunner "diskgc-e2e-runner-debian" e2eDebianImage "standard" e2eUnderTest;
      e2eRunnerDocker = mkE2eRunner "diskgc-e2e-runner-docker" e2eDockerImage "dind" e2eUnderTest;
      e2eRunnerRelease = mkE2eRunner "diskgc-e2e-runner-release" e2eBaseImage "standard" e2eReleaseUnderTest;
      e2eRunnerReleaseDebian = mkE2eRunner "diskgc-e2e-runner-release-debian" e2eDebianImage "standard" e2eReleaseUnderTest;
      vmHistoricalCtime = import ./diskgc-e2e/vm-historical-ctime.nix {
        inherit pkgs;
        image = e2eVmImage;
        underTest = e2eUnderTest;
      };
      # TASK-156: the release package reclaims genuinely old /tmp sessions.
      vmReleaseTmpReclaim = import ./diskgc-e2e/vm-release-tmp-reclaim.nix {
        inherit pkgs;
        release = diskgcReleaseTested;
        versionLine = testedVersionLine;
      };
      vmColdScale = import ./diskgc-e2e/vm-cold-scale.nix {
        inherit pkgs;
        image = e2eVmImage;
        underTest = e2eUnderTest;
      };
      # TASK-29: the background tally's pressure brake on a guest whose
      # disk is throttled (its /proc/pressure is its own).
      vmTallyPressure = import ./diskgc-e2e/vm-tally-pressure.nix {
        inherit pkgs;
        image = e2eVmImage;
        underTest = e2eUnderTest;
      };
      mkVmPidfd = vmPkgs: kernelPackages: expectedKernel: expectPollHup: cores: extra:
        import ./diskgc-e2e/vm-old-kernel-pidfd.nix ({
          pkgs = vmPkgs;
          inherit kernelPackages expectedKernel expectPollHup cores;
          kernelFactTests = diskgcLinuxTests;
          image = e2eVmImage;
          underTest = e2eUnderTest;
        } // extra);
      # Nixpkgs 20.09 predates pkgs.testers.nixosTest. Keep its VM driver
      # and guest together so the test module API matches the guest release.
      nixosTest58 = (import "${nixpkgs58.outPath}/nixos/lib/testing-python.nix" {
        inherit system;
        pkgs = pkgs58;
      }).makeTest;
      vmOldKernelPidfd = mkVmPidfd pkgs pkgs.linuxPackages_6_6 "6.6.158" false 2 { };
      vmPidfdLinux61 = mkVmPidfd pkgs pkgs.linuxPackages_6_1 "6.1.189" false 2 { };
      vmPidfdLinux612 = mkVmPidfd pkgs pkgs.linuxPackages_6_12 "6.12.112" true 2 { };
      vmPidfdLinux515 = mkVmPidfd pkgs515 pkgs515.linuxPackages_5_15 "5.15.221" false 4 { };
      vmPidfdLinux58 = mkVmPidfd pkgs58 pkgs58.linuxPackages_5_8 "5.8.16" false 4 {
        nixosTest = nixosTest58;
        probePython = pkgs58.python39;
        probePythonCommand = "${pkgs58.python39}/bin/python3.9";
        # The 20.09 driver runs black --check over the generated test
        # script; its formatting differs from the current driver's, and the
        # lint checks no behaviour.
        driverAttrs.skipLint = true;
        # Podman 2.1.1's built-in seccomp profile (2020) answers pidfd_open
        # with EPERM, and diskgc refuses to start without it (measured in
        # this guest; docs/safety.md, In use). The current Podman of the
        # other kernel points allows it. This point tests the kernel, so its
        # containers run without seccomp; their isolation (namespaces,
        # read-only binds, the VM's private disk, the sandbox gate) does not
        # rest on seccomp.
        podmanRunOptions = "--security-opt seccomp=unconfined";
      };
      vmPidfdLinux68 = mkVmPidfd pkgs68 pkgs68.linuxPackages_6_8 "6.8.12" false 2 { };
      # The NixOS VM checks, one list: they are packages and checks, and the
      # dev shell leaves them out.
      vmChecks = {
        vm-historical-ctime = vmHistoricalCtime;
        vm-cold-scale = vmColdScale;
        vm-tally-pressure = vmTallyPressure;
        vm-release-tmp-reclaim = vmReleaseTmpReclaim;
        vm-old-kernel-pidfd = vmOldKernelPidfd;
        vm-pidfd-linux-6-1 = vmPidfdLinux61;
        vm-pidfd-linux-6-12 = vmPidfdLinux612;
        vm-pidfd-linux-5-15 = vmPidfdLinux515;
        vm-pidfd-linux-5-8 = vmPidfdLinux58;
        vm-pidfd-linux-6-8 = vmPidfdLinux68;
        vm-nixos-module = vmNixosModule;
      };
      # The NixOS module (docs/architecture.md, NixOS module). Its default
      # package is the development package of the evaluating system.
      nixosModule = import ./packaging/nixos/module.nix {
        defaultPackage =
          p:
          self.packages.${p.stdenv.hostPlatform.system}.default
            or (throw "diskgc: no package for ${p.stdenv.hostPlatform.system}; only ${system} is built");
      };
      # Its checks: evaluation and the build-time check (no VM), and one
      # NixOS VM test, run by `just e2e-full` only (docs/testing.md,
      # Deployment units, 4 and 5). Like the VM checks, the dev shell
      # leaves them out: each evaluates NixOS systems.
      nixosModuleChecks = import ./packaging/nixos/checks.nix {
        inherit pkgs lib nixpkgs systemdVerify;
        module = nixosModule;
        package = diskgcTested;
        defaultPackage = diskgc;
      };
      vmNixosModule = import ./packaging/nixos/vm-test.nix {
        inherit pkgs;
        module = nixosModule;
        package = diskgcTested;
      };
    in
    {
      nixosModules.default = nixosModule;

      packages.${system} = {
        default = diskgc;
        release-candidate = diskgcRelease;
        e2e-runner = e2eRunner;
        e2e-runner-debian = e2eRunnerDebian;
        e2e-runner-docker = e2eRunnerDocker;
        e2e-runner-release = e2eRunnerRelease;
        e2e-runner-release-debian = e2eRunnerReleaseDebian;
        e2e-base-image = e2eBaseImage;
        e2e-vm-image = e2eVmImage;
        e2e-debian-image = e2eDebianImage;
        e2e-docker-image = e2eDockerImage;
        e2e-under-test = e2eUnderTest;
        e2e-release-under-test = e2eReleaseUnderTest;
      }
      // vmChecks;

      checks.${system} = {
        diskgc = diskgcTested;

        # Static config validation must run in a Nix build sandbox: it may
        # read the config, but cannot require deployment roots or backends.
        # The example's root does not exist in the sandbox: check warns with
        # the decision ensure refuses with (TASK-194), which needs its host
        # to start in a build sandbox, and still exits 0.
        config-check-sandbox = pkgs.runCommand "diskgc-config-check-sandbox" { } ''
          output=$(${lib.getExe diskgcTested} check --config ${./docs/example.yaml} 2> stderr)
          case "$output" in
            *"profile:cargo-target && filter:idle-30d"*) ;;
            *) echo "check did not show the normalized tier" >&2; exit 1 ;;
          esac
          if ! grep -F "warning: root /home/example/src does not exist on this machine" stderr; then
            cat stderr >&2
            echo "check did not warn about the example's absent root" >&2
            exit 1
          fi
          touch $out
        '';

        # The packaged `watch` templates as systemd reads them
        # (docs/testing.md, Deployment units, 2): each linked under two
        # instance names (`srv-ci\x2dbuilds`, and `-` for `/`), its
        # ExecStart pointed at the packaged binary, verified in system and
        # --user mode against systemd's own units for that manager only.
        # `systemd-analyze verify` only warns on unknown keys and bad values
        # (exit 0), so any output fails the check; two negative controls
        # show that a missing binary and an unknown key are caught. The exact
        # text and the hardening lines are tools/unit_lint.py's (just check).
        deployment-units = pkgs.runCommand "diskgc-deployment-units-check"
          { nativeBuildInputs = [ pkgs.systemd pkgs.util-linux ]; }
          ''
            export HOME=$TMPDIR/home XDG_RUNTIME_DIR=$TMPDIR/run
            mkdir -p $HOME $XDG_RUNTIME_DIR $TMPDIR/root
            bin=${lib.getExe diskgcTested}
            for manager in user system; do
              cmp ${diskgcTested}/lib/systemd/$manager/diskgc-watch@.service \
                ${watchUnits}/$manager/diskgc-watch@.service
            done
            # stage MANAGER DIR BIN [EXTRA-LINE]: the template in DIR, pointed
            # at BIN, with both instance names linked to it.
            stage() {
              mkdir -p "$2"
              sed "s|^ExecStart=/usr/local/bin/diskgc |ExecStart=$3 |" \
                ${diskgcTested}/lib/systemd/$1/diskgc-watch@.service > "$2/diskgc-watch@.service"
              grep -qF "ExecStart=$3 watch " "$2/diskgc-watch@.service"
              if [ -n "''${4-}" ]; then printf '%s\n' "$4" >> "$2/diskgc-watch@.service"; fi
              ln -s diskgc-watch@.service "$2/diskgc-watch@srv-ci\x2dbuilds.service"
              ln -s diskgc-watch@.service "$2/diskgc-watch@-.service"
            }
            ${systemdVerify}
            # verify_staged MANAGER DIR: verify both instance names in DIR.
            verify_staged() {
              verify "$1" "$2" "$2/diskgc-watch@srv-ci\x2dbuilds.service" "$2/diskgc-watch@-.service"
            }
            for manager in user system; do
              stage $manager $TMPDIR/$manager "$bin"
              if ! verify_staged $manager $TMPDIR/$manager || [ -s $TMPDIR/$manager.log ]; then
                cat $TMPDIR/$manager.log >&2
                echo "error: systemd-analyze verify rejected or warned about the $manager template" >&2
                exit 1
              fi
              echo "$manager template: verified, no output"

              stage $manager $TMPDIR/$manager-nobin $TMPDIR/absent/diskgc
              if verify_staged $manager $TMPDIR/$manager-nobin; then
                cat $TMPDIR/$manager-nobin.log >&2
                echo "error: negative control: verify accepted a missing binary ($manager)" >&2
                exit 1
              fi
              grep -F 'is not executable' $TMPDIR/$manager-nobin.log

              stage $manager $TMPDIR/$manager-badkey "$bin" 'NoSuchKey=1'
              verify_staged $manager $TMPDIR/$manager-badkey || true
              if ! grep -F "Unknown key 'NoSuchKey'" $TMPDIR/$manager-badkey.log; then
                cat $TMPDIR/$manager-badkey.log >&2
                echo "error: negative control: verify said nothing about an unknown key ($manager)" >&2
                exit 1
              fi
            done
            touch $out
          '';

        # Guards acceptance criterion "statically linked": no program
        # interpreter and no shared-library dependencies, and it runs.
        diskgc-static =
          pkgs.runCommand "diskgc-static-check" { nativeBuildInputs = [ pkgs.binutils pkgs.stdenv.cc ]; }
            ''
              ${checkStaticElf}
              bin=${lib.getExe diskgcTested}
              check_static_elf "$bin"

              # A locally compiled glibc-linked executable is the negative
              # control. Assert its dynamic records first, so this proof
              # cannot pass if the compiler ever changes its link defaults.
              cat > "$TMPDIR/dynamic-control.c" <<'C'
              #include <stdio.h>
              int main(void) { return puts("dynamic control") < 0; }
              C
              cc "$TMPDIR/dynamic-control.c" -o "$TMPDIR/dynamic-control"
              program_headers="$(readelf --wide --program-headers "$TMPDIR/dynamic-control")"
              dynamic_section="$(readelf --wide --dynamic "$TMPDIR/dynamic-control")"
              case "$program_headers" in *INTERP*) ;; *) echo "negative control has no interpreter" >&2; exit 1 ;; esac
              case "$dynamic_section" in *NEEDED*) ;; *) echo "negative control has no shared-library dependency" >&2; exit 1 ;; esac
              if check_static_elf "$TMPDIR/dynamic-control"; then
                echo "error: static-binary check accepted a dynamically linked ELF" >&2
                exit 1
              fi
              got=$("$bin" --version)
              want=${lib.escapeShellArg testedVersionLine}
              if [ "$got" != "$want" ]; then
                echo "error: --version printed '$got', expected '$want'" >&2
                exit 1
              fi
              touch $out
            '';

        # The test driver and the controlled Host wrapper (diskgc-e2e) are
        # not in the release package: neither their symbols nor their strings
        # are in the shipped binary, and its dependency tree has no
        # diskgc-e2e. The driver binary itself must be flagged by the same
        # needles (diskgc-e2e/src/release_check.rs).
        release-excludes-e2e = pkgs.runCommand "diskgc-release-excludes-e2e" { } ''
          ${e2eBins}/bin/diskgc-e2e-release-check \
            --shipped ${lib.getExe diskgcReleaseTested} \
            --dep-tree ${releaseDepTree} \
            --control ${e2eBins}/bin/diskgc-e2e-driver
          touch $out
        '';

        release-excludes-clock-offset = pkgs.runCommand "diskgc-release-excludes-clock-offset" { } ''
          ${e2eBins}/bin/diskgc-e2e-release-check \
            --clock-offset-excluded ${lib.getExe diskgcReleaseTested} \
            --clock-offset-control ${lib.getExe diskgcTested}
          touch $out
        '';

        release-gate-needs-catalog = releaseGateNeedsCatalog;

        # The release package as the tests see it (unstamped); for a
        # release version also the shipped binary, which is then the same.
        release-candidate-package = pkgs.runCommand "diskgc-release-candidate-package-check"
          { nativeBuildInputs = [ pkgs.binutils ]; }
          ''
            ${checkStaticElf}
            pkg=${diskgcReleaseTested}
            bin=${lib.getExe diskgcReleaseTested}
            check_static_elf "$bin"
            if grep -aFq '/.diskgc-test-sandbox' "$bin"; then
              echo "candidate release binary still contains the sandbox marker" >&2
              exit 1
            fi
            if ! grep -aFq '/.diskgc-test-sandbox' ${lib.getExe diskgcTested}; then
              echo "development control binary lacks the sandbox marker" >&2
              exit 1
            fi
            cmp $pkg/lib/tmpfiles.d/diskgc.conf ${./packaging/diskgc.conf}
            cmp $pkg/share/doc/diskgc/examples/example.yaml ${./docs/example.yaml}
            cmp $pkg/share/doc/diskgc/README.md ${./packaging/README.md}
            cmp $pkg/share/doc/diskgc/reference.md ${./docs/reference.md}
            head -n 1 $pkg/share/doc/diskgc/RELEASE-NOTES.md \
              | grep -qxF ${lib.escapeShellArg releaseNotesTitle}
            cmp "$bin" ${lib.getExe diskgcReleaseBuilds.tested}
            got=$("$bin" --version)
            if [ "$got" != ${lib.escapeShellArg testedVersionLine} ]; then
              echo "error: --version printed '$got', expected '${testedVersionLine}'" >&2
              exit 1
            fi
            ${lib.optionalString (!isDevVersion) "cmp \"$bin\" ${lib.getExe diskgcRelease}"}
            touch $out
          '';

        # The stamp: the shipped packages print their commit (a release
        # version: the version alone), and differ from the tested ones only
        # in the binary. The one check that builds the stamped packages; no
        # VM, no container.
        version-stamp = pkgs.runCommand "diskgc-version-stamp-check" { } ''
          for bin in ${lib.getExe diskgc} ${lib.getExe diskgcRelease}; do
            got=$("$bin" --version)
            if [ "$got" != ${lib.escapeShellArg shippedVersionLine} ]; then
              echo "error: $bin --version printed '$got', expected '${shippedVersionLine}'" >&2
              exit 1
            fi
          done
          diff -r --exclude=bin ${diskgcTested} ${diskgc}
          diff -r --exclude=bin ${diskgcReleaseTested} ${diskgcRelease}
          touch $out
        '';

        workspace-fmt = craneLib.cargoFmt workspaceArgs;

        workspace-clippy = craneLib.cargoClippy (
          workspaceArgs
          // {
            cargoArtifacts = workspaceDeps;
            cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
          }
        );

        workspace-tests = craneLib.cargoTest (
          workspaceArgs
          // {
            src = workspaceTestSrc;
            cargoArtifacts = workspaceDeps;
            # --no-fail-fast: one failing test binary must not hide the
            # rest (TASK-133: diskgc-model and diskgc-core integration
            # binaries never ran while diskgc-linux failed).
            cargoTestExtraArgs = "--workspace --no-fail-fast";
          }
        );

        e2e-fmt = craneLib.cargoFmt e2eArgs;

        e2e-clippy = craneLib.cargoClippy (
          e2eArgs
          // {
            cargoArtifacts = e2eDeps;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          }
        );

        # Pure-function unit tests of the harness (parsers, oracle diff,
        # runner verdict). Nothing here needs a container.
        e2e-tests = craneLib.cargoTest (e2eArgs // { cargoArtifacts = e2eDeps; });

        # The runners, image scripts and binaries under test build; running
        # them needs Podman, so that happens in `just e2e` and
        # `just e2e-debian`, not here.
        e2e-runner = e2eRunner;
        e2e-runner-debian = e2eRunnerDebian;
        e2e-runner-docker = e2eRunnerDocker;
        e2e-runner-release = e2eRunnerRelease;
        e2e-runner-release-debian = e2eRunnerReleaseDebian;
      }
      // nixosModuleChecks
      // vmChecks;

      # The toolchain from rust-toolchain.toml (host target by default) plus
      # the inputs of every check except the NixOS VM and module checks,
      # which run only through `nix build` and whose NixOS systems (one per
      # kernel point or module example) would be evaluated by every
      # `nix develop`. Run recipes as
      # `nix develop -c just <recipe>`.
      devShells.${system} = {
        default = craneLib.devShell {
          checks = removeAttrs self.checks.${system} (
            builtins.attrNames vmChecks ++ builtins.attrNames nixosModuleChecks
          );
          # perf and inferno: profiling the reclaim walk (TASK-292).
          packages = [
            pkgs.just
            pkgs.python3
            pkgs.coreutils
            pkgs.cargo-mutants
            pkgs.perf
            pkgs.inferno
          ];
          shellHook = devShellHook;
        };

        # This shell does not replace the stable release or test toolchain.
        fuzz = pkgs.mkShell {
          packages = [
            fuzzToolchain
            pkgs.cargo-fuzz
            pkgs.clang
            pkgs.just
            pkgs.python3
            pkgs.ripgrep
            pkgs.coreutils
          ];
          shellHook = devShellHook;
        };
      };
    };
}
