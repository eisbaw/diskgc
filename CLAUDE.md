# diskgc

**Read `PRD.md` first in every session, before doing anything in this
repository.** It is the product contract. Its document map identifies the
single home for each detailed rule in `docs/`; read the relevant document
before changing behavior it governs.

## Product and release evidence

- Check release claims against the PRD and every open release criterion using
  the actual gate-lifted artifact. A passing gated development build or a
  proposed release candidate does not approve a release or change its scope.
- Before shipping a `/tmp` deletion profile, verify the producer's lifecycle
  and test a live-session counterexample. Age, the absence of an open file
  descriptor or lock, and a successful dry run do not prove a path is unused.
- Record the exact command, exit code, environment, and scope of test evidence.
  A harness that exists has not necessarily run; a passing rerun does not
  resolve an intermittent failure. Preserve raw diagnostics and investigate
  the cause before calling a failure fixed.
- Before resuming an old branch or worktree, compare it with current HEAD and
  the task tracker so committed work is not repeated.

## Development workflow

- Build, test, and package this Rust project through the Nix flake (crane and
  rust-overlay are pinned by `rust-toolchain.toml`). Use its dev shell and the
  `justfile`; do not add another build system. If `shell.nix` is present, use
  it. Keep common commands such as build, lint, test, and run in the Justfile,
  and run Just recipes inside the Nix shell. Do not add verbose `shellHook`
  output.
- Run `nix develop -c just e2e` before every Git commit. A failing E2E gate
  blocks the commit; fix the failure or review and regenerate snapshots when
  the change is intentional.
- Before every commit, run a qa-test-runner and an mped-architect review in
  parallel. Fix their reported blockers.
- Before starting each backlog implementation task, run
  `nix develop -c just scratch-tally` and inspect the size, growth, and free
  space warnings. Run memory-heavy integration jobs one at a time, and check
  available disk and RAM before starting new VM or mutation builds. Do not
  weaken a resource guard merely to make a run start.
- Create task worktrees beneath the main checkout's `tmp/worktrees/`. Keep
  controllable build output, host test temporary files, and E2E logs beneath
  `tmp/`. Treat existing `/tmp/diskgc*` worktrees and root `target/` as legacy
  data; inventory them before migration.
- Do not delete host files without explicit user authorization. Deletion
  inside disposable Docker or Podman containers is allowed; host tallies are
  allowed.

## Test isolation

- Destructive tests do not run diskgc against the developer's live filesystem.
  They run only in rootless test containers (`docs/testing.md`). The sole named
  host exception is
  `a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate`
  (`crates/diskgc/tests/cli.rs`): its roots and PATH are its own temporary
  fixture, and it must be refused by the sandbox gate. Do not add another.
- **Never run a reclaiming `diskgc` (`ensure` or `watch` without `--dry-run`)
  on this machine outside that named gate test.** Development builds refuse
  deletion outside test containers; do not work around the gate. `check`,
  `survey`, `profiles`, and `--dry-run` are safe to run anywhere.

## Engineering standards

- Do not implement workarounds. When something breaks, identify the root
  cause, fix it, test the integrated behavior, and state any remaining
  limitation.
- Do not claim success without testing or present incomplete work as complete.
  Test features end to end as well as at the unit level. Be self-critical
  about assumptions, pitfalls, and limitations; avoid unwarranted confidence
  and excessive emojis.

## Backlog and repository hygiene

- Track tasks with the `backlog` CLI only; never edit backlog Markdown by
  hand. Use `--plain` for commands with an interactive TUI, inspect stdout
  and stderr, and do not run backlog commands in the background. Whenever
  GitLab or the backlog is referenced, run `linux-utils` to learn the
  available tools.
- Do not produce summary files that will go stale. Put durable decisions in
  commit messages or the appropriate documentation. Move stale, irrelevant
  files or docs to `cruft/`, but never move backlog task Markdown there.
- Do not credit Claude Code in commit messages. Do not override the Git user
  name or email; use the repository's existing configuration.

## Electron troubleshooting

- If DrawIO, Chrome, or another Electron app flashes and exits with code 0,
  inspect stale `Singleton*` IPC files in its config directory and the
  referenced socket under `/tmp/`. Remove stale IPC only when authorized by
  the host deletion rule above.
