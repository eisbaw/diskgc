//! The shared harness of the admission-evidence suites (docs/testing.md,
//! Admission evidence): [`crate::project_tree_suite`] (TASK-350),
//! [`crate::pkg_cache_suite`] (TASK-351) and the suites after them.
//!
//! Every artifact these suites judge is made by a real pinned tool run as
//! tester ([`tool`], [`InFlight`]); the containers have no network, so a
//! tool that fetches is served by a local [`Registry`]. Fixture files and
//! control files (ready, gate and go files, captured output) live below
//! tester's home ([`HOME`], [`CTL`]), outside every candidate.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::{KillOnDrop, wait_for};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// Tester's home, the measured filesystem of the admission scenarios.
pub(crate) const HOME: &str = "/home/tester";
/// Where the harness and the fixture processes exchange files (ready, gate
/// and go files, the registry log, captured output): in the home, outside
/// every candidate.
pub(crate) const CTL: &str = "ctl";
/// The local registry's tree below the home: npm packuments and tarballs
/// (`reg/NAME`), a PEP 503 simple index (`reg/simple/NAME/`) and whatever
/// else a scenario serves.
pub(crate) const REG: &str = "reg";
/// The registry's port in the container's own network namespace.
const PORT: &str = "4873";
pub(crate) const REGISTRY: &str = "http://127.0.0.1:4873";

/// The pinned Node.js, npm and uv of the Nix image (flake.nix: `cacheTools`).
pub(crate) const NODE: &str = "/bin/node";
pub(crate) const NPM: &str = "/bin/npm";
pub(crate) const UV: &str = "/bin/uv";

/// Bound for a tool run, a fixture process or a gate.
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(180);

/// The verdicts a scenario expects of its found candidates: `None`
/// reclaimable, `Some(reason)` kept for that reason. A lookalike that the
/// profile's `find` does not reach is not listed: it must not be planned.
pub(crate) type Expected = Vec<(PathBuf, Option<&'static str>)>;

// ---- tools run as tester ---------------------------------------------------

/// The pinned CPython, as its own `bin/python3`: the bundle's
/// `agent-python3` link resolved once, so that `-m venv` links the
/// environment's interpreter under the names a user's `python3 -m venv`
/// gives it (a venv also links the name it was invoked by).
pub(crate) fn python(ctx: &Ctx) -> Result<PathBuf, String> {
    let link = ctx.bin_dir.join("agent-python3");
    let target =
        std::fs::read_link(&link).map_err(|e| format!("read the link {}: {e}", link.display()))?;
    ensure!(
        target.file_name().is_some_and(|n| n == "python3") && target.is_absolute(),
        "{} links to {}, not an interpreter's absolute bin/python3",
        link.display(),
        target.display()
    );
    Ok(target)
}

pub(crate) fn home(rel: &str) -> PathBuf {
    Path::new(HOME).join(rel)
}

/// Writes the fixture file `rel` below tester's home, creating its parents.
pub(crate) fn write(ctx: &Ctx, rel: &str, content: &[u8]) -> TestResult {
    if let Some((parent, _)) = rel.rsplit_once('/') {
        ctx.home.mkdirs(parent, 0o755, TESTER)?;
    }
    ctx.home.write_file(rel, content, 0o644, TESTER)?;
    Ok(())
}

/// `program args` as tester in `cwd`, with only `PATH`, `HOME`, `LANG` and
/// `env` in its environment.
pub(crate) fn tester_command(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(&str, &str)],
    program: &Path,
    args: &[&str],
) -> Command {
    let mut command = Command::new(&ctx.self_exe);
    command
        .args(["as-tester", "--"])
        .arg(program)
        .args(args)
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", HOME)
        .env("LANG", "C.UTF-8")
        .envs(env.iter().copied())
        .current_dir(cwd);
    command
}

