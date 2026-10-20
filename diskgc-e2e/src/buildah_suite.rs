//! A real containers/image spool held by rootless Buildah. The large Nix
//! image stays cached: the runner exposes pinned tools from its read-only
//! store and this test installs only the setuid mapping helpers into the
//! disposable container's writable overlay.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const BUILD_ROOT: &str = "/home/tester/task97-buildah-store";
const BUILD_RUNROOT: &str = "/home/tester/run/task97-buildah";
const CONFIG: &str = "/home/tester/diskgc.yaml";
const PROFILE: &str = r#"version: 1
profiles:
  - name: buildah-live-spool
    find: {roots: [/var/tmp], paths: ["container_images_storage*"], type: dir}
    min_idle_for: 1s
    action: delete
    examples: {match: [/var/tmp/container_images_storage123]}
"#;

/// The shipped source's 3d floor is pinned by the model tests. This kernel
/// case uses a 1s floor so a real Buildah operation can be held long enough
/// to reach the in-use scan without changing the clock or the shipped rule.
pub fn containers_image_spool_held_by_buildah_is_in_use(ctx: &Ctx) -> TestResult {
    install_rootless_mapping_helpers(ctx)?;
    ctx.home.mkdirs(".config/containers", 0o700, TESTER)?;
    ctx.home.mkdir("run", 0o700, TESTER)?;
    ctx.home.write_file(
        ".config/containers/policy.json",
        br#"{"default":[{"type":"insecureAcceptAnything"}]}"#,
        0o600,
        TESTER,
    )?;
    ctx.home
        .write_file("diskgc.yaml", PROFILE.as_bytes(), 0o600, TESTER)?;

    // Incompressible data keeps the destination's blob fd open long enough
    // for the harness to observe it; the fixed 4 MiB bound avoids a large
    // image or a network pull.
    let mut state = 0x7d_4c_32_a1_u32;
    let mut payload = vec![0_u8; 4 * 1024 * 1024];
    for byte in &mut payload {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    ctx.home
        .write_file("task97-payload", &payload, 0o600, TESTER)?;
    drop(payload);

    let from = buildah(ctx)
        .args(["from", "scratch"])
        .output()
        .map_err(|error| format!("start Buildah from scratch: {error}"))?;
    require_buildah(&from, "from scratch")?;
    let container = String::from_utf8_lossy(&from.stdout).trim().to_owned();
    ensure!(
        !container.is_empty(),
        "Buildah from scratch returned no container name"
    );
    let add = buildah(ctx)
        .args(["add", &container, "/home/tester/task97-payload", "/payload"])
        .output()
        .map_err(|error| format!("start Buildah add: {error}"))?;
    require_buildah(&add, "add")?;

    let child = buildah(ctx)
        .args(["commit", &container, "localhost/task97:probe"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start Buildah commit: {error}"))?;
    let mut commit = Commit::new(child);
    let held = commit.stop_at_spool(Duration::from_secs(10))?;
    ensure!(
        fs::metadata(&held.spool)
            .map_err(|error| format!("stat {}: {error}", held.spool.display()))?
            .uid()
            == TESTER.uid,
        "Buildah spool is not tester-owned: {}",
        held.spool.display()
    );
    // The ctime of a newly created spool cannot be backdated by touching
    // mtime. Let the short test floor actually pass while Buildah is stopped.
    std::thread::sleep(Duration::from_secs(2));
    ensure!(
        fs::read_link(&held.fd).ok().as_deref() == Some(held.target.as_path()),
        "Buildah released its spool fd while stopped: {}",
        held.fd.display()
    );

    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.diskgc(),
        &[
            "ensure",
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            CONFIG,
            "--min-free",
            "100%",
            "--select",
            "profile:buildah-live-spool",
            "/var/tmp",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let result = report(&run)?;
    ensure!(
        result["plan"]
            .as_array()
            .is_some_and(|plan| plan.iter().any(|entry| {
                entry["candidate"] == held.spool.to_string_lossy().as_ref()
                    && entry["reason"] == "in_use"
            })),
        "real Buildah spool was not in_use: {result}"
    );
    ensure!(held.spool.exists(), "held Buildah spool was removed");
    verdict.assert_clean()?;
    commit.resume_and_finish()
}

fn install_rootless_mapping_helpers(ctx: &Ctx) -> TestResult {
    for name in ["newuidmap", "newgidmap"] {
        let source = ctx.bin_dir.join(name);
        let destination = Path::new("/bin").join(name);
        fs::copy(&source, &destination).map_err(|error| {
            format!(
                "copy pinned {} into disposable container: {error}",
                source.display()
            )
        })?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o4755))
            .map_err(|error| format!("chmod {}: {error}", destination.display()))?;
    }
    fs::write("/etc/subuid", b"tester:2000:1000\n")
        .map_err(|error| format!("write disposable /etc/subuid: {error}"))?;
    fs::write("/etc/subgid", b"tester:2000:1000\n")
        .map_err(|error| format!("write disposable /etc/subgid: {error}"))?;
    Ok(())
}

fn buildah(ctx: &Ctx) -> Command {
    let mut command = Command::new(&ctx.self_exe);
    command
        .args(["as-tester", "--"])
        .arg(ctx.bin_dir.join("buildah"))
        .args([
            "--root",
            BUILD_ROOT,
            "--runroot",
            BUILD_RUNROOT,
            "--storage-driver",
            "vfs",
        ])
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", "/home/tester")
        .env("XDG_RUNTIME_DIR", "/home/tester/run")
        .current_dir("/");
    command
}

fn require_buildah(output: &Output, operation: &str) -> TestResult {
    ensure!(
        output.status.success(),
        "Buildah {operation} exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct HeldSpool {
    pid: u32,
    fd: PathBuf,
    target: PathBuf,
    spool: PathBuf,
}

struct Commit {
    child: Child,
    stopped: Option<u32>,
}

impl Commit {
    fn new(child: Child) -> Self {
        Self {
            child,
            stopped: None,
        }
    }

    fn stop_at_spool(&mut self, timeout: Duration) -> Result<HeldSpool, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(held) = find_held_spool()? {
                signal(held.pid, libc::SIGSTOP)?;
                self.stopped = Some(held.pid);
                if fs::read_link(&held.fd).ok().as_deref() == Some(held.target.as_path()) {
                    return Ok(held);
                }
                signal(held.pid, libc::SIGCONT)?;
                self.stopped = None;
            }
            if self
                .child
                .try_wait()
                .map_err(|error| format!("poll Buildah commit: {error}"))?
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err("Buildah commit ended before its real spool fd could be held".to_owned())
    }

    fn resume_and_finish(&mut self) -> TestResult {
        if let Some(pid) = self.stopped.take() {
            signal(pid, libc::SIGCONT)?;
        }
        let status = self
            .child
            .wait()
            .map_err(|error| format!("wait Buildah commit: {error}"))?;
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_string(&mut stderr)
                .map_err(|error| format!("read Buildah commit stderr: {error}"))?;
        }
        ensure!(status.success(), "Buildah commit exited {status}: {stderr}");
        Ok(())
    }
}

impl Drop for Commit {
    fn drop(&mut self) {
        if let Some(pid) = self.stopped.take() {
            let _ = signal(pid, libc::SIGCONT);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn signal(pid: u32, signal: i32) -> TestResult {
    // SAFETY: kill takes a plain process ID and signal number. Both are
    // obtained from /proc or libc; no pointer is passed.
    let result = unsafe { libc::kill(pid as i32, signal) };
    ensure!(
        result == 0,
        "signal {signal} to Buildah pid {pid}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn find_held_spool() -> Result<Option<HeldSpool>, String> {
    for process in fs::read_dir("/proc").map_err(|error| format!("read /proc: {error}"))? {
        let process = process.map_err(|error| format!("read /proc entry: {error}"))?;
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Ok(relative) = target.strip_prefix("/var/tmp") else {
                continue;
            };
            let Some(first) = relative.components().next() else {
                continue;
            };
            let name = first.as_os_str().to_string_lossy();
            if !name.starts_with("container_images_storage") || relative.components().count() < 2 {
                continue;
            }
            let spool = Path::new("/var/tmp").join(first.as_os_str());
            if fs::metadata(&spool).is_ok_and(|meta| meta.uid() == TESTER.uid) {
                return Ok(Some(HeldSpool {
                    pid,
                    fd: fd.path(),
                    target,
                    spool,
                }));
            }
        }
    }
    Ok(None)
}
