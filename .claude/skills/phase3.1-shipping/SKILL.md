---
name: phase3.1-shipping
description: >-
  Ship the last 20% of a project: user value, UX and features, walked
  through the real CLI/TUI/GUI the way a user sees it (tmux, zellij,
  browser), against realistic data confined in reproducible containers,
  VMs or Nix environments. Highest-value-first task selection, small
  MPED-style modules composed together, commits never blocked by QA:
  QA runs asynchronously against pinned commits in its own worktree (suite
  QA plus walk-and-fix QA that commits atomic fixes with regression
  checks), a persistent walkthrough driver keeps walks cheap, a full
  codebase review and a retro run every 10 tasks, and docs are checked
  against the release binary before each release. Self-improving: it
  copies itself into the project on first use, and each retro writes
  measured experiments and gotchas into its project-local calibration
  section (never weakening its invariants). Move fast, walk it, test
  async. Use when the user asks to "ship it", "do the last 20%", "make it
  production ready / usable", "focus on UX/features/user value", "move
  fast", "phase 3.1", or wants velocity over hardening. Sibling of
  phase3-backlog-ralph (which hardens); this one ships.
---

# phase3.1-shipping: ship the last 20%

Project-local copy for diskgc, copied 2026-10-24 from the global skill
`~/.claude-nv/skills/phase3.1-shipping/SKILL.md`.

`phase3-backlog-ralph` makes software correct. This variant makes it
**usable and shipped**. The base is assumed sound (core built, safety
proven where it must be). What remains is the last 20% that decides
whether a user gets value: features, UX, install, defaults, messages,
docs that match the binary, and real use on realistic data.

Move fast and break things, but break them where they can be seen:
every cycle ends with a **walkthrough** of what the user actually sees,
not with a test suite.

Opting into this skill is the user's explicit instruction that commits
are NOT blocked by QA, reviews or full end-to-end suites, overriding any
"run X before every commit" rule in CLAUDE.md for this project while the
skill is active. Project safety rules that protect the user's machine or
data (e.g. "never run a destructive command on the host") are NOT
relaxed. Say so once when starting.

## Start: make the skill project-local

This skill adapts to each project. On the first invocation in a project:

1. If `<repo>/.claude/skills/phase3.1-shipping/SKILL.md` does not exist,
   copy this file there, add a line under its title naming the global
   source and the date copied, and commit it
   (`phase3.1: adopt project-local shipping skill`). Claude Code prefers
   the project-scoped skill of the same name from then on.
2. If it exists, you are reading it: follow it, including its
   **Project calibration** section, which overrides the generic defaults
   above it where they conflict (never the Invariants).
3. If the repository must not carry agent files (e.g. a public repo the
   user wants clean), keep the copy outside git in the project's
   agent memory dir instead, and say so.

## Invariants (no retro may weaken these)

- Honest reporting: no claim of a walk, suite or fix that did not happen;
  failures are recorded as faithfully as passes.
- The user's live machine and data are never put at risk; destructive
  actions run only in disposable environments; the project's own safety
  rules (CLAUDE.md) stand.
- A red fast check blocks a merge.
- Safety-critical protocol changes (deletion, data integrity, migrations,
  auth) get one review round before merge.
- Anything leaving the machine (push, publish, issues) needs the user.
- The self-improvement rules below.

## Doctrine

1. **Value first.** Pick the task that gives a real user the most value
   per unit of effort now. A working feature a user can touch beats an
   edge case nobody hits. Hardening tasks wait unless a walkthrough shows
   the user would hit the problem.
2. **See what the user sees.** Every task is accepted by a walkthrough:
   drive the real CLI/TUI/GUI and read its output as a user would. The
   walkthrough is the acceptance signal, not a test count.
