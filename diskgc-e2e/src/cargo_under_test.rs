//! Cargo under test (docs/testing.md, Cargo toolchains under test): the
//! pinned toolchains in the image, the offline fixture projects, Cargo runs
//! as `tester`, the control-file gates that pause them, and the smoke check
//! that runs before every Cargo test.
//!
//! - **Toolchains.** The image holds each toolchain at
//!   `/opt/cargo-under-test/NAME` and the C toolchain (the linker) at
//!   `/opt/cargo-under-test/cc`: in the Nix image symlinks into its
//!   `/nix/store`, in the Debian image into its rustup toolchain and `/usr`
//!   (read-only bind-mounted by `Ctx::setup` as the image's descriptor
//!   says, [`crate::image`], so the deletion oracle's snapshot does not walk
//!   them). The manifest `/opt/cargo-under-test/toolchains` lists `NAME
//!   VERSION TIER` per toolchain; flake.nix (`cargoUnderTest`, and the
//!   Debian image's one line) is the single source of the list and of each
//!   toolchain's tier.
//! - **Fixture projects** ([`SMOKE`], [`RACE`]) are files under
//!   `diskgc-e2e/fixtures/cargo/`, compiled into the harness and written to
//!   a scratch tmpfs per test, owned by tester. They have no dependencies
//!   and build offline (`CARGO_NET_OFFLINE=true` in [`Toolchain::env`], and
//!   the container has no network).
//! - **Cargo runs.** [`Cargo::spawn`] starts Cargo untraced as `tester`: a
//!   fixture process (e.g. a build the lock holder blocks), declared to the
//!   oracle as a concurrent writer where it writes during a judged run.
//! - **Gates** ([`Gate`]) pause a helper on a control file on the scratch
//!   mount: the race fixture's build script ([`BUILD_GATE_ENV`]), its
//!   `#[test]` ([`TEST_GATE_ENV`]: a test binary still running) and the
//!   Cargo runner wrapper ([`RUNNER_GATE_ENV`], `crate::cargo_probe`).
//! - **Smoke** ([`smoke`]): builds and runs [`SMOKE`] with a toolchain. The
//!   suite runs it before every Cargo test; a failure is reported as a
//!   toolchain problem (exit [`crate::exit::TOOLCHAIN`], `TOOLCHAIN` in the
//!   runner), never as a race failure.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::scratch::Scratch;
use crate::suite::{Ctx, Tier};
use crate::under_test::TESTER_ENV;
use crate::{TESTER, info};

/// Where the image puts the toolchains under test.
pub const CARGO_UNDER_TEST_DIR: &str = "/opt/cargo-under-test";

/// The image's list of toolchains: `NAME VERSION TIER` per line, in flake
/// order.
pub const MANIFEST: &str = "/opt/cargo-under-test/toolchains";

/// The C toolchain's `bin` (`cc`, `ld`), shared by every toolchain.
pub const CC_BIN_DIR: &str = "/opt/cargo-under-test/cc/bin";

/// Variable naming the race fixture build script's gate (see
/// `fixtures/cargo/race/build.rs`).
pub const BUILD_GATE_ENV: &str = "DISKGC_E2E_BUILD_GATE";

/// Variable naming the Cargo runner wrapper's gate (`crate::cargo_probe`).
pub const RUNNER_GATE_ENV: &str = "DISKGC_E2E_RUNNER_GATE";

/// Variable naming the gate the race fixture's `#[test]` waits at, so its
/// test binary keeps running (see `fixtures/cargo/race/src/main.rs`).
pub const TEST_GATE_ENV: &str = "DISKGC_E2E_TEST_GATE";

/// Suffix of the file a helper writes when it waits at a gate.
pub const WAITING_SUFFIX: &str = ".waiting";

/// Suffix of the file the harness creates to let it go on.
pub const GO_SUFFIX: &str = ".go";

/// How long a helper waits at a gate before it gives up and fails (the
/// build script has its own copy of this value).
pub const GATE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the harness waits for a helper to reach a gate, or for Cargo
/// to reach a lock.
pub const REACH_TIMEOUT: Duration = Duration::from_secs(120);

/// Scratch directory (relative to the scratch root) holding the gates.
const GATES_DIR: &str = "cargo-gates";