/// Runs a real tool to its end and returns its stdout; fails with its
/// output unless it exits 0.
pub(crate) fn tool(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(&str, &str)],
    program: impl AsRef<Path>,
    args: &[&str],
) -> Result<String, String> {
    let program = program.as_ref();
    let output = tester_command(ctx, cwd, env, program, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("start {}: {e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    info!(
        "{} {args:?} in {}: {}",
        program.display(),
        cwd.display(),
        output.status
    );
    ensure!(
        output.status.success(),
        "{} {args:?} in {} failed: {}\n--- stdout\n{stdout}--- stderr\n{stderr}",
        program.display(),
        cwd.display(),
        output.status
    );
    Ok(stdout)
}

/// Writes the executable fixture script `rel` below tester's home.
pub(crate) fn write_script(ctx: &Ctx, rel: &str, content: &[u8]) -> TestResult {
    if let Some((parent, _)) = rel.rsplit_once('/') {
        ctx.home.mkdirs(parent, 0o755, TESTER)?;
    }
    ctx.home.write_file(rel, content, 0o755, TESTER)?;
    Ok(())
}

/// The control directory, made before any oracle is armed.
pub(crate) fn control_dir(ctx: &Ctx) -> TestResult {
    ctx.home.mkdirs(CTL, 0o777, TESTER)?;
    Ok(())
}

pub(crate) fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Logs each tool's version as it runs in this container and requires the
/// version its profile's `verified_with` names: a bump of the flake's pin
/// fails here until the evidence is redone.
pub(crate) fn versions(ctx: &Ctx, tools: &[(&str, &[&str], &str)]) -> TestResult {
    for (program, args, want) in tools {
        let out = tool(ctx, Path::new(HOME), &[], program, args)?;
        info!("tool version: {program}: {}", out.trim());
        ensure!(
            out.contains(want),
            "{program} reports {:?}, not the verified {want}",
            out.trim()
        );
    }
    Ok(())
}

/// A hand-made lookalike: a cache-shaped tree at a path the profile must
/// not reach. Returns its file.
pub(crate) fn lookalike(ctx: &Ctx, rel: &str) -> Result<PathBuf, String> {
    write(ctx, &format!("{rel}/lookalike.bin"), &payload(4096))?;
    Ok(home(&format!("{rel}/lookalike.bin")))
}

pub(crate) fn expect_kept(paths: &[&Path]) -> TestResult {
    for path in paths {
        ensure!(exists(path), "{} was removed", path.display());
    }
    Ok(())
}

// ---- the local registry ----------------------------------------------------

/// A static file server for the registry tree. A request whose path holds
/// the gate text writes `ctl/gate.hit` and is answered only once
/// `ctl/gate.go` exists: the requesting tool is then in flight, waiting on
/// the network, for as long as the test needs. Every request is logged to
/// `ctl/server.log` (`200 PATH`, `gated PATH`, `404 PATH`). Responses carry
/// HTTP caching headers, as a real index does, so pip caches them.
const SERVER_JS: &str = r#"
const http = require('http'), fs = require('fs'), path = require('path');
const [root, ctl, gate] = process.argv.slice(2);
const log = (m) => fs.appendFileSync(path.join(ctl, 'server.log'), m + '\n');
function send(req, res) {
  const rel = decodeURIComponent(req.url.split('?')[0]);
  let file = path.join(root, rel);
  if (!file.startsWith(root)) { res.writeHead(403); return res.end(); }
  let st;
  try { st = fs.statSync(file); } catch { log('404 ' + rel); res.writeHead(404); return res.end(); }
  if (st.isDirectory()) {
    const index = path.join(file, 'index.json');
    if (fs.existsSync(index)) { file = index; st = fs.statSync(file); }
    else {
      const links = fs.readdirSync(file).sort().map((n) => `<a href="${n}">${n}</a><br>`);
      log('200 ' + rel);
      res.writeHead(200, { 'content-type': 'text/html' });
      return res.end(`<html><body>\n${links.join('\n')}\n</body></html>`);
    }
  }
  const body = fs.readFileSync(file);
  const type = file.endsWith('.json') ? 'application/json' : 'application/octet-stream';
  res.writeHead(200, { 'content-type': type, 'content-length': body.length,
    'cache-control': 'max-age=31536000, public',
    'last-modified': new Date(st.mtimeMs).toUTCString(),
    'etag': '"' + st.size + '-' + Math.floor(st.mtimeMs) + '"' });
  log('200 ' + rel);
  res.end(body);
}
http.createServer((req, res) => {
  if (gate && req.url.includes(gate)) {
    log('gated ' + req.url);
    fs.writeFileSync(path.join(ctl, 'gate.hit'), req.url + '\n');
    const wait = () => fs.existsSync(path.join(ctl, 'gate.go')) ? send(req, res) : setTimeout(wait, 50);
    return wait();
  }
  send(req, res);
}).listen(Number(process.env.PORT), '127.0.0.1', () => fs.writeFileSync(path.join(ctl, 'port'), process.env.PORT));
"#;

/// Packs each package directory `PKGS/NAME` with the real npm (its own
/// cache, so the tarball is not already in the cache under test) and writes
/// its packument (`REG/NAME/index.json`) and tarball (`REG/NAME/-/`).
const MKREG_JS: &str = r#"
const fs = require('fs'), path = require('path'), crypto = require('crypto'), cp = require('child_process');
const [reg, registry, pkgs, ...names] = process.argv.slice(2);
for (const name of names) {
  const dir = path.join(pkgs, name);
  const out = cp.execFileSync('npm', ['pack', '--json', '--pack-destination', dir],
    { cwd: dir, env: { ...process.env, npm_config_cache: path.join(pkgs, '.packcache') } });
  const file = JSON.parse(out)[0].filename;
  const data = fs.readFileSync(path.join(dir, file));
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'package.json')));
  fs.mkdirSync(path.join(reg, name, '-'), { recursive: true });
  fs.copyFileSync(path.join(dir, file), path.join(reg, name, '-', file));
  const dist = { tarball: `${registry}/${name}/-/${file}`,
    shasum: crypto.createHash('sha1').update(data).digest('hex'),
    integrity: 'sha512-' + crypto.createHash('sha512').update(data).digest('base64') };
  fs.writeFileSync(path.join(reg, name, 'index.json'), JSON.stringify({ name,
    'dist-tags': { latest: manifest.version }, versions: { [manifest.version]: { ...manifest, dist } } }));
}
"#;

