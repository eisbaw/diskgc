---
id: TASK-1
title: 'Scaffold: Nix flake, Cargo workspace, justfile, static musl build'
status: Done
assignee:
  - '@mped-architect'
created_date: '2026-10-16 22:37'
updated_date: '2026-10-16 23:16'
labels:
  - wave-1
  - foundation
dependencies: []
references:
  - PRD.md
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: everything else builds on this. Create the Nix flake (crane, rust-overlay pinned by rust-toolchain.toml, statically linked musl package), the Cargo workspace with the crates named in docs/architecture.md (diskgc-core, diskgc-linux, diskgc-model, diskgc-modelgen, diskgc; diskgc-e2e as a separate project outside the workspace), and a justfile with the recipes in docs/testing.md (check, e2e, e2e-one, soak, ci) wired to the dev shell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 nix build produces a statically linked diskgc binary (verified with file/ldd) that runs on a non-Nix system image
- [x] #2 just check runs fmt, clippy (warnings are errors) and the (empty) test suites green in the dev shell
- [x] #3 cargo test --workspace does not compile diskgc-e2e
- [x] #4 Editing a file under docs/ does not trigger a rebuild (crane source filtering)
- [x] #5 The dev profile uses debug = "line-tables-only" with dependency debug info off
- [x] #6 The musl target is set only through crane's build target (not .cargo/config.toml), so cargo-fuzz on the gnu target still works
- [x] #7 diskgc-e2e has its own clippy check in the flake, although it is outside the workspace
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. rust-toolchain.toml: pin a current stable from rust-overlay (+ x86_64-unknown-linux-musl target, clippy, rustfmt).
2. Cargo workspace: crates/diskgc-core, diskgc-linux, diskgc-model, diskgc-modelgen (libs, one smoke test each), crates/diskgc (bin: --version / usage, exit 2 on unknown args). exclude = [diskgc-e2e]. [profile.dev] debug=line-tables-only, [profile.dev.package."*"] debug=false.
3. diskgc-e2e/: separate Cargo project with own Cargo.lock, path-depends on diskgc-core, trivial lib.
4. flake.nix: nixpkgs + crane + rust-overlay; craneLib.overrideToolchain(fromRustupToolchainFile); source filter = cargo sources + toml/lock only (docs, backlog, *.md excluded); shared cargoArtifacts (deps-only) for gnu checks; separate musl package with CARGO_BUILD_TARGET + static crt flags set in the derivation only. checks: fmt, clippy -D warnings, nextest/test workspace, e2e clippy (own cargoArtifacts from diskgc-e2e/Cargo.lock), package build. devShell: toolchain, just, cargo-nextest.
5. justfile: check, e2e/e2e-full/e2e-one/soak/fuzz/mutants honest placeholders, ci, build.
6. .gitignore target/, result*.
7. Verify every AC with evidence (file/ldd, podman debian run, cargo test crate list, out-path identity after docs touch, nix flake check -L).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implementation notes (task-1):
- Pins: Rust 1.99.1 (rust-toolchain.toml; latest stable in rust-overlay ec07af4, 2026-10-16), nixpkgs nixos-unstable 55bfb4d (2026-10-14), crane 0.24.0 (needs Nix >= 2.31.2). Bump Rust only in rust-toolchain.toml.
- Flakes only see git-tracked files: new files must be staged (git add) before nix build/check sees them. The scaffold files are staged, not committed.
- Every flake derivation (package, clippy, tests, e2e clippy) builds for x86_64-unknown-linux-musl via CARGO_BUILD_TARGET in the crane args, so they share one deps build (buildDepsOnly) and the flake tests run the shipped target. The dev shell has no target set and builds for gnu. So just check tests gnu and nix flake check tests musl; both run.
- No musl C toolchain is configured. A dependency with C code (cc crate) would need one (pkgsCross.musl64 / pkgsStatic stdenv). Prefer pure-Rust crates.
- Source filtering is an allowlist (lib.fileset): root Cargo.toml + Cargo.lock + commonCargoSources of crates/ (plus diskgc-e2e/ for the e2e checks). docs/, backlog/, *.md, flake.nix and justfile are not build inputs.
- diskgc-e2e: built by crane with postUnpack cd into diskgc-e2e (crane FAQ workspace-not-at-source-root). Its source includes the workspace files because path deps inherit [workspace.package] fields. It has its own Cargo.lock, so update it separately (cargo update --manifest-path diskgc-e2e/Cargo.toml). The flake uses --locked, so a stale lock fails e2e-clippy instead of passing silently. Root [profile.*] settings do not apply to it.
- Extra check diskgc-static: readelf finds no INTERP and no NEEDED, and --version prints the workspace version. The binary is static-pie. crane strips only debug info (-S), so file reports 'not stripped'.
- The justfile placeholders (e2e container part, e2e-full, e2e-one, soak, fuzz, mutants) print 'NOT RUN: ... (arrives with task-N)' to stderr and exit 0. e2e and e2e-full do run check first.
- Only x86_64-linux is supported. Another system needs its musl triple in flake.nix and a gate run on that system.
- Outside the dev shell, this machine's rustup (default nightly) would honour rust-toolchain.toml and download 1.99.1. Inside nix develop, the Nix cargo is first on PATH.

