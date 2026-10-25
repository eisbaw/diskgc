# diskgc developer recipes (docs/testing.md, Commands).
#
# Run every recipe inside the flake's dev shell, which provides the pinned
# toolchain (rust-toolchain.toml) and just itself:
#
#     nix develop -c just <recipe>      # one-off
#     nix develop                       # then: just <recipe>
#
# Recipes call Nix, Cargo, Podman and the scratch tally's Python helper (docs/architecture.md,
# Implementation). The dev shell builds for the host (gnu) target; the flake's
# package and checks build for static musl.

# Inspect project scratch, legacy /tmp/diskgc* and free space before a task.
scratch-tally:
    python3 tools/scratch_tally.py

# List the recipes.
default:
    @just --list

# diskgc-e2e is only format-checked here: compiling and unit-testing it is
# left to the flake (e2e-clippy, e2e-tests, e2e-runner; docs/testing.md,
# Safety interlock).
#
# Cargo metadata resolves the separate E2E project without compiling or
# running its destructive harness.
# The last two lines lint and test the diskgc crate again with the release
# package's catalog feature, the only crate with feature-gated code and
# tests; without them no fast gate saw that code (TASK-148, TASK-176).
#
# Check E2E lockfile freshness, formatting, clippy and workspace tests.
check:
    cargo metadata --locked --manifest-path diskgc-e2e/Cargo.toml --format-version 1 > /dev/null
    cargo fmt --all --check
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo fmt --manifest-path diskgc-e2e/Cargo.toml --check
    python3 -m unittest discover -s tools -p 'test_*.py'
    python3 tools/check_docs.py docs/reference.md docs/safety.md
    cargo test --manifest-path fuzz/Cargo.toml --test seeds --locked
    cargo clippy --workspace --all-targets --locked -- --deny warnings
    cargo test --workspace --locked
    cargo clippy --locked -p diskgc --all-targets --features proposed-release-01-catalog -- --deny warnings
    cargo test --locked -p diskgc --features proposed-release-01-catalog

# The container recipes first run the harness's unit tests (a flake check),
# then the flake's e2e runner. The runner runs nothing
# destructive on the host, reuses the base image when its Nix hash is
# already loaded, starts one rootless Podman container per test and reports
# per-test durations (docs/testing.md, End-to-end tests in containers). The
# budget is measured by the runner and is also the deadline for hung tests.
# Host requirement: rootless Podman (not provided by the dev shell).

# `check`, sandboxed config and release checks, and fast Nix and Docker container tests.
e2e: check _gcroot-dev
    nix build -L --no-link .#checks.x86_64-linux.config-check-sandbox
    nix build -L --no-link .#checks.x86_64-linux.diskgc-static
    nix build -L --no-link .#checks.x86_64-linux.e2e-clippy
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    nix build -L --no-link .#checks.x86_64-linux.release-excludes-e2e
    nix build -L --no-link .#checks.x86_64-linux.release-excludes-clock-offset
    nix build -L --no-link .#checks.x86_64-linux.release-01-package
    nix build -L --no-link .#checks.x86_64-linux.release-gate-needs-catalog
    just _runner e2e-runner run --suite fast --budget-secs 300
    just _runner e2e-runner-release-01 --log-dir tmp/e2e-logs-release run --suite release --budget-secs 300
    just _runner e2e-runner-release-01-debian --log-dir tmp/e2e-logs-release-debian run --suite release --budget-secs 300
    just _e2e-docker fast 300
    nix build -L --no-link .#checks.x86_64-linux.vm-historical-ctime
    nix build -L --no-link .#checks.x86_64-linux.vm-release-tmp-reclaim
    nix build -L --no-link .#checks.x86_64-linux.vm-old-kernel-pidfd
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-1
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-12
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-5-15
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-5-8
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-8

# Every `e2e` stage, not stopping at a failure, for async QA (TASK-199);
# prints and saves a per-stage exit table under tmp/e2e-all/.
e2e-all:
    python3 tools/e2e_all.py e2e