/// Where the smoke check mounts its own tmpfs (baked into the image), so
/// that nothing of it stays visible to a test: it is unmounted afterwards.
const SMOKE_MOUNT: &str = "/srv/cargo-smoke";

/// A Cargo toolchain under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    /// The name in the manifest and in test names (`cargo_smoke@NAME`).
    pub name: String,
    /// rust-overlay's version: `1.90.0`, `1.101.0-nightly-2026-10-16`.
    pub version: String,
    /// The gate that runs this toolchain's Cargo tests.
    pub tier: Tier,
    /// Holds `cargo` and `rustc`.
    pub bin_dir: PathBuf,
    /// Holds `cc`, the linker rustc invokes.
    pub cc_bin_dir: PathBuf,
}

/// Parses the manifest text. Fails on a malformed line, an unknown tier, a
/// duplicate name, an empty list or one without a fast toolchain: the Cargo
/// tests must never silently vanish from either gate.
pub fn parse_manifest(text: &str) -> Result<Vec<Toolchain>, String> {
    let mut toolchains: Vec<Toolchain> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [name, version, tier] = fields.as_slice() else {
            return Err(format!(
                "{MANIFEST}: expected `NAME VERSION TIER`, got {line:?}"
            ));
        };
        let tier = Tier::parse(tier)
            .ok_or_else(|| format!("{MANIFEST}: tier must be fast or full in {line:?}"))?;
        let valid = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        };
        if !valid(name) || !valid(version) {
            return Err(format!("{MANIFEST}: unexpected characters in {line:?}"));
        }
        if toolchains.iter().any(|t| t.name == *name) {
            return Err(format!("{MANIFEST}: toolchain {name} listed twice"));
        }
        toolchains.push(Toolchain {
            name: (*name).to_owned(),
            version: (*version).to_owned(),
            tier,
            bin_dir: Path::new(CARGO_UNDER_TEST_DIR).join(name).join("bin"),
            cc_bin_dir: PathBuf::from(CC_BIN_DIR),
        });
    }
    if toolchains.is_empty() {
        return Err(format!("{MANIFEST} lists no toolchain"));
    }
    if !toolchains.iter().any(|t| t.tier == Tier::Fast) {
        return Err(format!(
            "{MANIFEST} lists no fast toolchain: `just e2e` would run no Cargo test"
        ));
    }
    Ok(toolchains)
}

impl Toolchain {
    /// Every toolchain in the image, in manifest order.
    pub fn all() -> Result<Vec<Self>, String> {
        let text = std::fs::read_to_string(MANIFEST).map_err(|e| {
            format!("read {MANIFEST} (the Cargo toolchains under test, flake.nix): {e}")
        })?;
        parse_manifest(&text)
    }

    /// The first toolchain of the fast tier.
    pub fn fast() -> Result<Self, String> {
        Self::all()?
            .into_iter()
            .find(|t| t.tier == Tier::Fast)
            .ok_or_else(|| format!("no fast Cargo toolchain under test in {MANIFEST}"))
    }

    /// The oldest stable toolchain in the manifest (the lowest release of
    /// those that are no nightly): the one whose lock set the protocol was
    /// first measured on, `.cargo-lock` alone for 1.90.0.
    pub fn oldest() -> Result<Self, String> {
        oldest_stable(Self::all()?)
            .ok_or_else(|| format!("no stable Cargo toolchain under test in {MANIFEST}"))
    }

    /// The toolchain called `name`.
    pub fn named(name: &str) -> Result<Self, String> {
        Self::all()?
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| format!("no Cargo toolchain under test named {name:?} in {MANIFEST}"))
    }

    pub fn cargo(&self) -> PathBuf {
        self.bin_dir.join("cargo")
    }

    pub fn rustc(&self) -> PathBuf {
        self.bin_dir.join("rustc")
    }

    /// The release `cargo --version` and `rustc --version` print: the
    /// version without rust-overlay's nightly date suffix.
    pub fn release(&self) -> &str {
        release_of(&self.version)
    }

    /// The environment a Cargo under test runs with, added to
    /// [`TESTER_ENV`]: the toolchain and the C toolchain first on `PATH`
    /// (rustc runs `cc` from `PATH`), offline, plain output.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "PATH".to_owned(),
                format!(
                    "{}:{}:/bin",
                    self.bin_dir.display(),
                    self.cc_bin_dir.display()
                ),
            ),
            ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
            ("CARGO_TERM_COLOR".to_owned(), "never".to_owned()),
            ("CARGO_TERM_PROGRESS_WHEN".to_owned(), "never".to_owned()),
        ]
    }
}

