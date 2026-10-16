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
  tests run only in the rootless test containers (`docs/testing.md`).