# Cold guest-cache 40k dry plan on the VM's private ext4 disk; no deletion.
e2e-vm-cold-scale: _gcroot-dev
    nix build -L --no-link .#checks.x86_64-linux.vm-cold-scale

# Every container test: Nix, Debian and native Docker; budgets guard hangs.
e2e-full: check _gcroot-dev
    nix build -L --no-link .#checks.x86_64-linux.config-check-sandbox
    nix build -L --no-link .#checks.x86_64-linux.diskgc-static
    nix build -L --no-link .#checks.x86_64-linux.e2e-clippy
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    nix build -L --no-link .#checks.x86_64-linux.release-excludes-e2e
    nix build -L --no-link .#checks.x86_64-linux.release-excludes-clock-offset
    nix build -L --no-link .#checks.x86_64-linux.release-01-package
    nix build -L --no-link .#checks.x86_64-linux.release-gate-needs-catalog
    just _runner e2e-runner run --suite full --budget-secs 3600
    just _runner e2e-runner-release-01 --log-dir tmp/e2e-logs-release run --suite release --budget-secs 300
    just _runner e2e-runner-release-01-debian --log-dir tmp/e2e-logs-release-debian run --suite release --budget-secs 300
    just _e2e-debian
    just _e2e-docker full 900
    nix build -L --no-link .#checks.x86_64-linux.vm-historical-ctime
    nix build -L --no-link .#checks.x86_64-linux.vm-release-tmp-reclaim
    nix build -L --no-link .#checks.x86_64-linux.vm-old-kernel-pidfd
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-1
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-12
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-5-15
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-5-8
    nix build -L --no-link .#checks.x86_64-linux.vm-pidfd-linux-6-8
    just e2e-vm-cold-scale

# The static binary, no Nix inside (docs/testing.md, Images).
# Every test but the Nix-only ones, on the Debian image.
e2e-debian: check _gcroot-dev
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    just _runner e2e-runner-release-01-debian --log-dir tmp/e2e-logs-release-debian run --suite release --budget-secs 300
    just _e2e-debian

# Native Docker Engine tests in a digest-pinned dind image under Podman.
e2e-docker: check _gcroot-dev
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    just _e2e-docker full 900

# No dev shell root here, to stay fast: the gate recipes root it.
# One container test by NAME, keeping the container on failure.
e2e-one NAME:
    just _runner e2e-runner one {{NAME}}

# One container test by NAME on the Debian image, kept on failure.
e2e-debian-one NAME:
    just _runner e2e-runner-debian --log-dir tmp/e2e-logs-debian one {{NAME}}

# One native Docker Engine test in the disposable dind image.
e2e-docker-one NAME:
    just _runner e2e-runner-docker --log-dir tmp/e2e-logs-docker one {{NAME}}

# The Debian image's run, shared by e2e-debian and e2e-full; its own log
# directory, since test names repeat across images.
_e2e-debian:
    just _runner e2e-runner-debian --log-dir tmp/e2e-logs-debian run --suite full --budget-secs 900

# Run the selected native Docker Engine suite with its own logs and deadline.
_e2e-docker SUITE BUDGET:
    just _runner e2e-runner-docker --log-dir tmp/e2e-logs-docker run --suite {{SUITE}} --budget-secs {{BUDGET}}

# Builds runner package NAME to an out-link under .gcroots/ (gitignored, at
# the repo root so `cargo clean` keeps it) and runs it with ARGS. The
# out-link is a GC root for the runner's closure, which holds its image's
# inputs (the toolchains, the pinned Debian and Docker images), so a Nix GC on a low
# disk does not force them to be fetched and built again; each build
# replaces its link, so roots do not accumulate (docs/testing.md, Images).
_runner NAME *ARGS:
    nix build -L -o .gcroots/{{NAME}} .#{{NAME}}
    .gcroots/{{NAME}}/bin/diskgc-e2e-runner {{ARGS}}

# A GC root for the dev shell's inputs, the same way.
_gcroot-dev:
    nix build -L -o .gcroots/dev .#devShells.x86_64-linux.default.inputDerivation