/// The stable toolchain (no `-nightly` version) with the lowest release,
/// compared number by number.
pub fn oldest_stable(toolchains: Vec<Toolchain>) -> Option<Toolchain> {
    let numbers = |t: &Toolchain| -> Vec<u64> {
        t.version
            .split('.')
            .map(|n| n.parse().unwrap_or(u64::MAX))
            .collect()
    };
    toolchains
        .into_iter()
        .filter(|t| !t.version.contains("-nightly"))
        .min_by_key(numbers)
}

/// `1.101.0-nightly-2026-10-16` -> `1.101.0-nightly`; a stable version as is.
pub fn release_of(version: &str) -> &str {
    match version.find("-nightly") {
        Some(at) => &version[..at + "-nightly".len()],
        None => version,
    }
}

/// An offline fixture project: its files (relative path, content) and the
/// line its binary prints.
#[derive(Debug, Clone, Copy)]
pub struct Project {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    /// The binary (= package) name.
    pub binary: &'static str,
    /// What the binary prints when it runs.
    pub output: &'static str,
}

/// The least a toolchain must build, link and run.
pub const SMOKE: Project = Project {
    name: "smoke",
    files: &[
        (
            "Cargo.toml",
            include_str!("../fixtures/cargo/smoke/Cargo.toml"),
        ),
        (
            "src/main.rs",
            include_str!("../fixtures/cargo/smoke/src/main.rs"),
        ),
    ],
    binary: "diskgc-e2e-smoke",
    output: "diskgc-e2e cargo smoke ran",
};

/// The race fixture: a binary with a test and a build script that waits at
/// the gate named by [`BUILD_GATE_ENV`] when it is set.
pub const RACE: Project = Project {
    name: "race",
    files: &[
        (
            "Cargo.toml",
            include_str!("../fixtures/cargo/race/Cargo.toml"),
        ),
        ("build.rs", include_str!("../fixtures/cargo/race/build.rs")),
        (
            "src/main.rs",
            include_str!("../fixtures/cargo/race/src/main.rs"),
        ),
    ],
    binary: "diskgc-e2e-race",
    output: "diskgc-e2e cargo race fixture ran",
};

impl Project {
    /// Writes the project to the scratch directory `rel` (created; its
    /// parent must exist), everything owned by tester; returns its path.
    pub fn write(&self, scratch: &Scratch, rel: &str) -> Result<PathBuf, String> {
        let root = scratch.mkdir(rel, 0o755, TESTER)?;
        for (file, content) in self.files {
            let path = format!("{rel}/{file}");
            if let Some((dir, _)) = path.rsplit_once('/')
                && !scratch.path(dir).exists()
            {
                scratch.mkdirs(dir, 0o755, TESTER)?;
            }
            scratch.write_file(&path, content.as_bytes(), 0o644, TESTER)?;
        }
        Ok(root)
    }

    /// `<project>/target/debug/<binary>`.
    pub fn debug_binary(&self, project: &Path) -> PathBuf {
        project.join("target/debug").join(self.binary)
    }
}