/// The running registry server (killed when dropped).
pub(crate) struct Registry {
    _server: KillOnDrop,
}

impl Registry {
    /// Starts the server on the registry tree; requests whose path holds
    /// `gate` wait for [`release`].
    pub(crate) fn start(ctx: &Ctx, gate: &str) -> Result<Self, String> {
        write(ctx, &format!("{CTL}/server.js"), SERVER_JS.as_bytes())?;
        ctx.home.mkdirs(REG, 0o755, TESTER)?;
        let child = tester_command(
            ctx,
            Path::new(HOME),
            &[("PORT", PORT)],
            Path::new(NODE),
            &[
                &format!("{HOME}/{CTL}/server.js"),
                &format!("{HOME}/{REG}"),
                &format!("{HOME}/{CTL}"),
                gate,
            ],
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start the registry: {e}"))?;
        let mut server = KillOnDrop(child);
        wait_for(TOOL_TIMEOUT, "the registry's port file", || {
            if let Some(status) = server.0.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("the registry exited: {status}"));
            }
            Ok(exists(&home(&format!("{CTL}/port"))).then_some(()))
        })?;
        Ok(Self { _server: server })
    }

    /// How many times `path` was served in full.
    pub(crate) fn served(&self, path: &str) -> Result<usize, String> {
        let log = std::fs::read_to_string(home(&format!("{CTL}/server.log")))
            .map_err(|e| format!("read the registry log: {e}"))?;
        let line = format!("200 {path}");
        Ok(log.lines().filter(|l| *l == line).count())
    }

    /// Waits until a tool's request reached the gate; returns its path.
    /// The server creates `gate.hit` before it writes the line: a read is
    /// taken only once the line ends.
    pub(crate) fn wait_gated(&self, run: &mut InFlight) -> Result<String, String> {
        let hit = home(&format!("{CTL}/gate.hit"));
        wait_for(TOOL_TIMEOUT, "a request at the registry's gate", || {
            run.still_running()?;
            Ok(std::fs::read_to_string(&hit)
                .ok()
                .filter(|text| text.ends_with('\n')))
        })
        .map(|text| text.trim().to_owned())
    }
}

