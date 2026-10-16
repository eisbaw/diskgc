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

      # Source allowlist: only Cargo manifests, the lock file and Rust/TOML
      # files under the crate directories reach the build. docs/, backlog/,
      # *.md, flake.nix and the justfile are not build inputs, so editing them
      # never changes an output path.
      workspaceFiles = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        (craneLib.fileset.commonCargoSources ./crates)
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
    in
    {
      packages.${system}.default = diskgc;

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
      };

      # The toolchain from rust-toolchain.toml (host target by default) plus
      # the inputs of every check. Run recipes as `nix develop -c just <recipe>`.
      devShells.${system}.default = craneLib.devShell {
        checks = self.checks.${system};
        packages = [ pkgs.just ];
      };
    };
}
