# diskgc developer recipes (docs/testing.md, Commands).
#
# Run every recipe inside the flake's dev shell, which provides the pinned
# toolchain (rust-toolchain.toml) and just itself:
#
#     nix develop -c just <recipe>      # one-off
#     nix develop                       # then: just <recipe>
#
# Recipes only call Nix, Cargo and Podman (docs/architecture.md,
# Implementation). The dev shell builds for the host (gnu) target; the flake's
# package and checks build for static musl.

# List the recipes.
default:
    @just --list

# diskgc-e2e is only format-checked here: compiling and unit-testing it is
# left to the flake (e2e-clippy, e2e-tests, e2e-runner; docs/testing.md,
# Safety interlock).
#
# Fast gate: formatting, clippy (warnings are errors), workspace tests.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path diskgc-e2e/Cargo.toml --check
    cargo clippy --workspace --all-targets --locked -- --deny warnings
    cargo test --workspace --locked

# The container recipes first run the harness's unit tests (a flake check),
# then the flake's e2e runner. The runner runs nothing
# destructive on the host, reuses the base image when its Nix hash is
# already loaded, starts one rootless Podman container per test and reports
# per-test durations (docs/testing.md, End-to-end tests in containers). The
# budget is measured by the runner and is also the deadline for hung tests.
# Host requirement: rootless Podman (not provided by the dev shell).

# Commit gate: `check` plus the fast container tests, failing over 5 minutes.
e2e: check
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    nix run -L .#e2e-runner -- run --suite fast --budget-secs 300

# Every container test, fast and full tiers; the budget only guards hangs.
e2e-full: check
    nix build -L --no-link .#checks.x86_64-linux.e2e-tests
    nix run -L .#e2e-runner -- run --suite full --budget-secs 3600

# One container test by NAME, keeping the container on failure.
e2e-one NAME:
    nix run -L .#e2e-runner -- one {{NAME}}

# Property tests with many cases.
soak:
    @just _not-yet "the soak property suite" task-24

# cargo-fuzz targets, in a nightly dev shell.
fuzz:
    @just _not-yet "the fuzz targets" task-27

# cargo-mutants on the safety-critical modules.
mutants:
    @just _not-yet "mutation testing" task-13

# What CI runs: every flake check, then every container test.
ci:
    nix flake check -L
    just e2e-full

# Build the static musl package into ./result.
build:
    nix build -L

# Placeholder for recipes whose tests do not exist yet. It says so plainly
# and runs nothing; it exits 0 so the gates that do exist stay usable.
_not-yet WHAT TASK:
    @echo "NOT RUN: {{WHAT}} - not implemented yet (arrives with {{TASK}}); no tests were run by this step." >&2