/// Lets every gated request (and gated fixture) carry on. Written only after
/// the oracle judged the reclaim: it is not part of the run.
pub(crate) fn release(ctx: &Ctx) -> TestResult {
    open_gate(ctx, "gate")
}

/// Writes `ctl/NAME.go`, the go file a fixture paused as `NAME` waits for.
/// Written only after the oracle judged the reclaim: it is not part of the
/// run.
pub(crate) fn open_gate(ctx: &Ctx, name: &str) -> TestResult {
    ctx.home
        .write_file(&format!("{CTL}/{name}.go"), b"go\n", 0o644, TESTER)?;
    Ok(())
}

/// npm packages `NAME@1.0.0` (an `index.js` exporting its name and a
/// 300 kB payload), packed by the real npm into the registry.
pub(crate) fn npm_packages(ctx: &Ctx, names: &[&str]) -> TestResult {
    for name in names {
        write(
            ctx,
            &format!("pkgs/{name}/package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","main":"index.js"}}"#).as_bytes(),
        )?;
        write(
            ctx,
            &format!("pkgs/{name}/index.js"),
            format!("module.exports = '{name}';\n").as_bytes(),
        )?;
        write(ctx, &format!("pkgs/{name}/payload.bin"), &payload(300_000))?;
    }
    write(ctx, &format!("{CTL}/mkreg.js"), MKREG_JS.as_bytes())?;
    let mut args = vec![
        format!("{HOME}/{CTL}/mkreg.js"),
        format!("{HOME}/{REG}"),
        REGISTRY.to_owned(),
        format!("{HOME}/pkgs"),
    ];
    args.extend(names.iter().map(|n| (*n).to_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tool(
        ctx,
        Path::new(HOME),
        &[("npm_config_offline", "true")],
        NODE,
        &args,
    )?;
    Ok(())
}

/// Bytes that do not compress or deduplicate.
pub(crate) fn payload(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

// ---- tool runs in flight ---------------------------------------------------

/// A tool run started as tester whose output goes to `ctl/NAME.out`.
pub(crate) struct InFlight {
    name: String,
    process: KillOnDrop,
}

impl InFlight {
    pub(crate) fn start(
        ctx: &Ctx,
        name: &str,
        cwd: &Path,
        env: &[(&str, &str)],
        program: &str,
        args: &[&str],
    ) -> Result<Self, String> {
        let out = ctx
            .home
            .write_file(&format!("{CTL}/{name}.out"), b"", 0o666, TESTER)?;
        let file = OpenOptions::new()
            .append(true)
            .open(&out)
            .map_err(|e| format!("open {}: {e}", out.display()))?;
        let err = file
            .try_clone()
            .map_err(|e| format!("clone {}: {e}", out.display()))?;
        let child = tester_command(ctx, cwd, env, Path::new(program), args)
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("start {name} ({program}): {e}"))?;
        info!("{name}: started {program} {args:?} in {}", cwd.display());
        Ok(Self {
            name: name.to_owned(),
            process: KillOnDrop(child),
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.process.0.id()
    }

    pub(crate) fn output(&self) -> String {
        std::fs::read_to_string(home(&format!("{CTL}/{}.out", self.name))).unwrap_or_default()
    }

    /// Fails if the run already ended (it should be held in flight).
    pub(crate) fn still_running(&mut self) -> TestResult {
        if let Some(status) = self.process.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "{} ended {status} before it was in flight:\n{}",
                self.name,
                self.output()
            ));
        }
        Ok(())
    }

    /// Waits for the run to end; returns its status and output.
    pub(crate) fn finish(&mut self) -> Result<(ExitStatus, String), String> {
        let status = wait_for(TOOL_TIMEOUT, &format!("{} to end", self.name), || {
            self.process.0.try_wait().map_err(|e| e.to_string())
        })?;
        let output = self.output();
        info!("{}: {status}\n{output}", self.name);
        Ok((status, output))
    }

    pub(crate) fn expect_success(&mut self) -> Result<String, String> {
        let (status, output) = self.finish()?;
        ensure!(
            status.success(),
            "{} ended {status}, expected success:\n{output}",
            self.name
        );
        Ok(output)
    }

    /// The pids of this run's process and its descendants (a wrapper
    /// script or sandbox may run the tool as a child).
    pub(crate) fn process_tree(&self) -> Result<BTreeSet<u32>, String> {
        let mut parents = Vec::new();
        for entry in std::fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))? {
            let entry = entry.map_err(|e| format!("read /proc: {e}"))?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            // `pid (comm) state ppid ...`; comm may hold spaces.
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(ppid) = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|p| p.parse::<u32>().ok())
            else {
                continue;
            };
            parents.push((pid, ppid));
        }
        let mut tree = BTreeSet::from([self.pid()]);
        loop {
            let before = tree.len();
            for (pid, ppid) in &parents {
                if tree.contains(ppid) {
                    tree.insert(*pid);
                }
            }
            if tree.len() == before {
                return Ok(tree);
            }
        }
    }
}