Evidence:
- AC1: file reports 'ELF 64-bit LSB pie executable, x86-64, static-pie linked'. ldd reports 'statically linked'. The runtime closure is only the package itself. podman debian:stable-slim (13.7, no /nix): --version prints 'diskgc 0.1.0' with exit 0, and no arguments gives usage with exit 2.
- AC2: nix develop -c just check exit 0, 4 tests passed (2 unit, 2 CLI integration).
- AC3: after cargo clean, cargo test --workspace compiles exactly diskgc-core, diskgc-linux, diskgc-model, diskgc-modelgen and diskgc. cargo metadata lists those 5 members, and no diskgc-e2e artifacts appear in target/.
- AC4: appending to docs/testing.md and adding a new docs/*.md leaves the .drv and the out path unchanged. Control: editing crates/diskgc-core/src/lib.rs changes the .drv. Both edits were reverted.
- AC5: cargo build -v shows workspace crates built with -C debuginfo=line-tables-only. A non-member path dependency (throwaway, in a scratch copy) gets no debuginfo flag (rustc default 0).
- AC6: no .cargo/ directory exists, and the musl target is set only in flake.nix targetArgs. cargo-fuzz itself was NOT run here (no nightly fuzz shell yet; task-27).
- AC7: checks.e2e-clippy runs cargo clippy --all-targets -D warnings in diskgc-e2e and compiles diskgc-e2e. Bite proof: an injected clippy::eq_op made it fail (exit 1). It passed again after the revert.
- Gate: nix build exit 0; nix develop -c just check exit 0 (4 passed); nix flake check -L exit 0 (checks: diskgc, diskgc-static, workspace-fmt, workspace-clippy, workspace-tests, e2e-fmt, e2e-clippy).

Orchestrator gate: qa-test-runner re-ran nix build (static-pie, statically linked), just check (4 passed / 0 failed), nix flake check (7 checks, all passed), just e2e (check + explicit NOT RUN for the harness), Debian podman run (exit 0). mped-architect: GO; non-blocking findings filed as task-58 and forward-carried to task-2 and task-27.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Scaffolded the Nix flake (crane 0.24.0 + rust-overlay, Rust 1.99.1 pinned by rust-toolchain.toml), a Cargo workspace (diskgc-core, diskgc-linux, diskgc-model, diskgc-modelgen, diskgc), a separate diskgc-e2e project (excluded, own Cargo.lock), the justfile and .gitignore.
- packages.default is a static-pie musl diskgc. It runs on debian:stable-slim: --version exits 0, and no arguments gives usage with exit 2.
- The musl target is set only in the flake derivations (no .cargo/config.toml). The dev shell builds for gnu.
- checks: diskgc, diskgc-static (readelf: no INTERP or NEEDED; --version), workspace-fmt, workspace-clippy (-D warnings), workspace-tests, e2e-fmt, e2e-clippy (bite-proven).
- The source allowlist keeps docs/, backlog/ and *.md out of the build inputs (out path unchanged after a docs edit, verified).
- Dev profile: debug = line-tables-only, dependencies debug = false (verified with rustc flags).
- justfile: check is real. e2e and e2e-full run check, then print NOT RUN for the container part. e2e-one, soak, fuzz and mutants are honest NOT-RUN placeholders that exit 0 and name their task. ci and build are real.
- Gate: nix build exit 0; nix develop -c just check exit 0 (4 tests passed); nix flake check -L exit 0.
- Limits: x86_64-linux only. cargo-fuzz was not exercised (task-27). No musl C toolchain is configured. Non-.rs/.toml fixtures must be added to the source allowlist (forward-carried to task-3/8/14/17). Scaffold files are staged in git (flakes need it) but not committed.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
