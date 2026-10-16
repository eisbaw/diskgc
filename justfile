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

# diskgc-e2e is only format-checked here: compiling it is left to the flake's
# e2e-clippy check and the e2e image (docs/testing.md, Safety interlock).
#
# Fast gate: formatting, clippy (warnings are errors), workspace tests.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path diskgc-e2e/Cargo.toml --check
    cargo clippy --workspace --all-targets --locked -- --deny warnings
    cargo test --workspace --locked

# Commit gate: `check` plus the fast container tests.
e2e: check
    @just _not-yet "the container tests of 'just e2e'" task-2

# Every container test (slow suites, all toolchains, dind, Nix, Debian).
e2e-full: e2e
    @just _not-yet "the container tests of 'just e2e-full'" task-2

# One container test by NAME, keeping the container on failure.
e2e-one NAME:
    @just _not-yet "container test '{{NAME}}'" task-2

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