/// Waits until `path` exists while `run` is still in flight.
pub(crate) fn wait_file(run: &mut InFlight, path: &Path) -> TestResult {
    wait_for(TOOL_TIMEOUT, &format!("{}", path.display()), || {
        run.still_running()?;
        Ok(exists(path).then_some(()))
    })
}

// ---- reports ---------------------------------------------------------------

/// The `(candidate, reason)` pairs of a report's plan, sorted.
pub(crate) fn plan_of(report: &Value) -> Result<Vec<(String, Option<String>)>, String> {
    let mut got: Vec<(String, Option<String>)> = report["plan"]
        .as_array()
        .ok_or_else(|| format!("no plan array: {report}"))?
        .iter()
        .map(|entry| {
            (
                entry["candidate"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().map(str::to_owned),
            )
        })
        .collect();
    got.sort();
    Ok(got)
}

pub(crate) fn sorted(expected: &Expected) -> Vec<(String, Option<String>)> {
    let mut want: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|(path, reason)| (path.display().to_string(), reason.map(str::to_owned)))
        .collect();
    want.sort();
    want
}

pub(crate) fn actions(report: &Value) -> Result<BTreeSet<String>, String> {
    Ok(report["actions"]
        .as_array()
        .ok_or_else(|| format!("no actions array: {report}"))?
        .iter()
        .filter_map(|a| a["candidate"].as_str().map(str::to_owned))
        .collect())
}

/// The reclaim report's skipped entry for `candidate`.
pub(crate) fn skipped<'a>(report: &'a Value, candidate: &Path) -> Result<&'a Value, String> {
    report["skipped"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["candidate"].as_str() == candidate.to_str())
        })
        .ok_or_else(|| format!("{} not skipped: {report}", candidate.display()))
}

// ---- what a run in flight holds ---------------------------------------------

/// The files below `dir` that `pid` holds open (from `/proc/PID/fd`).
pub(crate) fn held_in(pid: u32, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let fds = format!("/proc/{pid}/fd");
    let mut held = Vec::new();
    for entry in std::fs::read_dir(&fds).map_err(|e| format!("list {fds}: {e}"))? {
        let link = entry.map_err(|e| e.to_string())?.path();
        if let Ok(target) = std::fs::read_link(&link)
            && target.starts_with(dir)
        {
            held.push(target);
        }
    }
    Ok(held)
}

/// The files below `dir` that `run`'s processes hold open or map now (read
/// from `/proc`, independently of diskgc's report, which names no path).
pub(crate) fn held_by(run: &InFlight, dir: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let mut held = BTreeSet::new();
    for pid in run.process_tree()? {
        // A process may end between the listing and the reads.
        held.extend(held_in(pid, dir).unwrap_or_default());
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
        held.extend(
            maps.lines()
                .filter_map(|line| line.split_whitespace().nth(5))
                .map(PathBuf::from)
                .filter(|path| path.starts_with(dir)),
        );
    }
    Ok(held)
}

