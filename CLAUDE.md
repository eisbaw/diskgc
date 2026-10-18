# diskgc

**Always read `PRD.md` first, in every session, before doing anything in
this repository.** It is the product contract. Its document map says which
file in `docs/` holds each detailed rule; read the relevant one before
changing behaviour it governs (each rule has exactly one home there).

- Written in Rust; built, tested and packaged through the Nix flake (crane,
  rust-overlay pinned by `rust-toolchain.toml`). Use the flake's dev shell
  and the `justfile` recipes; do not add other build systems.
- Tasks are tracked with the `backlog` CLI only (use `--plain` for any
  command that has a TUI); never edit task files by hand.
- No test ever runs diskgc against the developer's live system: destructive
  tests run only in the rootless test containers (`docs/testing.md`). The
  one named exception is the gate test
  `a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate`
  (`crates/diskgc/tests/cli.rs`): a reclaiming `ensure` on the host whose
  roots and PATH are its own temp fixture, expected to be refused by the
  sandbox gate. Do not add another.
- **Never run a reclaiming `diskgc` (`ensure` or `watch` without
  `--dry-run`) on this machine.** Until the release task lifts it, the
  binary itself refuses to delete outside the test containers (sandbox
  gate); do not work around the gate. `check`, `survey`, `profiles` and
  `--dry-run` are safe to run anywhere.