3. **Realistic but confined.** Walk with realistic data (realistic sizes,
   names, process loads, configs, mess) inside a disposable, reproducible
   environment: Nix dev shell or `nix build` artifact, Podman/Docker
   container, NixOS VM test, or a scratch directory. Never the user's
   live data when the action could change it. Realism matters: the bugs
   that block shipping (slow scans on a busy host, confusing output on a
   full disk, a default nobody understands) only show on realistic data.
4. **Sound architecture at speed.** Fast does not mean tangled. Follow
   MPED (`mped-architect` principles): small modules with one job,
   composed; data first; explicit interfaces; fail fast with clear
   messages; no speculative abstraction. A shortcut is fine if it is
   local and filed; a shortcut that leaks across module boundaries is
   not.
5. **Async quality.** Implementers may write unit tests where they help
   them move. QA never blocks a commit: it tests pinned commits in its
   own worktree while implementation continues. Its findings become
   tasks, prioritised by user impact like everything else.
6. **Honest, always.** Fast does not license fake claims. "Walked and
   works" means a walkthrough was actually run and its output read.
   Report what was not checked.

## Roles

- **Orchestrator** (main thread): ranks tasks by value, briefs
  implementers, runs or delegates walkthroughs, merges, schedules async
  QA and the 10-task review, keeps the tracker and memory. Decisions the
  user delegated go to `mark-emulator` (if the user said so); decisions
  that need the user's own authority (destructive host actions, anything
  leaving the machine) go to the user.
- **Implementers** (`mped-architect` agent type, write mode): one per
  task, each in its own git worktree under the project's worktree dir.
  Several may run in parallel when their tasks touch different modules;
  the orchestrator merges one at a time (rebase, fast check, merge).
- **Walker**: the implementer walks its own feature before reporting; the
  orchestrator (or a fresh subagent) re-walks the merged result on
  master when the feature is user-facing and significant. Walkers use
  `xterm-tmux-control` / `zellij-control` for terminals and a browser
  driver (Chrome DevTools MCP, Playwright in a container) for web UIs,
  and capture what they saw (pane dumps, screenshots) as evidence.
- **Async QA** (`qa-test-runner`, read-only on source): runs the project's
  test/lint/e2e suites against a pinned commit in a detached worktree,
  never the live checkout. Reports findings; files tasks; writes the
  verification note on that commit.
- **Walk-and-fix QA** (`mped-architect`, write mode, own worktree): walks
  the user journeys on a pinned commit, and for small user-visible bugs
  commits an atomic fix plus a regression check on its own branch for
  the orchestrator to merge (step 4b).
- **Periodic reviewer** (`mped-architect` read-only, plus a cross-model
  reviewer on data-integrity surfaces): a full codebase review every 10
  merged tasks.

## The loop

### 0. Rank by value
- List unblocked tasks. For each, ask: what can a user do after this that
  they could not before, how many users or how often, and how much effort?
  Prefer: broken or missing core user journeys > confusing UX on common
  paths > missing features users ask for > install/upgrade/packaging >
  docs that disagree with the binary > polish > hardening.
- If the backlog lacks user-facing work, generate it: run a walkthrough
  of the main user journeys (PRD "users / actors"), and file every kink
  you see as a task with the pane dump attached. This is how the last 20%
  is found.
- Batch tasks that touch independent modules into parallel implementers.

### 1. Brief the implementer
Include: the user story and the walkthrough that will accept it (exact
commands or UI steps, the environment, what the user should see); the
closest existing module to extend; MPED constraints (small module, clear
interface, compose rather than extend a god-object); the project's FAST
check command; the rule that commits are not blocked by QA but must not
break the build; explicit "work in your own worktree, merge is the
orchestrator's job" (never share the main checkout with another writer);
tracker conventions; the "file it, don't fix it" rule for anything
outside the task's value slice.

### 2. Implement, then walk
- Implement in small composed modules. Unit tests where they speed the
  implementer up or pin a tricky pure function; no mutation sweeps, no
  exhaustive edge suites, no load series.