/// diskgc kept `candidate` `in_use`, each blocker a process of `run`'s tree
/// with one of the `evidence` kinds (`fd`: an open file; `map`: a mapping),
/// and the run holds a file below it that `holds` (named `what`) accepts.
/// Logs the blockers and the held files.
pub(crate) fn expect_in_use_by(
    report: &Value,
    candidate: &Path,
    run: &InFlight,
    evidence: &[&str],
    what: &str,
    holds: &dyn Fn(&Path) -> bool,
) -> TestResult {
    let held = held_by(run, candidate)?;
    info!("{}: the run holds {held:?}", candidate.display());
    ensure!(
        held.iter().any(|path| holds(path)),
        "the run in flight holds no {what} below {}: {held:?}",
        candidate.display()
    );
    expect_blocked_by(report, candidate, run, evidence)
}

/// The working directories of `run`'s processes that lie below `dir`.
pub(crate) fn cwds_below(run: &InFlight, dir: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let mut cwds = BTreeSet::new();
    for pid in run.process_tree()? {
        // A process may end between the listing and the read.
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd"))
            && cwd.starts_with(dir)
        {
            cwds.insert(cwd);
        }
    }
    Ok(cwds)
}

/// The entries (`NAME=VALUE`) of the initial environments of `run`'s
/// processes whose value, or one of its `:`-separated parts, names a path
/// below `dir`.
pub(crate) fn environ_below(run: &InFlight, dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut named = BTreeSet::new();
    for pid in run.process_tree()? {
        // A process may end between the listing and the read.
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        for entry in environ.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            if let Some((_, value)) = entry.split_once('=')
                && value.split(':').any(|v| Path::new(v).starts_with(dir))
            {
                named.insert(entry.into_owned());
            }
        }
    }
    Ok(named)
}

/// [`expect_in_use_by`] for any use `/proc` shows: a working directory,
/// an open file, a mapping, or an initial-environment entry (`NAME=VALUE`)
/// below `candidate`, at least one of which `uses` accepts. Logs them.
pub(crate) fn expect_used_by(
    report: &Value,
    candidate: &Path,
    run: &InFlight,
    evidence: &[&str],
    uses: &dyn Fn(&str) -> bool,
) -> TestResult {
    let mut seen: BTreeSet<String> = held_by(run, candidate)?
        .iter()
        .chain(&cwds_below(run, candidate)?)
        .map(|p| p.display().to_string())
        .collect();
    seen.extend(environ_below(run, candidate)?);
    info!("{}: the run's processes use {seen:?}", candidate.display());
    ensure!(
        seen.iter().any(|u| uses(u)),
        "the run in flight shows no expected use of {}: {seen:?}",
        candidate.display()
    );
    expect_blocked_by(report, candidate, run, evidence)
}

/// The reclaim acted not on `candidate` but skipped it `in_use`, and every
/// process blocker it names is a process of `run`'s tree with one of the
/// `evidence` kinds.
fn expect_blocked_by(
    report: &Value,
    candidate: &Path,
    run: &InFlight,
    evidence: &[&str],
) -> TestResult {
    ensure!(
        !actions(report)?.contains(&candidate.display().to_string()),
        "diskgc acted on {} although it was in use: {report}",
        candidate.display()
    );
    let entry = skipped(report, candidate)?;
    ensure!(
        entry["reason"] == "in_use",
        "{} skipped as {}, expected in_use: {entry}",
        candidate.display(),
        entry["reason"]
    );
    let tree = run.process_tree()?;
    let blockers: Vec<(u32, String)> = entry["process_blockers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            (
                u32::try_from(b["pid"].as_u64().unwrap_or(0)).unwrap_or(0),
                b["evidence"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    info!(
        "{}: in use by {blockers:?} (the run's processes: {tree:?})",
        candidate.display()
    );
    ensure!(
        !blockers.is_empty()
            && blockers
                .iter()
                .all(|(pid, kind)| tree.contains(pid) && evidence.contains(&kind.as_str())),
        "{} blockers {blockers:?}, expected {evidence:?} of the run's processes {tree:?}: {entry}",
        candidate.display()
    );
    Ok(())
}

// ---- inspection helpers ----------------------------------------------------

/// Regular files below `root` (not following symlinks) whose name
/// satisfies `keep`; 0 when `root` is gone.
pub(crate) fn count_files(root: &Path, keep: &dyn Fn(&str) -> bool) -> Result<usize, String> {
    if !exists(root) {
        return Ok(0);
    }
    let mut count = 0;
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.file_name().to_str().is_some_and(keep) {
                count += 1;
            }
        }
    }
    Ok(count)
}

