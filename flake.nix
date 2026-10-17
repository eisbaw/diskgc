{
  description = "diskgc: keeps enough disk space free by reclaiming rebuildable caches";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
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
      inherit (pkgs) lib;

      # The product toolchain, pinned by rust-toolchain.toml (single source).
      craneLib = (crane.mkLib pkgs).overrideToolchain (
        p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
      );

      # Single source of the version: [workspace.package] in Cargo.toml.
      version = (lib.importTOML ./Cargo.toml).workspace.package.version;

      # The test-sandbox marker constant (docs/testing.md, Safety interlock):
      # compiled into the binaries by include_bytes! and copied byte for byte
      # into the e2e base image. As a path of its own it changes the image
      # only when the marker itself changes.
      sandboxMarker = ./crates/diskgc-linux/src/sandbox-marker;

      # Source allowlist: only Cargo manifests, the lock file, Rust/TOML
      # files under the crate directories and the crates' listed data files
      # reach the build. docs/, backlog/, *.md, flake.nix and the justfile are
      # not build inputs, so editing them never changes an output path.
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
      # diskgc-e2e path-depends on workspace crates, which inherit fields from
      # the root manifest, so its source is the workspace plus its directory.
      e2eSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          workspaceFiles
          (craneLib.fileset.commonCargoSources ./diskgc-e2e)
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

      diskgc = craneLib.buildPackage (
        workspaceArgs
        // {
          pname = "diskgc";
          cargoArtifacts = workspaceDeps;
          cargoExtraArgs = "--locked --package diskgc";
          # Tests run once, in checks.workspace-tests.
          doCheck = false;
          meta.mainProgram = "diskgc";
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

      # ---- Container e2e harness (docs/testing.md, Container setup) ----

      # The diskgc-e2e binaries: the in-container harness and tests, the
      # do-nothing interlock canary, and the host-side runner. Static musl, so
      # they run in any image and on the host.
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
      e2eUnderTest = pkgs.runCommand "diskgc-e2e-under-test" { } ''
        mkdir -p $out/bin
        cp ${lib.getExe diskgc} $out/bin/diskgc
        cp ${e2eBins}/bin/diskgc-e2e ${e2eBins}/bin/diskgc-e2e-canary $out/bin/
      '';

      # The base image: userland, tools, users, markers and mount points (the
      # harness provisions the host lock per test, on a tmpfs).
      # Nothing here depends on diskgc's or the harness's code, so its tag
      # (the Nix hash of its configuration) changes only when these inputs
      # do, and the runner reuses a loaded image with that tag.
      #
      # Extension point (task-4): Cargo-under-test toolchains and their C
      # toolchain go into `contents` (e.g. under /opt/cargo-under-test);
      # they are base-image inputs, not code under test.
      e2eBaseImage = pkgs.dockerTools.streamLayeredImage {
        name = "localhost/diskgc-e2e-base";
        contents = [
          pkgs.busybox
          pkgs.strace
        ];
        # Runs under fakeroot in the image's root directory.
        fakeRootCommands = ''
          # Interlock marker (docs/testing.md, Safety interlock, layer 2).
          cp ${sandboxMarker} .diskgc-test-sandbox
          chmod 0444 .diskgc-test-sandbox

          # Users: tester runs the code under test; other owns the
          # "other user's" fixtures (both mapped through the subuid range).
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

          # Mount points. The harness mounts tmpfs on /tmp (scratch), on
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
        config = {
          Cmd = [ "/bin/sh" ];
          Env = [ "PATH=/bin" ];
          WorkingDir = "/";
        };
      };

      # The host-side runner with the image and binaries baked into its
      # arguments: `nix run .#e2e-runner -- run --suite fast`. A compiled
      # wrapper (makeBinaryWrapper), not a shell script.
      e2eRunner =
        pkgs.runCommand "diskgc-e2e-runner"
          {
            nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
            meta.mainProgram = "diskgc-e2e-runner";
          }
          ''
            makeBinaryWrapper ${e2eBins}/bin/diskgc-e2e-runner $out/bin/diskgc-e2e-runner \
              --add-flags "--image-stream ${e2eBaseImage}" \
              --add-flags "--image-ref ${e2eBaseImage.imageName}:${e2eBaseImage.imageTag}" \
              --add-flags "--under-test ${e2eUnderTest}"
          '';
    in
    {
      packages.${system} = {
        default = diskgc;
        e2e-runner = e2eRunner;
        e2e-base-image = e2eBaseImage;
        e2e-under-test = e2eUnderTest;
      };

      checks.${system} = {
        inherit diskgc;

        # Guards acceptance criterion "statically linked": no program
        # interpreter and no shared-library dependencies, and it runs.
        diskgc-static =
          pkgs.runCommand "diskgc-static-check" { nativeBuildInputs = [ pkgs.binutils ]; }
            ''
              bin=${lib.getExe diskgc}
              if readelf --program-headers "$bin" | grep -q INTERP; then
                echo "error: $bin requests a program interpreter; it is not statically linked" >&2
                exit 1
              fi
              if readelf --dynamic "$bin" | grep -q NEEDED; then
                echo "error: $bin needs shared libraries:" >&2
                readelf --dynamic "$bin" | grep NEEDED >&2
                exit 1
              fi
              got=$("$bin" --version)
              want="diskgc ${version}"
              if [ "$got" != "$want" ]; then
                echo "error: --version printed '$got', expected '$want'" >&2
                exit 1
              fi
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
            cargoArtifacts = workspaceDeps;
            cargoTestExtraArgs = "--workspace";
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

        # The runner, image script and binaries under test build; running
        # them needs Podman, so that happens in `just e2e`, not here.
        e2e-runner = e2eRunner;
      };

      # The toolchain from rust-toolchain.toml (host target by default) plus
      # the inputs of every check. Run recipes as `nix develop -c just <recipe>`.
      devShells.${system}.default = craneLib.devShell {
        checks = self.checks.${system};
        packages = [ pkgs.just ];
      };
    };
}