- Run the project's fast check (build + lint + unit tests). A red fast
  check is the only thing that blocks a commit.
- **Walk it**: build the real artifact, start the realistic confined
  environment, drive the user journey, read the output. Capture the pane
  dump or screenshot. If the user would be confused, fix it now if small,
  file it if not.
- Report: what the user can now do, the walkthrough transcript (trimmed),
  commits, what was not walked.

### 3. Merge
The orchestrator rebases the branch on master, runs the fast check, and
merges. No review gate. A quick orchestrator sanity pass over the diff
shape (does it match the claim; does it respect module boundaries) is
enough. Safety-critical protocol changes (deletion, data integrity,
migrations, auth) are the exception: they still get one review round
before merge, with blockers limited to correctness and safety.

### 4. Async QA (never blocks)
- Every few merges, or when a risky change lands, start `qa-test-runner`
  on the current master sha in a detached worktree
  (`git worktree add --detach <dir> <sha>`). Implementation continues.
- QA runs the full suites the project has (including slow e2e) on that
  sha, records a verification note on that sha (pass or fail, exit code,
  what was skipped), and returns findings.
- Findings become tasks ranked by user impact. A regression a user would
  hit jumps to the top; a test-only failure ranks like any other task.
- Never rerun a red suite until green and call it fixed; find the cause.

### 4b. Walk-and-fix QA (async)
Alongside suite QA, a walk-and-fix agent (`mped-architect`, write mode)
walks the main user journeys on the pinned sha in its own worktree, using
the persistent driver (below). For each bug a user would hit it:
1. reproduces it in the walk and saves the pane dump or screenshot;
2. writes the smallest regression check that fails on it (a unit test,
   a YAML/model vector, or a scripted walk step);
3. commits an atomic fix with that check on its own branch;
4. re-walks to confirm, then hands the branch to the orchestrator to
   rebase and merge like any implementer branch.
Bugs whose fix is not small or crosses module boundaries are filed as
tasks with the evidence instead. It never edits the main checkout and
never blocks other implementers.

### 5. Every 10 merged tasks: full review + retro
- A read-only `mped-architect` review of the whole codebase (not the diff):
  module boundaries, duplication, leaky shortcuts, docs vs binary, dead
  code. Add a cross-model reviewer (e.g. `codex`) when data-integrity
  code changed in the window.
- Findings are filed and ranked; the next 1-2 cycles fix the ones that
  slow feature work or risk user data. Do not stop shipping to polish.