pub(crate) fn any_file(_: &str) -> bool {
    true
}

// ---- diskgc runs of plain-delete profiles ----------------------------------

/// `ensure [--dry-run] --json --no-index --min-free 100% --select SELECT
/// [--roots ROOTS] PATH`: asked for every byte, so a reclaim takes every
/// eligible candidate (and exits 1).
pub(crate) fn ensure_args<'a>(
    dry_run: bool,
    select: &'a str,
    roots: Option<&'a str>,
    path: &'a str,
) -> Vec<&'a str> {
    let mut args = vec!["ensure"];
    if dry_run {
        args.push("--dry-run");
    }
    args.extend([
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        select,
    ]);
    if let Some(roots) = roots {
        args.extend(["--roots", roots]);
    }
    args.push(path);
    args
}

/// `binary ensure ...` as tester under strace, logged.
pub(crate) fn run_ensure(
    ctx: &Ctx,
    binary: &Path,
    dry_run: bool,
    select: &str,
    roots: Option<&str>,
    path: &str,
) -> Result<Run, String> {
    let args = ensure_args(dry_run, select, roots, path);
    let run = ctx.run_as_tester(binary, &args)?;
    info!(
        "{} {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
        binary.display(),
        args.join(" "),
        run.code,
        run.stdout,
        run.stderr
    );
    Ok(run)
}

pub(crate) fn reclaimable(expected: &Expected) -> BTreeSet<String> {
    expected
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .map(|(path, _)| path.display().to_string())
        .collect()
}

/// A dry run, then a reclaim, of `select` with `binary`, both judged by one
/// deletion oracle whose eligible set is exactly the expected reclaimable
/// candidates (declared from the fixture, never read from the report).
/// The dry run plans exactly `expected`; the reclaim (asked for 100 %, so
/// exit 1) deletes exactly the reclaimable ones and skips every other one
/// for its expected reason. Returns the reclaim's report.
pub(crate) fn dry_run_then_reclaim(
    ctx: &Ctx,
    binary: &Path,
    select: &str,
    roots: Option<&str>,
    path: &str,
    expected: &Expected,
) -> Result<Value, String> {
    let fixture = expected
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .fold(Fixture::new(), |fixture, (path, _)| {
            fixture.eligible(path.clone())
        });
    let oracle = Oracle::arm(fixture)?;
    let dry = run_ensure(ctx, binary, true, select, roots, path)?;
    let run = run_ensure(ctx, binary, false, select, roots, path)?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&dry)?;
    ensure!(
        plan_of(&planned)? == sorted(expected),
        "{select}: the dry run planned {:?}, expected {:?}: {planned}",
        plan_of(&planned)?,
        sorted(expected)
    );
    let reclaimed = report(&run)?;
    let actions = actions(&reclaimed)?;
    ensure!(
        actions == reclaimable(expected),
        "{select}: the reclaim acted on {actions:?}, expected {:?}: {reclaimed}",
        reclaimable(expected)
    );
    for (candidate, reason) in expected {
        match reason {
            None => ensure!(
                !exists(candidate),
                "{select}: {} was reported deleted but exists",
                candidate.display()
            ),
            Some(reason) => {
                let entry = skipped(&reclaimed, candidate)?;
                ensure!(
                    entry["reason"] == *reason,
                    "{select}: {} skipped as {}, expected {reason}: {entry}",
                    candidate.display(),
                    entry["reason"]
                );
                ensure!(
                    exists(candidate),
                    "{select}: kept {} is gone",
                    candidate.display()
                );
            }
        }
    }
    verdict.assert_clean()?;
    Ok(reclaimed)
}