/// The `.cargo-*lock*` files in a profile directory, sorted (the same name
/// pattern as the product's structural recogniser, known names or not).
pub fn lock_files(profile_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(profile_dir)
        .map_err(|e| format!("read_dir {}: {e}", profile_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read_dir {}: {e}", profile_dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".cargo-") && name.contains("lock") {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// A Cargo command of one toolchain in one project directory.
#[derive(Debug, Clone)]
pub struct Cargo {
    toolchain: Toolchain,
    cwd: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl Cargo {
    /// `cargo ARGS` run from `cwd` (the project directory, as a user would:
    /// diskgc's process heuristics look at Cargo's working directory).
    pub fn new(toolchain: &Toolchain, cwd: &Path, args: &[&str]) -> Self {
        Self {
            toolchain: toolchain.clone(),
            cwd: cwd.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: toolchain.env(),
        }
    }

    /// Adds `key=value` to its environment (overriding an earlier value).
    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }

    /// Pauses the race fixture's build script at `gate`.
    pub fn build_gate(self, gate: &Gate) -> Self {
        let path = gate.path().display().to_string();
        self.env(BUILD_GATE_ENV, path)
    }

    /// Keeps the race fixture's test binary running at `gate` (`cargo
    /// test`): a test binary still running.
    pub fn test_gate(self, gate: &Gate) -> Self {
        let path = gate.path().display().to_string();
        self.env(TEST_GATE_ENV, path)
    }

    /// Runs binaries (`cargo run`, `cargo test`) through the runner wrapper,
    /// which waits at `gate` before it execs them.
    pub fn runner_gate(self, ctx: &Ctx, gate: &Gate) -> Self {
        let runner = format!("{} probe cargo-runner", ctx.self_exe.display());
        let path = gate.path().display().to_string();
        self.env(crate::cargo_probe::RUNNER_ENV, runner)
            .env(RUNNER_GATE_ENV, path)
    }

    fn what(&self) -> String {
        format!(
            "cargo {} ({}) in {}",
            self.args.join(" "),
            self.toolchain.name,
            self.cwd.display()
        )
    }

    /// Starts it untraced as tester: a fixture process, not code under test.
    pub fn spawn(&self, ctx: &Ctx) -> Result<CargoChild, String> {
        let what = self.what();
        let mut child = Command::new(&ctx.self_exe)
            .args(["as-tester", "--"])
            .arg(self.toolchain.cargo())
            .args(&self.args)
            .env_clear()
            .envs(TESTER_ENV.iter().copied())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {what}: {e}"))?;
        let stdout = read_all(child.stdout.take().ok_or("cargo stdout")?);
        let stderr = read_all(child.stderr.take().ok_or("cargo stderr")?);
        info!("started {what}: pid {}", child.id());
        Ok(CargoChild {
            child,
            stdout: Some(stdout),
            stderr: Some(stderr),
            what,
        })
    }

    /// Runs it untraced as tester to the end.
    pub fn output(&self, ctx: &Ctx) -> Result<CargoOutput, String> {
        self.spawn(ctx)?.wait()
    }
}

fn read_all(mut from: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut all = Vec::new();
        if let Err(e) = from.read_to_end(&mut all) {
            all.extend_from_slice(format!("\n[harness: reading failed: {e}]\n").as_bytes());
        }
        String::from_utf8_lossy(&all).into_owned()
    })
}

/// A Cargo started by [`Cargo::spawn`]. Dropped while running, it is killed.
pub struct CargoChild {
    child: Child,
    stdout: Option<JoinHandle<String>>,
    stderr: Option<JoinHandle<String>>,
    what: String,
}

/// How a Cargo run ended.
#[derive(Debug, Clone)]
pub struct CargoOutput {
    pub what: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CargoOutput {
    /// Fails unless Cargo exited 0.
    pub fn expect_success(&self) -> Result<&Self, String> {
        if self.code == Some(0) {
            Ok(self)
        } else {
            Err(self.failure())
        }
    }

    /// A message with the exit code and both outputs.
    pub fn failure(&self) -> String {
        format!(
            "{} exited with {:?}\n--- stdout\n{}--- stderr\n{}",
            self.what, self.code, self.stdout, self.stderr
        )
    }
}

impl CargoChild {
    /// Cargo's pid (`as-tester` execs Cargo in the spawned process).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether Cargo is still running.
    pub fn is_running(&mut self) -> Result<bool, String> {
        self.child
            .try_wait()
            .map(|s| s.is_none())
            .map_err(|e| format!("wait for {}: {e}", self.what))
    }

    /// Waits for Cargo to exit and collects its output.
    pub fn wait(mut self) -> Result<CargoOutput, String> {
        let status = self
            .child
            .wait()
            .map_err(|e| format!("wait for {}: {e}", self.what))?;
        let join = |h: Option<JoinHandle<String>>| {
            h.map(|h| h.join().unwrap_or_else(|_| "[reader panicked]".to_owned()))
                .unwrap_or_default()
        };
        let output = CargoOutput {
            what: self.what.clone(),
            code: status.code(),
            stdout: join(self.stdout.take()),
            stderr: join(self.stderr.take()),
        };
        info!("{} exited with {:?}", output.what, output.code);
        Ok(output)
    }
}

impl Drop for CargoChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A control-file gate on the scratch mount (the tester-run helpers must
/// see it). Protocol, shared by the race fixture's build script and the
/// runner wrapper ([`wait_at_gate`]): the helper writes its pid and a
/// newline to `GATE.waiting`, then waits until `GATE.go` exists (at most
/// [`GATE_TIMEOUT`], then it fails); the harness sees the waiting file
/// ([`Gate::wait_until_waiting`]), checks what it must while the helper
/// waits, and creates the go file ([`Gate::open`]).
#[derive(Debug, Clone)]
pub struct Gate {
    rel: String,
    path: PathBuf,
}

impl Gate {
    /// A new gate `name` in the scratch gates directory (made on first use,
    /// tester-owned). The name must be unique in the test.
    pub fn new(scratch: &Scratch, name: &str) -> Result<Self, String> {
        if !scratch.path(GATES_DIR).exists() {
            scratch.mkdir(GATES_DIR, 0o755, TESTER)?;
        }
        let rel = format!("{GATES_DIR}/{name}");
        let path = scratch.path(&rel);
        if suffixed(&path, WAITING_SUFFIX).exists() || suffixed(&path, GO_SUFFIX).exists() {
            return Err(format!("gate {} was used before", path.display()));
        }
        Ok(Self { rel, path })
    }

    /// The gate's path (what the helpers' variables name).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Waits until a helper waits at the gate; returns its pid.
    pub fn wait_until_waiting(&self, timeout: Duration) -> Result<u32, String> {
        let waiting = suffixed(&self.path, WAITING_SUFFIX);
        let deadline = Instant::now() + timeout;
        loop {
            // Complete once its line ends with a newline.
            if let Ok(text) = std::fs::read_to_string(&waiting)
                && let Some(pid) = text.strip_suffix('\n')
            {
                let pid = pid
                    .parse()
                    .map_err(|_| format!("{}: not a pid: {text:?}", waiting.display()))?;
                info!("a helper (pid {pid}) waits at gate {}", self.path.display());
                return Ok(pid);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no helper reached gate {} within {timeout:?}",
                    self.path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Lets the helper go on.
    pub fn open(&self, scratch: &Scratch) -> Result<(), String> {
        scratch.write_file(&format!("{}{GO_SUFFIX}", self.rel), b"", 0o644, TESTER)?;
        info!("opened gate {}", self.path.display());
        Ok(())
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The helper's side of the gate protocol (see [`Gate`]).
pub fn wait_at_gate(gate: &Path) -> Result<(), String> {
    let waiting = suffixed(gate, WAITING_SUFFIX);
    let go = suffixed(gate, GO_SUFFIX);
    std::fs::write(&waiting, format!("{}\n", std::process::id()))
        .map_err(|e| format!("write {}: {e}", waiting.display()))?;
    let deadline = Instant::now() + GATE_TIMEOUT;
    while !go.exists() {
        if Instant::now() >= deadline {
            return Err(format!(
                "{} did not appear within {GATE_TIMEOUT:?}",
                go.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The parent pid of `pid`, from `/proc/<pid>/status`.
pub fn parent_of(pid: u32) -> Result<u32, String> {
    let path = format!("/proc/{pid}/status");
    let status = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| format!("no PPid in {path}"))
}

/// A toolchain that failed its smoke check: a toolchain problem, not a
/// race failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainBroken {
    pub toolchain: String,
    /// The smoke step that failed.
    pub step: &'static str,
    pub detail: String,
}

impl std::fmt::Display for ToolchainBroken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cargo toolchain under test {} failed its smoke check at {}: {}",
            self.toolchain, self.step, self.detail
        )
    }
}

/// What a passed smoke check saw.
#[derive(Debug, Clone)]
pub struct SmokeReport {
    pub cargo_version: String,
    pub rustc_version: String,
    pub took: Duration,
}

/// The smoke check: `cargo --version` and `rustc --version` print the
/// manifest's release, and [`SMOKE`] builds offline (as tester, untraced)
/// and its binary runs. It works on a tmpfs of its own that it unmounts
/// afterwards (also `CARGO_HOME`), so it leaves nothing a test could see.
pub fn smoke(ctx: &Ctx, toolchain: &Toolchain) -> Result<SmokeReport, ToolchainBroken> {
    let started = Instant::now();
    let broken = |step: &'static str, detail: String| ToolchainBroken {
        toolchain: toolchain.name.clone(),
        step,
        detail,
    };
    let mount = SmokeMount::mount().map_err(|e| broken("mounting the smoke tmpfs", e))?;
    let project = SMOKE
        .write(&mount.scratch, SMOKE.name)
        .map_err(|e| broken("writing the smoke project", e))?;
    let cargo_home = mount.scratch.path("cargo-home");
    let cargo = |args: &[&str]| {
        Cargo::new(toolchain, &project, args).env("CARGO_HOME", cargo_home.display().to_string())
    };

    let expect_version = |step: &'static str, out: CargoOutput, tool: &str| {
        out.expect_success().map_err(|e| broken(step, e.clone()))?;
        let want = format!("{tool} {} ", toolchain.release());
        let got = out.stdout.trim().to_owned();
        if got.starts_with(&want) {
            Ok(got)
        } else {
            Err(broken(
                step,
                format!(
                    "printed {got:?}, expected it to start with {want:?} (manifest version {})",
                    toolchain.version
                ),
            ))
        }
    };
    let cargo_version = expect_version(
        "cargo --version",
        cargo(&["--version"])
            .output(ctx)
            .map_err(|e| broken("cargo --version", e))?,
        "cargo",
    )?;
    let rustc_out = run_as_tester_untraced(
        ctx,
        &toolchain.rustc(),
        &["--version"],
        &toolchain.env(),
        &project,
    )
    .map_err(|e| broken("rustc --version", e))?;
    let rustc_version = expect_version("rustc --version", rustc_out, "rustc")?;

    cargo(&["build"])
        .output(ctx)
        .map_err(|e| broken("cargo build", e))?
        .expect_success()
        .map_err(|e| broken("cargo build", e))?;
    let ran =
        run_as_tester_untraced::<&str>(ctx, &SMOKE.debug_binary(&project), &[], &[], &project)
            .map_err(|e| broken("running the built binary", e))?;
    ran.expect_success()
        .map_err(|e| broken("running the built binary", e))?;
    if ran.stdout.trim() != SMOKE.output {
        return Err(broken(
            "running the built binary",
            format!("printed {:?}, expected {:?}", ran.stdout, SMOKE.output),
        ));
    }
    drop(mount);
    let report = SmokeReport {
        cargo_version,
        rustc_version,
        took: started.elapsed(),
    };
    info!(
        "toolchain {} passed its smoke check in {:.2?}: {}; {}",
        toolchain.name, report.took, report.cargo_version, report.rustc_version
    );
    Ok(report)
}

/// Runs `program args` untraced as tester from `cwd`, with `env` added to
/// [`TESTER_ENV`]: a fixture process, not code under test.
pub fn run_as_tester_untraced<S: AsRef<OsStr>>(
    ctx: &Ctx,
    program: &Path,
    args: &[S],
    env: &[(String, String)],
    cwd: &Path,
) -> Result<CargoOutput, String> {
    let shown: Vec<String> = args
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    let what = format!("{} {}", program.display(), shown.join(" "));
    let out = Command::new(&ctx.self_exe)
        .args(["as-tester", "--"])
        .arg(program)
        .args(args)
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {what}: {e}"))?;
    Ok(CargoOutput {
        what,
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The smoke check's tmpfs, unmounted (lazily) when dropped.
struct SmokeMount {
    scratch: Scratch,
}

impl SmokeMount {
    fn mount() -> Result<Self, String> {
        Ok(Self {
            scratch: Scratch::mount(SMOKE_MOUNT, "0755", TESTER)?,
        })
    }
}

impl Drop for SmokeMount {
    fn drop(&mut self) {
        if let Err(e) =
            rustix::mount::unmount(self.scratch.root(), rustix::mount::UnmountFlags::DETACH)
        {
            eprintln!(
                "warning: unmounting the smoke tmpfs {}: {e}",
                self.scratch.root().display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_lists_name_and_version_per_line() {
        let parsed = parse_manifest(
            "1.90.0 1.90.0 full\nnightly-2026-10-16 1.101.0-nightly-2026-10-16 fast\n",
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "1.90.0");
        assert_eq!(parsed[0].tier, Tier::Full);
        assert_eq!(parsed[1].tier, Tier::Fast);
        assert_eq!(
            parsed[1].bin_dir,
            PathBuf::from("/opt/cargo-under-test/nightly-2026-10-16/bin")
        );
        assert_eq!(parsed[1].release(), "1.101.0-nightly");
        assert_eq!(parsed[0].release(), "1.90.0");
        assert_eq!(parsed[1].cc_bin_dir, PathBuf::from(CC_BIN_DIR));
    }

    #[test]
    fn a_bad_manifest_is_an_error_not_an_empty_list() {
        assert!(parse_manifest("").is_err());
        assert!(parse_manifest("\n\n").is_err());
        assert!(parse_manifest("1.90.0 1.90.0\n").is_err());
        assert!(parse_manifest("1.90.0 1.90.0 fast extra\n").is_err());
        assert!(parse_manifest("1.90.0 1.90.0 slow\n").is_err());
        assert!(parse_manifest("../x 1.0 fast\n").is_err());
        assert!(parse_manifest("a 1 fast\na 2 full\n").is_err());
        let no_fast = parse_manifest("a 1 full\nb 2 full\n").unwrap_err();
        assert!(no_fast.contains("no fast toolchain"), "{no_fast}");
    }

    #[test]
    fn cargo_runs_offline_with_the_toolchain_and_linker_first_on_path() {
        let tc = &parse_manifest("1.90.0 1.90.0 fast\n").unwrap()[0];
        let env = tc.env();
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("CARGO_NET_OFFLINE"), Some("true"));
        assert_eq!(
            get("PATH"),
            Some("/opt/cargo-under-test/1.90.0/bin:/opt/cargo-under-test/cc/bin:/bin")
        );
    }

    /// Offline needs no registry: no fixture manifest declares a dependency
    /// table (any kind, any target).
    #[test]
    fn fixture_manifests_declare_no_dependencies() {
        for project in [SMOKE, RACE] {
            let (_, manifest) = project
                .files
                .iter()
                .find(|(f, _)| *f == "Cargo.toml")
                .expect("a manifest");
            for line in manifest.lines().map(str::trim) {
                assert!(
                    !(line.starts_with('[') && line.contains("dependencies")),
                    "{}: {line}",
                    project.name
                );
            }
            assert!(manifest.contains(&format!("name = \"{}\"", project.binary)));
        }
    }

    /// The build script keeps its own copy of the gate protocol (it is
    /// compiled by the Cargo under test, without this crate).
    #[test]
    fn the_build_script_speaks_the_gate_protocol() {
        let (_, build) = RACE.files.iter().find(|(f, _)| *f == "build.rs").unwrap();
        for needle in [
            format!("\"{BUILD_GATE_ENV}\""),
            format!("\"{WAITING_SUFFIX}\""),
            format!("\"{GO_SUFFIX}\""),
            format!("from_secs({})", GATE_TIMEOUT.as_secs()),
        ] {
            assert!(build.contains(&needle), "build.rs lacks {needle}");
        }
    }

    /// The fixture's test keeps its own copy of the gate protocol (it is
    /// compiled by the Cargo under test, without this crate).
    #[test]
    fn the_fixture_test_speaks_the_gate_protocol() {
        let (_, main) = RACE
            .files
            .iter()
            .find(|(f, _)| *f == "src/main.rs")
            .unwrap();
        for needle in [
            format!("\"{TEST_GATE_ENV}\""),
            format!("\"{WAITING_SUFFIX}\""),
            format!("\"{GO_SUFFIX}\""),
            format!("from_secs({})", GATE_TIMEOUT.as_secs()),
        ] {
            assert!(main.contains(&needle), "main.rs lacks {needle}");
        }
    }

    #[test]
    fn the_oldest_stable_is_the_lowest_release_not_the_first_listed() {
        let listed = parse_manifest(
            "1.99.1 1.99.1 fast\n1.100.0 1.100.0 full\nnightly-2026-10-16 \
             1.89.0-nightly-2026-10-16 full\n1.90.0 1.90.0 full\n",
        )
        .unwrap();
        assert_eq!(oldest_stable(listed).unwrap().name, "1.90.0");
        let only_nightly =
            parse_manifest("nightly-2026-10-16 1.101.0-nightly-2026-10-16 fast\n").unwrap();
        assert!(oldest_stable(only_nightly).is_none());
    }

    #[test]
    fn nightly_dates_are_not_part_of_the_release() {
        assert_eq!(release_of("1.96.0-nightly-2026-03-27"), "1.96.0-nightly");
        assert_eq!(release_of("1.99.1"), "1.99.1");
    }
}
