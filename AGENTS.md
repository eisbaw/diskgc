# Development workflow

- Run `nix develop -c just e2e` before every Git commit. E2E failures block commits; fix failures or review and regenerate intentional snapshots.
- Before every commit, run a qa-test-runner and an mped-architect review in parallel. Fix their reported blockers.
- Keep `build`, `lint`, `test`, `run` and other common commands in the Justfile. Run Just recipes inside the Nix shell. If a `shell.nix` is present, use it.
- Do not add verbose `shellHook` output.
- Before starting each backlog implementation task, run `nix develop -c just scratch-tally` and inspect its size, growth and free-space warning.
- Create new task worktrees beneath the main checkout's `tmp/worktrees/`. Keep controllable build output, host test temporary files and E2E logs beneath `tmp/`.
- Treat existing `/tmp/diskgc*` worktrees and root `target/` as legacy data. Inventory them before migration. Do not delete host files without explicit user authorization. Deletion inside disposable Docker or Podman containers is allowed; host tallies are allowed.

# Engineering standards

- Fix root causes. Do not implement workarounds, claim success without testing, or present incomplete work as complete.
- When something breaks, identify the cause, fix it, test the integrated behavior and state any unresolved limitation clearly.
- Be self-critical about assumptions, pitfalls and limitations. Avoid unwarranted confidence and excessive emojis.
- Test features end to end, not only at unit level.

# Repository hygiene

- Do not produce summary files that will go stale; put durable decisions in commit messages or the appropriate documentation.
- Move stale, irrelevant files or docs to `cruft/`, but never move backlog task Markdown files there.
- Whenever GitLab or the backlog is referenced, run `linux-utils` to learn the available tools.
- For backlog tasks, use the `backlog` CLI only. Do not edit backlog Markdown directly. Inspect CLI stdout and stderr; do not run backlog tasks in the background. Use `--plain` for commands with interactive TUI modes.
- Do not credit Claude Code in commit messages. Do not override Git user name or email; use the repository's existing configuration.

# Electron troubleshooting

- If DrawIO, Chrome or another Electron app flashes and exits with code 0, inspect stale `Singleton*` IPC files in its config directory and the referenced socket under `/tmp/`. Remove stale IPC only when authorized by the host deletion rule above.