/// The skipped entry names exactly one process blocker, `pid` with
/// `evidence`, and its detail starts with `detail`.
pub(crate) fn expect_blocker(
    report: &Value,
    candidate: &Path,
    pid: u32,
    evidence: &str,
    detail: &str,
) -> TestResult {
    let entry = skipped(report, candidate)?;
    let blockers: Vec<(u64, String)> = entry["process_blockers"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|b| {
                    (
                        b["pid"].as_u64().unwrap_or(0),
                        b["evidence"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    ensure!(
        blockers == [(u64::from(pid), evidence.to_owned())],
        "{} blockers {blockers:?}, expected [({pid}, {evidence})]: {entry}",
        candidate.display()
    );
    ensure!(
        entry["detail"]
            .as_str()
            .is_some_and(|d| d.starts_with(detail)),
        "{} detail {}, expected to start with {detail:?}",
        candidate.display(),
        entry["detail"]
    );
    Ok(())
}

// ---- the candidate catalog -------------------------------------------------

/// The profile names `binary profiles list --json` lists.
pub(crate) fn listed_profiles(ctx: &Ctx, binary: &Path) -> Result<BTreeSet<String>, String> {
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(binary, &["profiles", "list", "--json"])?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(0)?;
    crate::release_suite::profile_names(&run.stdout)
}

/// The candidate list the flake built `diskgc-candidate-catalog` with
/// (diskgc-e2e/candidate-catalog, copied beside the binary).
pub(crate) fn candidate_list(ctx: &Ctx) -> Result<BTreeSet<String>, String> {
    let path = ctx.bin_dir.join("diskgc-candidate-catalog.list");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// Every family whose not-yet-admitted members may be in the test-only
/// candidate list (diskgc-e2e/candidate-catalog): each suite with a
/// candidate-catalog scenario names its family here.
const CANDIDATE_FAMILIES: &[&[&str]] = &[
    crate::project_tree_suite::FAMILY,
    crate::runtime_cache_suite::FAMILY,
];

/// docs/testing.md, Admission evidence (candidate catalog): checks the
/// candidate-catalog build against the release candidate and returns its
/// path. Its catalog is exactly the release candidate's plus the candidate
/// list; the list belongs to the families of [`CANDIDATE_FAMILIES`] and is
/// disjoint from the release catalog; and every member of `family` (the
/// calling suite's) is in one of the two.
pub(crate) fn check_candidate_catalog(ctx: &Ctx, family: &[&str]) -> Result<PathBuf, String> {
    let candidate = ctx.bin_dir.join("diskgc-candidate-catalog");
    let release = crate::release_suite::binary_path(ctx, "diskgc-release");
    let listed = listed_profiles(ctx, &candidate)?;
    let released = listed_profiles(ctx, &release)?;
    let admitted = candidate_list(ctx)?;
    let families: BTreeSet<String> = CANDIDATE_FAMILIES
        .iter()
        .flat_map(|members| members.iter().map(|s| (*s).to_owned()))
        .collect();
    ensure!(
        admitted.is_subset(&families) && released.is_disjoint(&admitted),
        "the candidate list {admitted:?} is not a part of the candidate families {families:?} \
         outside the release candidate's catalog {released:?}"
    );
    let covered: BTreeSet<String> = released.union(&admitted).cloned().collect();
    let family: BTreeSet<String> = family.iter().map(|s| (*s).to_owned()).collect();
    ensure!(
        family.is_subset(&covered),
        "family members in neither the release catalog nor the candidate list: {:?}",
        family.difference(&covered).collect::<Vec<_>>()
    );
    ensure!(
        listed == covered,
        "the candidate catalog lists {listed:?}, expected the release candidate's plus the \
         candidate list: {covered:?}"
    );
    Ok(candidate)
}