- **Retro**: a query over the data recorded at each merge (step 6, "Retro
  lines"), never reconstructed from memory. The orchestrator creates one
  tracker task `Retro: <first task>..<last task>`, puts the metrics table
  and the decisions in its final summary, and closes it. That task is the
  record; there is no summary file. It reports:
  - cycle time per task (started to merged) and the slowest three, with why;
  - rework: review rounds, reverted or re-fixed merges;
  - where user-visible bugs were found: walkthrough, walk-and-fix QA,
    async suites, or the user (the user finding one is the worst signal);
  - async QA lag: merged commits with no `qa:` line yet;
  - one or two process changes for the next 10 tasks, applied immediately
    (a durable change also goes to memory or to this skill).

### 5a. After each retro: improve this skill

The retro's process changes are written into the project-local copy of
this skill, so the next cycle runs the improved process. Rules:

- **Edit only the Project calibration section** at the end of the file.
  Never edit the Invariants; edit the generic sections only by moving a
  rule into calibration as an explicit project override.
- **Each entry is an experiment**, with: the date and retro task id; the
  observation and its numbers (e.g. "pytest-heavy tasks: 4.6 h median,
  3 review rounds"); the change (e.g. "skip host dry runs under 20 min
  of runtime; run them at release only"); and the metric that will judge
  it at the next retro.
- **At most three new entries per retro.** At the next retro each entry
  is marked `kept` (metric improved or held), `reverted` (worse; remove
  the rule, keep a one-line record of why), or `extend` (unclear; one more
  window).
- **Counterweight against cutting quality**: any entry that reduces
  testing, review or walking is reverted automatically if the next window
  shows more user-visible bugs found late (`found_by: user` or
  `async-qa`) than the window before. Entries may also ADD rigour where
  bugs escaped: the skill tunes in both directions.
- **Gotchas** (non-obvious facts about the project: slow builds, flaky
  harness parts, resource limits, tools that lie) go into a short
  `Gotchas` list in the same section, each with its date and evidence;
  prune ones that are no longer true.
- Commit the edit with the retro task id
  (`phase3.1: calibrate after Retro TASK-NNN`). Re-read the section before
  the next task.
- **Upstream**: a finding that would help every project (not this one's
  quirks) is proposed to the user as a diff against the global skill;
  never edit the global skill without the user's approval.

### 5b. Before every release: docs match the binary
On the release candidate artifact,
not a dev build: run every documented command and flag (`--help`,
README and reference examples, man page, example configs) in the
confined environment and compare with what the docs say: names, flags,
defaults, exit codes, JSON field names, example output. Fix the docs or
the binary; a mismatch is a release blocker, because it is the first
thing a new user hits.

### 6. Record
Tracker via its CLI only. Each task gets: user story, walkthrough
evidence, commits, follow-ups. Verification notes (`git notes
--ref=verification`) record what actually ran on which sha, including
"walkthrough only" and "QA pending". Durable lessons go to memory and to
the notes of the tasks they affect.

**Retro lines.** When the orchestrator merges a task, it appends fixed
`key: value` lines to that merge commit's verification note, so the retro
is a `git log --notes=verification` query:

    task: TASK-123
    started: 2026-10-24T09:12Z      # when the implementer was briefed
    merged: 2026-10-24T11:40Z
    review_rounds: 0                # 0 unless the safety exception applied
    walked: yes                     # yes | no (say why in the note)
    found_by: walk                  # only on bug fixes: walk | walk-fix | async-qa | review | user

Async QA appends `qa: <pass|fail> <UTC>` to the note of the sha it tested.
A revert or re-fix adds `refix_of: <sha>`. Keep the keys exactly these, so
one `grep`/`awk` over `git log --notes=verification --format=%N` yields
the table; if the project has a justfile, add a `retro` recipe that does
it.

## Persistent walkthrough driver

Walks must be cheap enough to run on every task, so keep the driver
alive instead of cold-starting it (a warm browser or shell answers in
milliseconds; a cold start costs seconds per step):

- **Terminal**: one long-lived tmux (or zellij) session per confined
  environment, named after the project and environment (e.g.
  `walk-<project>-<env>`), with a shell already inside the container/VM
  or Nix shell and the realistic data loaded. Walkers attach, send keys,
  dump the pane, and leave it running. Reset the data from its generator
  rather than rebuilding the environment.
- **Browser**: one long-lived headless browser in a container (Playwright
  or Chromium with a DevTools endpoint), reused across walks; screenshot
  each step. If the browser MCP is unavailable, use the container's own
  Playwright CLI.
- Record the driver's start command in the repo (a `just walk-up` /
  `just walk-down` recipe or equivalent) so any agent can bring it up
  identically. Tear it down at the end of a session, not between tasks.

## Walkthrough recipes

- **CLI**: run inside the confined environment through tmux
  (`xterm-tmux-control`): a fresh shell, realistic `$HOME`, realistic
  data generated or restored from a fixture, then the exact commands a
  user would type, including `--help`, wrong input, and the JSON mode a
  script would use. Dump the pane and read it as the user.
- **TUI**: same via tmux/zellij; capture several frames (startup, typical
  state, error state, resize).
- **Web/GUI**: run the service in a container, drive it with a browser
  (DevTools MCP or Playwright), screenshot each step.
- **Install/upgrade**: walk a clean install from the shipped artifact on
  a clean container or VM image for each supported target; then upgrade
  from the previous release with existing config and data.
- **Realistic data**: generate it reproducibly (a seeded Nix derivation or
  script that builds a tree with realistic sizes, names and ages; a
  process-load container shaped like a real host) rather than copying
  user data. Keep generators in the repo so walks are reproducible.

## Stop conditions
- The main user journeys walk cleanly on realistic data for every
  supported target, install and upgrade walk cleanly, docs match the
  binary (step 5b), and async QA on the release candidate sha is green: ship, and
  recommend the release step to the user (publishing is the user's call).
- Remaining tasks are low value relative to effort: stop and present the
  ranked remainder.
- The same class of user-visible breakage recurs after merges: slow down
  for one cycle, add the missing guard (a walkthrough script in CI, a
  test at the seam), then resume.

## Anti-patterns
- Hardening by stealth: mutation sweeps, exhaustive vectors, multi-round
  reviews on non-safety code.
- Blocking a commit on QA, a review, or a slow suite.
- Declaring a feature done from tests alone without walking it.
- Walking on toy data that hides the real problems (ten files instead of
  ten thousand; an idle container instead of a busy host).
- Walking on the user's live machine where the action could change data.
- Sharing the main checkout between the orchestrator and implementers.
- Big-bang modules: speed is not an excuse for a god-object; compose.
- Fake green: claiming a walkthrough or suite ran when it did not.

## Project calibration (retro-maintained)

Empty in the global skill. In a project-local copy, retros append here
(step 5a). Entries override the generic defaults above for this project;
they never override the Invariants.

### Experiments

<!-- - YYYY-MM-DD, Retro TASK-NNN: observation (numbers) -> change; judged by <metric>. Status: new | kept | extend | reverted (why) -->

### Gotchas

<!-- - YYYY-MM-DD: fact (evidence). -->
- 2026-10-24: `just e2e` takes ~15 min (880 s at fe0ab5f, 27 min on
  5e81db1 incl. image reload); it includes `just check`, container suites
  and 7 kernel VMs. Run it async on a pinned sha, never per commit.
- 2026-10-24: `just check` only format-checks the diskgc-e2e crate; it
  does not compile it. A change to a shared type compiles in `just check`
  and breaks e2e (TASK-127 run 1). When diskgc-e2e or a type it uses
  changes, also build the flake checks `e2e-clippy` and `e2e-tests`.
- 2026-10-24: the user's laptop is the realistic host: ~600-660
  processes, load 5-18, Brave/Electron with many fds. Idle containers
  hide scan-cost bugs (TASK-115: a real reclaim cleared nothing before
  the fix). Walk reclaim paths under a host-shaped process load
  (TASK-115's container: ~281 processes, ~2810 threads, busy loops).
- 2026-10-24: rootless `--pid=host` containers cannot read the host
  user's processes (child user namespace); they do NOT reproduce host
  in-use scans (TASK-115).
- 2026-10-24: the static musl release binary is the shipped artifact and
  is ~1.5x slower per scan than glibc; walk with the release artifact,
  not a dev build.
- 2026-10-24: disk is the binding resource (dropped to 16-17 GiB free
  twice today; Nix store growth from static/glibc/VM builds). Run
  `nix develop -c just scratch-tally` before heavy work; share
  `tmp/target`; never create extra target dirs. A hook blocks `rm`;
  `git worktree remove` of merged clean worktrees is the cleanup path.
- 2026-10-24: retitling a backlog task renames its file; commit with
  `git add -A -- backlog/tasks` rather than by old path.
- 2026-10-24: never run a reclaiming diskgc on the host (CLAUDE.md);
  dev builds refuse anyway (sandbox gate). Read-only `--dry-run`,
  `check`, `survey`, `profiles` on the host are allowed.
