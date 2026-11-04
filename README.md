# diskgc

diskgc keeps enough free space on a Linux filesystem for unattended builds
by deleting rebuildable cache that, as far as it can tell, nothing is using:
build output such as Cargo `target/` directories, the `/tmp` leftovers of
ended test and shell sessions, and tool caches. What a release takes is in
its release notes; the whole library is in
[docs/reference.md](docs/reference.md#built-in-profile-library).

Machines that build software unattended stop when the disk fills, and most
of what fills it is cache that is cheap to rebuild. Unlike age-only
cleanup, diskgc deletes only when free space is short, takes cache in a
configured order, and checks every candidate on the live filesystem just
before deleting it: it keeps any candidate a process it can see has open,
mapped or as its working directory, however old. [PRD.md](PRD.md) says what
it is for, what it guarantees and what it does not.

**Using a release package?** Read [packaging/README.md](packaging/README.md),
which the package ships as `share/doc/diskgc/README.md`.

## Development quick start

Everything builds and runs through the Nix flake and the `justfile`; run
recipes inside the flake's dev shell:

    nix develop -c just check         # formatting, lints, unit and integration tests
    nix develop -c just e2e           # the commit gate: check, flake checks, container and VM tests
    nix build -o tmp/rc .#release-candidate   # the gate-lifted package with this version's release row
    nix develop -c just --list        # every recipe

`just e2e` needs rootless Podman on the host
([docs/testing.md](docs/testing.md#container-setup) has the requirements).
Development builds refuse to delete anything outside diskgc's test
containers ([docs/safety.md](docs/safety.md#pre-release-sandbox-gate)). The
release-candidate package deletes for real; built from any commit but a
release tag it is a development version, for disposable test containers
only. `check`, `profiles` and `ensure --dry-run` are safe to run anywhere.

## Where things are

- [PRD.md](PRD.md): the product contract. Read it first.
- [docs/reference.md](docs/reference.md): commands, exit codes, JSON
  output, configuration, profiles, and the draft release notes.
- [docs/safety.md](docs/safety.md): every check made before anything is
  deleted.
- [docs/architecture.md](docs/architecture.md): index, tally, reclaim
  walk, deployment and the release procedure.
- [docs/testing.md](docs/testing.md): test layers, scenarios and the
  `just` recipes.
- `crates/`: the Rust workspace; `diskgc-e2e/`: the container test
  harness; `packaging/`: what the packages install beside the binary.

## Contributing

Read [PRD.md](PRD.md) and the document it names for the rule you change,
and run `nix develop -c just e2e` before every commit.

## License

MIT; see [LICENSE](LICENSE).