# Then `nix store gc`; Podman's image copies go with `podman rmi`.
# Drops every GC root the container recipes made (.gcroots/).
gcroots-drop:
    rm -rf .gcroots

# Safety and integrated selection properties with many cases.
soak:
    DISKGC_PROPERTY_CASES=1024 cargo test -p diskgc-modelgen --test properties --locked
    DISKGC_PROPERTY_CASES=1024 cargo test -p diskgc-modelgen --test selection_reference --locked
    DISKGC_PROPERTY_CASES=1024 cargo test -p diskgc-modelgen --test selection_general_reference --locked

# Fuzz one parser or all five for DURATION seconds each in the pinned nightly shell.
fuzz TARGET="all" DURATION="1800":
    nix develop .#fuzz -c bash fuzz/run.sh {{TARGET}} {{DURATION}}

# Run the complete five-source mutation sweep in resource-capped container shards.
# The runner stops on resource pressure or any unverified mutant outcome.
mutants:
    git check-ignore -q tmp/system || { echo 'tmp/system must be Git-ignored before cargo-mutants copies the source tree' >&2; exit 2; }
    python3 tools/mutation_scope.py run

# Validate local links and anchors in the reference and safety contracts.
docs-check:
    python3 tools/check_docs.py docs/reference.md docs/safety.md
    cargo test -p diskgc-core --lib --locked

# Run what docs/reference.md documents against diskgc binary BIN and report drift
# (phase3.1 step 5b); read-only on the host, reclaiming probes only with
# `--container NAME` against a walk env's BIN (e.g. /usr/local/bin/diskgc).
docs-vs-binary BIN *ARGS:
    python3 tools/docs_vs_binary.py "{{BIN}}" {{ARGS}}

# What CI runs: every flake check, then every container test.
ci:
    nix flake check -L
    just e2e-full

# Time the in-use process scan on this host, read-only (docs/safety.md,
# Freshness), with the host (gnu) build and the shipped static musl target.
scan-timing SCANS="20" *ARGS="":
    cargo build --release --locked -p diskgc-linux --example scan_timing
    cargo build --release --locked -p diskgc-linux --example scan_timing --target x86_64-unknown-linux-musl
    "$CARGO_TARGET_DIR/release/examples/scan_timing" {{SCANS}} {{ARGS}}
    "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/examples/scan_timing" {{SCANS}} {{ARGS}}

# Walkthrough driver (tools/walk, phase3.1 skill): a disposable rootless
# Podman container from the e2e base image with the 0.1 release package on
# PATH, seeded data on size-limited tmpfs and a host-shaped process load,
# driven through tmux session walk-diskgc-NAME. Reclaiming runs belong there.
# Each NAME gets its own container and uid slot (docs/testing.md).
# Bring up walk environment NAME: its container and tmux session.
walk-up NAME="podman":
    bash tools/walk/up.sh {{NAME}}

# Regenerate walk environment NAME's data in place from WALK_SEED (default 1),
# with WALK_TMP_BULK bulk /tmp leftovers (default 0).
walk-reset NAME="podman":
    podman exec -e WALK_SEED="${WALK_SEED:-1}" -e WALK_TMP_BULK="${WALK_TMP_BULK:-0}" walk-diskgc-{{NAME}} sh /opt/walk/reset.sh

# Copy diskgc binary BIN into walk environment NAME as diskgc-TAG.
walk-install NAME BIN TAG:
    bash tools/walk/install.sh {{NAME}} "{{BIN}}" {{TAG}}

# Remove walk environment NAME's container and tmux session.
walk-down NAME="podman":
    bash tools/walk/down.sh {{NAME}}

# Print the retro table from the verification notes' retro lines.
retro:
    python3 tools/retro.py

# Build the static musl package into ./result.
build:
    nix build -L

# Placeholder for recipes whose tests do not exist yet. It says so plainly
# and runs nothing; it exits 0 so the gates that do exist stay usable.
_not-yet WHAT TASK:
    @echo "NOT RUN: {{WHAT}} - not implemented yet (arrives with {{TASK}}); no tests were run by this step." >&2
