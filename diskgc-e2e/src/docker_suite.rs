//! Native Docker Engine smoke in a disposable dind image. Podman is only the
//! outer container runtime; every `docker` command here is the pinned
//! `/usr/local/bin/docker` client talking to the child `/usr/local/bin/dockerd`
//! over a socket created inside this container.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use diskgc_core::docker_backend::{DockerInfo, ImageStore};
use serde_json::Value;

use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::scratch;
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::under_test;
use crate::{TESTER, ensure, info};

const DOCKER: &str = "/usr/local/bin/docker";
const DOCKERD: &str = "/usr/local/bin/dockerd";
const SOCKET: &str = "/var/run/docker.sock";
const ENGINE_VERSION: &str = "29.8.2";
const IMAGE: &str = "diskgc-native-smoke:1";
const CONTAINER: &str = "diskgc-native-held";
const VOLUME: &str = "diskgc-native-volume";

/// Owns the child daemon so an early test failure still stops it. Podman
/// removes the entire container, including both data tmpfs mounts, after
/// the test exits.
pub(crate) struct Daemon {
    child: Child,
    log_path: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    pub(crate) fn start(ctx: &Ctx) -> Result<Self, String> {
        require_tmpfs(Path::new("/var/lib/docker"))?;
        require_tmpfs(Path::new("/var/lib/containerd"))?;
        let log_path = ctx
            .records
            .write_file("dockerd.log", b"", 0o600, crate::ROOT)?;
        let log = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| format!("open {}: {error}", log_path.display()))?;
        let child = Command::new(DOCKERD)
            .args([
                "--host=unix:///var/run/docker.sock",
                "--data-root=/var/lib/docker",
                "--exec-root=/run/docker",
                "--pidfile=/run/docker-dind.pid",
                "--group=tester",
                "--iptables=false",
                "--ip6tables=false",
                "--bridge=none",
                "--tls=false",
                "--feature=containerd-snapshotter=true",
            ])
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|error| format!("clone daemon log: {error}"))?,
            )
            .stderr(
                log.try_clone()
                    .map_err(|error| format!("clone daemon log: {error}"))?,
            )
            .spawn()
            .map_err(|error| format!("start native {DOCKERD}: {error}"))?;
        let mut daemon = Self { child, log_path };
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(status) = daemon
                .child
                .try_wait()
                .map_err(|error| format!("check dockerd process: {error}"))?
            {
                return Err(format!(
                    "native dockerd exited {status} before its private socket became ready:\n{}",
                    daemon.log_tail()
                ));
            }
            if docker_output(ctx, &["version", "--format", "{{json .Server}}"], false)
                .is_ok_and(|out| out.status.success())
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "native dockerd did not serve {SOCKET} within 45s:\n{}",
                    daemon.log_tail()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let socket = std::fs::symlink_metadata(SOCKET)
            .map_err(|error| format!("stat private Docker socket {SOCKET}: {error}"))?;
        ensure!(
            socket.file_type().is_socket(),
            "{SOCKET} is not a Unix socket"
        );
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("read mountinfo for Docker socket: {error}"))?;
        ensure!(
            !mountinfo
                .lines()
                .any(|line| line.contains(" /run/docker.sock ")
                    || line.contains(" /var/run/docker.sock ")),
            "Docker socket is a separate mount, possibly a host socket"
        );
        Ok(daemon)
    }

    fn log_tail(&mut self) -> String {
        let mut text = String::new();
        if let Err(error) =
            File::open(&self.log_path).and_then(|mut log| log.read_to_string(&mut text))
        {
            return format!("read {}: {error}", self.log_path.display());
        }
        let start = text.len().saturating_sub(8 * 1024);
        text.get(start..).unwrap_or(&text).to_owned()
    }
}

/// The runner promised these two mounts. A missing mount aborts before the
/// daemon can write to the image's overlay or an anonymous Podman volume.
fn require_tmpfs(path: &Path) -> TestResult {
    let root_id = scratch::mount_id(path, false)?;
    let parent_id = scratch::mount_id(path.parent().ok_or("tmpfs path has no parent")?, false)?;
    ensure!(
        root_id != parent_id,
        "{} is not a separate mount",
        path.display()
    );
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("read mountinfo: {error}"))?;
    let target = path.to_str().ok_or("non-UTF-8 tmpfs path")?;
    let found = mountinfo.lines().find_map(|line| {
        let (mount, fs) = line.split_once(" - ")?;
        (mount.split_whitespace().nth(4) == Some(target))
            .then(|| fs.split_whitespace().next().unwrap_or(""))
    });
    ensure!(
        found == Some("tmpfs"),
        "{target} must be tmpfs, got {found:?}"
    );
    Ok(())
}

/// Production users need search permission on the daemon-reported storage
/// roots to verify their filesystem. Grant only search (no read/write) in
/// this disposable fixture; the ordinary 0700 mode is tested separately as
/// a fail-closed preflight, never silently assumed traversable.
fn grant_store_traversal() -> TestResult {
    for path in [
        "/var/lib/docker",
        "/var/lib/docker/containerd",
        "/var/lib/docker/containerd/daemon",
    ] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o711))
            .map_err(|error| format!("grant fixture search permission on {path}: {error}"))?;
    }
    Ok(())
}

fn docker_output(ctx: &Ctx, args: &[&str], as_tester: bool) -> Result<Output, String> {
    let mut command = if as_tester {
        let mut command = Command::new(&ctx.self_exe);
        command.args(["as-tester", "--", DOCKER]);
        command
    } else {
        Command::new(DOCKER)
    };
    command
        .args(args)
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .env(
            "DOCKER_CONFIG",
            if as_tester {
                "/tmp/docker-client"
            } else {
                "/tmp/docker-fixture-client"
            },
        )
        .env_remove("DOCKER_CONTEXT")
        .stdin(Stdio::null());
    command
        .output()
        .map_err(|error| format!("run native docker {args:?}: {error}"))
}

fn docker_ok(ctx: &Ctx, args: &[&str], as_tester: bool) -> Result<String, String> {
    let out = docker_output(ctx, args, as_tester)?;
    if !out.status.success() {
        return Err(format!(
            "native docker {args:?} exited {}:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Stable BuildKit record IDs provide a deletion oracle independent of the
/// Docker prune command's success code or its reported reclaimed bytes.
fn reclaimable_build_cache(ctx: &Ctx) -> Result<HashSet<String>, String> {
    let output = docker_ok(
        ctx,
        &["buildx", "du", "--builder", "default", "--format=json"],
        false,
    )?;
    output
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line)
                .map_err(|error| format!("parse BuildKit cache record {line:?}: {error}"))?;
            let id = record["ID"]
                .as_str()
                .ok_or_else(|| format!("BuildKit cache record has no ID: {record}"))?;
            let reclaimable = record["Reclaimable"]
                .as_bool()
                .ok_or_else(|| format!("BuildKit cache record has no reclaimability: {record}"))?;
            Ok((id.to_owned(), reclaimable))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(|records| {
            records
                .into_iter()
                .filter_map(|(id, reclaimable)| reclaimable.then_some(id))
                .collect()
        })
}

/// Import a local tar stream. The fixture never needs a registry or a host
/// image, so a prune assertion cannot accidentally target external data.
fn import_image(ctx: &Ctx, tag: Option<&str>) -> Result<String, String> {
    let mut tar = Command::new("/bin/tar")
        .args(["-C", "/tmp/docker-source", "-cf", "-", "payload"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("archive Docker payload: {error}"))?;
    let tar_stream = tar.stdout.take().ok_or("tar has no stdout pipe")?;
    let mut args = vec!["import", "-"];
    if let Some(tag) = tag {
        args.push(tag);
    }
    let import = Command::new(DOCKER)
        .args(&args)
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .env("DOCKER_CONFIG", "/tmp/docker-fixture-client")
        .env_remove("DOCKER_CONTEXT")
        .stdin(tar_stream)
        .output()
        .map_err(|error| format!("import offline Docker image: {error}"))?;
    let tar_status = tar
        .wait()
        .map_err(|error| format!("wait for tar: {error}"))?;
    ensure!(
        tar_status.success(),
        "offline Docker tar exited {tar_status}"
    );
    ensure!(
        import.status.success(),
        "native Docker import exited {}: {}",
        import.status,
        String::from_utf8_lossy(&import.stderr)
    );
    let id = String::from_utf8_lossy(&import.stdout).trim().to_owned();
    ensure!(
        id.starts_with("sha256:"),
        "native Docker import returned no image ID: {id:?}"
    );
    let inspected = docker_ok(
        ctx,
        &[
            "image",
            "inspect",
            tag.unwrap_or(&id),
            "--format",
            "{{.Id}}",
        ],
        false,
    )?;
    ensure!(
        inspected == id,
        "imported image ID {id} differs from inspect {inspected}"
    );
    Ok(id)
}

fn validate_server_identity(server: &Value, expected_version: &str) -> TestResult {
    let name = server["Platform"]["Name"].as_str().unwrap_or("");
    let version = server["Version"].as_str().unwrap_or("");
    ensure!(
        name.starts_with("Docker Engine") && version == expected_version,
        "server is not pinned Docker Engine {expected_version}: name={name:?}, version={version:?}"
    );
    Ok(())
}

/// Require the pinned native Engine behind the socket before a Docker E2E
/// attributes any result to Docker rather than a compatible CLI server.
pub(crate) fn assert_native_server_identity(ctx: &Ctx) -> TestResult {
    let server_text = docker_ok(ctx, &["version", "--format", "{{json .Server}}"], true)?;
    let server: Value = serde_json::from_str(&server_text)
        .map_err(|error| format!("parse Docker server identity {server_text:?}: {error}"))?;
    validate_server_identity(&server, ENGINE_VERSION)
}

/// A native Docker server, image, container and volume entirely inside the
/// disposable Podman container. The diskgc binary under test is still run
/// and judged by the deletion oracle; no Docker kind is required yet.
pub fn native_docker_engine_dind_smoke(ctx: &Ctx) -> TestResult {
    ensure!(
        ctx.image.name == "docker-dind",
        "wrong image: {}",
        ctx.image.name
    );
    ctx.scratch.mkdir("docker-client", 0o755, TESTER)?;
    ctx.scratch.mkdir("docker-source", 0o755, TESTER)?;
    ctx.scratch.write_file(
        "docker-source/payload",
        b"offline Docker image\n",
        0o644,
        TESTER,
    )?;

    let _daemon = Daemon::start(ctx)?;
    let server_text = docker_ok(ctx, &["version", "--format", "{{json .Server}}"], true)?;
    let server: Value = serde_json::from_str(&server_text)
        .map_err(|error| format!("parse Docker server identity {server_text:?}: {error}"))?;
    // Bite: the same real response must fail a wrong expected engine version.
    ensure!(
        validate_server_identity(&server, "0.0.0").is_err(),
        "Docker server identity check accepted a deliberately wrong version"
    );
    validate_server_identity(&server, ENGINE_VERSION)?;
    info!("native Docker server: {}", server_text);
    let info_text = docker_ok(ctx, &["info", "--format", "{{json .}}"], true)?;
    let daemon_info = DockerInfo::parse(&info_text)?;
    let ImageStore::BundledContainerd(image_store) = daemon_info.image_store else {
        return Err(
            "native Docker fixture did not use its bundled containerd image store".to_owned(),
        );
    };
    info!("native Docker image store: {}", image_store.display());

    let _imported_id = import_image(ctx, Some(IMAGE))?;
    let blobs = image_store.join("io.containerd.content.v1.content/blobs/sha256");
    let mut entries = std::fs::read_dir(&blobs)
        .map_err(|error| format!("read imported image content {}: {error}", blobs.display()))?;
    ensure!(
        entries.next().is_some(),
        "imported image left no content in {}",
        blobs.display()
    );
    let blobs = image_store.join("io.containerd.content.v1.content/blobs/sha256");
    let entries = std::fs::read_dir(&blobs)
        .map_err(|error| format!("read imported image content {}: {error}", blobs.display()))?;
    let mut found_blob = false;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read image blob entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("non-UTF-8 image blob name")?;
        let digest_name = name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit());
        let kind = entry
            .file_type()
            .map_err(|error| format!("stat image blob entry: {error}"))?;
        if digest_name && kind.is_file() {
            let size = entry
                .metadata()
                .map_err(|error| format!("measure image blob entry: {error}"))?
                .len();
            if size > 0 {
                found_blob = true;
                break;
            }
        }
    }
    ensure!(
        found_blob,
        "imported image left no blob in {}",
        blobs.display()
    );
    let mut external_entries = std::fs::read_dir("/var/lib/containerd")
        .map_err(|error| format!("read separate containerd root: {error}"))?;
    ensure!(
        external_entries.next().is_none(),
        "imported image also wrote to separate /var/lib/containerd"
    );
    docker_ok(
        ctx,
        &["create", "--name", CONTAINER, IMAGE, "/payload"],
        false,
    )?;
    docker_ok(ctx, &["volume", "create", VOLUME], false)?;
    let image = docker_ok(
        ctx,
        &["image", "inspect", IMAGE, "--format", "{{.Id}}"],
        false,
    )?;
    let container = docker_ok(
        ctx,
        &["container", "inspect", CONTAINER, "--format", "{{.Id}}"],
        false,
    )?;
    let volume = docker_ok(
        ctx,
        &["volume", "inspect", VOLUME, "--format", "{{.Name}}"],
        false,
    )?;
    ensure!(volume == VOLUME, "unexpected Docker volume {volume:?}");

    let oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let run = ctx.run_as_tester(&ctx.diskgc(), &["profiles", "list"])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;
    verdict.assert_clean()?;
    ensure!(
        docker_ok(
            ctx,
            &["image", "inspect", IMAGE, "--format", "{{.Id}}"],
            false
        )? == image,
        "Docker image changed during diskgc smoke"
    );
    ensure!(
        docker_ok(
            ctx,
            &["container", "inspect", CONTAINER, "--format", "{{.Id}}"],
            false
        )? == container,
        "Docker container changed during diskgc smoke"
    );
    ensure!(
        docker_ok(
            ctx,
            &["volume", "inspect", VOLUME, "--format", "{{.Name}}"],
            false
        )? == volume,
        "Docker volume changed during diskgc smoke"
    );
    Ok(())
}

/// Reclaim only an unreferenced image through diskgc's fixed Docker action.
/// The stopped container's image and a volume are independent preservation
/// oracles, queried through the same private native daemon after the run.
pub fn native_docker_image_prune_preserves_containers_volumes(ctx: &Ctx) -> TestResult {
    ensure!(
        ctx.image.name == "docker-dind",
        "wrong image: {}",
        ctx.image.name
    );
    ctx.scratch.mkdir("docker-client", 0o755, TESTER)?;
    ctx.scratch.mkdir("docker-source", 0o755, TESTER)?;
    ctx.scratch.write_file(
        "docker-source/payload",
        b"offline image for prune\n",
        0o644,
        TESTER,
    )?;
    let _daemon = Daemon::start(ctx)?;
    assert_native_server_identity(ctx)?;
    let held = import_image(ctx, Some("diskgc-held:1"))?;
    let unused = import_image(ctx, Some("diskgc-unused:1"))?;
    ensure!(
        held != unused,
        "offline images unexpectedly have the same ID"
    );
    docker_ok(
        ctx,
        &[
            "create",
            "--name",
            "diskgc-held-container",
            "diskgc-held:1",
            "/payload",
        ],
        false,
    )?;
    docker_ok(ctx, &["volume", "create", "diskgc-held-volume"], false)?;
    let container = docker_ok(
        ctx,
        &[
            "container",
            "inspect",
            "diskgc-held-container",
            "--format",
            "{{.Id}}",
        ],
        false,
    )?;

    let env = vec![
        ("PATH".to_owned(), "/usr/local/bin:/bin".to_owned()),
        (
            "DOCKER_HOST".to_owned(),
            "unix:///var/run/docker.sock".to_owned(),
        ),
        ("DOCKER_CONFIG".to_owned(), "/tmp/docker-client".to_owned()),
    ];
    let args = [
        "ensure",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-unreferenced",
        "/var/lib/docker",
    ];
    let dry_args = [
        "ensure",
        "--dry-run",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-unreferenced",
        "/var/lib/docker",
    ];
    let denied_oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let denied = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &dry_args,
    )?;
    denied_oracle.judge(&denied)?.assert_clean()?;
    ensure!(
        denied.code != Some(0) && denied.stderr.contains("EACCES"),
        "unreadable Docker store did not fail closed: {denied:?}"
    );
    grant_store_traversal()?;
    let dry_oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let dry = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &dry_args,
    )?;
    dry_oracle.judge(&dry)?.assert_clean()?;
    dry.expect_code(0)?;
    let dry_report = report(&dry)?;
    ensure!(
        dry_report["plan"]
            .as_array()
            .is_some_and(|plan| plan.len() == 1 && plan[0]["reason"].is_null()),
        "native Docker backend did not yield one eligible dry-run plan: {dry_report}"
    );
    let oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let run = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &args,
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    verdict.assert_clean()?;
    let result = report(&run)?;
    ensure!(
        result["actions"]
            .as_array()
            .is_some_and(|actions| actions.len() == 1),
        "Docker prune did not report one action: {result}"
    );
    let absent = docker_output(ctx, &["image", "inspect", "diskgc-unused:1"], false)?;
    ensure!(
        !absent.status.success(),
        "unreferenced image {unused} survived prune"
    );
    ensure!(
        docker_ok(
            ctx,
            &["image", "inspect", "diskgc-held:1", "--format", "{{.Id}}"],
            false
        )? == held,
        "container's image changed"
    );
    ensure!(
        docker_ok(
            ctx,
            &[
                "container",
                "inspect",
                "diskgc-held-container",
                "--format",
                "{{.Id}}"
            ],
            false
        )? == container,
        "stopped container changed"
    );
    ensure!(
        docker_ok(
            ctx,
            &[
                "volume",
                "inspect",
                "diskgc-held-volume",
                "--format",
                "{{.Name}}"
            ],
            false
        )? == "diskgc-held-volume",
        "volume changed"
    );
    Ok(())
}

/// The Docker CLI inherits the host lock before a prune starts. Stop that
/// child at a test-only gate, kill diskgc, and verify that the lock remains
/// held until the child finishes its real prune against the private daemon.
pub fn native_docker_prune_child_keeps_lock_after_parent_kill(ctx: &Ctx) -> TestResult {
    ensure!(
        ctx.image.name == "docker-dind",
        "wrong image: {}",
        ctx.image.name
    );
    ctx.scratch.mkdir("docker-client", 0o755, TESTER)?;
    ctx.scratch.mkdir("docker-source", 0o755, TESTER)?;
    ctx.scratch
        .write_file("docker-source/payload", b"lock test image\n", 0o644, TESTER)?;
    let _daemon = Daemon::start(ctx)?;
    assert_native_server_identity(ctx)?;
    let unused = import_image(ctx, Some("diskgc-lock-unused:1"))?;
    grant_store_traversal()?;
    let original_socket = rustix::fs::open(
        SOCKET,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| format!("pin fixture's original Docker socket: {error}"))?;

    let ready = ctx.scratch.path("docker-prune-ready");
    let gate = ctx.scratch.path("docker-prune-go");
    let bin = ctx.scratch.mkdir("docker-bin", 0o755, TESTER)?;
    let script = format!(
        "#!/bin/sh\nif [ \"$2\" = image ] && [ \"$3\" = prune ]; then\n  case \"$1\" in --host=unix:///proc/self/fd/*) ;; *) exit 91 ;; esac\n  printf '%s\\n' \"$PPID\" > {}\n  while [ ! -e {} ]; do sleep 0.02; done\nfi\nexec /usr/local/bin/docker \"$@\"\n",
        ready.display(),
        gate.display()
    );
    ctx.scratch
        .write_file("docker-bin/docker", script.as_bytes(), 0o755, TESTER)?;
    let env = vec![
        (
            "PATH".to_owned(),
            format!("{}:/usr/local/bin:/bin", bin.display()),
        ),
        (
            "DOCKER_HOST".to_owned(),
            "unix:///var/run/docker.sock".to_owned(),
        ),
        ("DOCKER_CONFIG".to_owned(), "/tmp/docker-client".to_owned()),
    ];
    let args = [
        "ensure",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-unreferenced",
        "/var/lib/docker",
    ];
    let oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker")
            .concurrent_writer("/run/docker.sock"),
    )?;
    let spawned = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &args,
    )?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let parent_pid: u32 = loop {
        match std::fs::read_to_string(&ready) {
            Ok(text) => {
                break text.trim().parse().map_err(|error| {
                    format!("invalid Docker prune parent pid {text:?}: {error}")
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("read Docker prune gate: {error}")),
        }
        ensure!(
            Instant::now() < deadline,
            "Docker prune child did not reach test gate"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        docker_lock_held()?,
        "host lock was free while Docker prune child waited"
    );
    let parent = rustix::process::Pid::from_raw(parent_pid.cast_signed())
        .ok_or("invalid Docker prune parent pid")?;
    rustix::process::kill_process(parent, rustix::process::Signal::KILL)
        .map_err(|error| format!("kill diskgc parent {parent_pid}: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !docker_process_exited(parent_pid)? {
        ensure!(
            Instant::now() < deadline,
            "diskgc parent {parent_pid} survived SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let child_held_lock = docker_lock_held()?;
    // Replace the public socket after diskgc pinned it and its prune child
    // reached the gate. A path-based client would now reach this decoy.
    std::fs::remove_file(SOCKET)
        .map_err(|error| format!("unlink private Docker socket for race test: {error}"))?;
    let decoy = UnixListener::bind(SOCKET)
        .map_err(|error| format!("bind replacement private Docker socket: {error}"))?;
    decoy
        .set_nonblocking(true)
        .map_err(|error| format!("make replacement Docker socket nonblocking: {error}"))?;
    let decoy_worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match decoy.accept() {
                Ok((mut client, _)) => {
                    let _ = client.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                    );
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return true,
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    ctx.scratch
        .write_file("docker-prune-go", b"", 0o644, TESTER)?;
    let run = spawned.wait()?;
    oracle.judge(&run)?.assert_clean()?;
    let decoy_contacted = decoy_worker.join().map_err(|_| "decoy thread panicked")?;
    ensure!(
        run.signal == Some(9),
        "diskgc parent did not die by SIGKILL: {run:?}"
    );
    ensure!(
        child_held_lock,
        "Docker prune child lost the host lock after diskgc died"
    );
    ensure!(
        !docker_lock_held()?,
        "host lock remains held after Docker client exited"
    );
    ensure!(
        !decoy_contacted,
        "Docker prune connected to replacement socket instead of pinned daemon"
    );
    let mut connection =
        UnixStream::connect(format!("/proc/self/fd/{}", original_socket.as_raw_fd()))
            .map_err(|error| format!("query original Docker socket after replacement: {error}"))?;
    connection
        .write_all(b"GET /images/diskgc-lock-unused:1/json HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")
        .map_err(|error| format!("send image inspection to original Docker daemon: {error}"))?;
    let mut response = String::new();
    connection
        .read_to_string(&mut response)
        .map_err(|error| format!("read original Docker daemon response: {error}"))?;
    ensure!(
        response.starts_with("HTTP/1.1 404"),
        "unreferenced image {unused} survived the resumed Docker prune"
    );
    Ok(())
}

/// Exercise both remaining Docker built-ins with native BuildKit output.
/// Building the same tag twice seeds cache; importing a separate untagged
/// image supplies the dangling target. A stopped container and volume stay
/// as preservation oracles.
pub fn native_docker_dangling_and_build_cache_prune(ctx: &Ctx) -> TestResult {
    ensure!(
        ctx.image.name == "docker-dind",
        "wrong image: {}",
        ctx.image.name
    );
    ctx.scratch.mkdir("docker-client", 0o755, TESTER)?;
    ctx.scratch.mkdir("docker-source", 0o755, TESTER)?;
    ctx.scratch.write_file(
        "docker-source/Dockerfile",
        b"FROM scratch AS source\nCOPY payload /payload\nFROM scratch\nCOPY --from=source /payload /payload\n",
        0o644,
        TESTER,
    )?;
    ctx.scratch
        .write_file("docker-source/payload", b"first\n", 0o644, TESTER)?;
    let _daemon = Daemon::start(ctx)?;
    assert_native_server_identity(ctx)?;
    docker_ok(
        ctx,
        &[
            "build",
            "--network=none",
            "-t",
            "diskgc-built:1",
            "/tmp/docker-source",
        ],
        false,
    )?;
    let old_image = docker_ok(
        ctx,
        &["image", "inspect", "diskgc-built:1", "--format", "{{.Id}}"],
        false,
    )?;
    ctx.scratch.remove_file("docker-source/payload")?;
    ctx.scratch
        .write_file("docker-source/payload", b"second\n", 0o644, TESTER)?;
    docker_ok(
        ctx,
        &[
            "build",
            "--network=none",
            "-t",
            "diskgc-built:1",
            "/tmp/docker-source",
        ],
        false,
    )?;
    let held_image = docker_ok(
        ctx,
        &["image", "inspect", "diskgc-built:1", "--format", "{{.Id}}"],
        false,
    )?;
    ensure!(
        old_image != held_image,
        "BuildKit did not produce distinct image IDs"
    );
    let dangling = import_image(ctx, None)?;
    ensure!(
        dangling != held_image,
        "dangling image shares held image ID"
    );
    docker_ok(
        ctx,
        &[
            "create",
            "--name",
            "diskgc-built-held",
            "diskgc-built:1",
            "/payload",
        ],
        false,
    )?;
    docker_ok(ctx, &["volume", "create", "diskgc-built-volume"], false)?;
    let cache_before = reclaimable_build_cache(ctx)?;
    ensure!(
        !cache_before.is_empty(),
        "BuildKit produced no reclaimable cache records"
    );
    // The fixed `docker builder prune` command uses the Engine API even
    // when Buildx selects a separate builder. Selection must not redirect
    // this profile to a docker-container builder.
    docker_ok(
        ctx,
        &[
            "buildx",
            "create",
            "--name",
            "side-builder",
            "--driver",
            "docker-container",
            "--use",
        ],
        true,
    )?;
    let builders = docker_ok(ctx, &["buildx", "ls"], true)?;
    ensure!(
        builders.contains("side-builder*"),
        "Buildx did not select the non-default builder: {builders}"
    );
    grant_store_traversal()?;
    std::fs::set_permissions(
        "/var/lib/docker/buildkit",
        std::fs::Permissions::from_mode(0o711),
    )
    .map_err(|error| format!("grant fixture BuildKit search permission: {error}"))?;
    let env = vec![
        ("PATH".to_owned(), "/usr/local/bin:/bin".to_owned()),
        (
            "DOCKER_HOST".to_owned(),
            "unix:///var/run/docker.sock".to_owned(),
        ),
        ("DOCKER_CONFIG".to_owned(), "/tmp/docker-client".to_owned()),
        ("BUILDX_BUILDER".to_owned(), "side-builder".to_owned()),
    ];
    // Endpoint selection must fail before any prune. The remote context is
    // real client configuration, but its endpoint is deliberately unreachable.
    docker_ok(
        ctx,
        &[
            "context",
            "create",
            "diskgc-remote",
            "--docker",
            "host=tcp://127.0.0.1:2375",
        ],
        true,
    )?;
    let mut remote_env: Vec<_> = env
        .iter()
        .filter(|(key, _)| key != "DOCKER_HOST")
        .cloned()
        .collect();
    remote_env.push(("DOCKER_CONTEXT".to_owned(), "diskgc-remote".to_owned()));
    let mut rootless_env = env.clone();
    rootless_env
        .iter_mut()
        .find(|(key, _)| key == "DOCKER_HOST")
        .ok_or("Docker fixture has no DOCKER_HOST")?
        .1 = "unix:///run/user/1000/docker.sock".to_owned();
    let cache_before_rejected = reclaimable_build_cache(ctx)?;
    ensure!(
        !cache_before_rejected.is_empty(),
        "rejected backends had no build cache to preserve"
    );
    for (case, rejected_env, reason) in [
        ("remote context", &remote_env, "unsupported_backend"),
        ("rootless socket", &rootless_env, "unsupported_backend"),
    ] {
        let args = [
            "ensure",
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:docker-build-cache",
            "/var/lib/docker",
        ];
        let oracle = Oracle::arm(
            Fixture::new()
                .concurrent_writer("/var/lib/docker")
                .concurrent_writer("/var/lib/containerd")
                .concurrent_writer("/run/docker"),
        )?;
        let run = under_test::run_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            rejected_env,
            &ctx.diskgc(),
            &args,
        )?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(1)?;
        let result = report(&run)?;
        ensure!(
            result["skipped"].as_array().is_some_and(|skipped| {
                !skipped.is_empty() && skipped.iter().all(|item| item["reason"] == reason)
            }) && result["actions"].as_array().is_some_and(Vec::is_empty),
            "{case} was not refused before pruning: {result}"
        );
        ensure!(
            reclaimable_build_cache(ctx)? == cache_before_rejected,
            "{case} pruned BuildKit cache despite the rejected binding"
        );
    }
    let bad_cli_dir = ctx.scratch.mkdir("docker-bad-cli", 0o755, TESTER)?;
    ctx.scratch.write_file(
        "docker-bad-cli/docker",
        b"#!/bin/sh\nif [ \"$1\" = version ] && [ \"$2\" = --format ] && [ \"$3\" = '{{json .Client}}' ]; then printf '%s\\n' '{\"Version\":\"30.0.0\"}'; exit 0; fi\nexec /usr/local/bin/docker \"$@\"\n",
        0o755,
        TESTER,
    )?;
    let mut bad_cli_env = env.clone();
    bad_cli_env[0].1 = format!("{}:/usr/local/bin:/bin", bad_cli_dir.display());
    let bad_cli_args = [
        "ensure",
        "--dry-run",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-build-cache",
        "/var/lib/docker",
    ];
    let bad_cli_oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let bad_cli = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &bad_cli_env,
        &ctx.diskgc(),
        &bad_cli_args,
    )?;
    bad_cli_oracle.judge(&bad_cli)?.assert_clean()?;
    let bad_cli_report = report(&bad_cli)?;
    ensure!(
        bad_cli_report["plan"].as_array().is_some_and(|plan| plan
            .iter()
            .all(|item| item["reason"] == "unsupported_backend"))
            && bad_cli_report["actions"]
                .as_array()
                .is_some_and(Vec::is_empty),
        "unverified Docker CLI dispatch was not refused before prune: {bad_cli_report}"
    );
    let recent_args = [
        "ensure",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-build-cache && filter:idle-1h",
        "/var/lib/docker",
    ];
    let recent_oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let recent = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &recent_args,
    )?;
    recent_oracle.judge(&recent)?.assert_clean()?;
    recent.expect_code(1)?;
    ensure!(
        reclaimable_build_cache(ctx)? == cache_before,
        "one-hour idle filter removed recently used BuildKit records"
    );
    for profile in ["docker-dangling", "docker-build-cache"] {
        let dry_args = [
            "ensure",
            "--dry-run",
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            &format!("profile:{profile}"),
            "/var/lib/docker",
        ];
        let dry_oracle = Oracle::arm(
            Fixture::new()
                .concurrent_writer("/var/lib/docker")
                .concurrent_writer("/var/lib/containerd")
                .concurrent_writer("/run/docker"),
        )?;
        let dry = under_test::run_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &env,
            &ctx.diskgc(),
            &dry_args,
        )?;
        dry_oracle.judge(&dry)?.assert_clean()?;
        dry.expect_code(0)?;
        let dry_report = report(&dry)?;
        ensure!(
            dry_report["plan"]
                .as_array()
                .is_some_and(|plan| plan.len() == 1 && plan[0]["reason"].is_null()),
            "{profile} had no eligible native plan: {dry_report}"
        );
        let args = [
            "ensure",
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            &format!("profile:{profile}"),
            "/var/lib/docker",
        ];
        let oracle = Oracle::arm(
            Fixture::new()
                .concurrent_writer("/var/lib/docker")
                .concurrent_writer("/var/lib/containerd")
                .concurrent_writer("/run/docker"),
        )?;
        let run = under_test::run_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &env,
            &ctx.diskgc(),
            &args,
        )?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(1)?;
        let result = report(&run)?;
        ensure!(
            result["actions"]
                .as_array()
                .is_some_and(|actions| actions.len() == 1),
            "{profile} did not prune: {result}"
        );
        if profile == "docker-dangling" {
            ensure!(
                !docker_output(ctx, &["image", "inspect", &dangling], false)?
                    .status
                    .success(),
                "dangling image {dangling} survived docker-dangling prune"
            );
        } else {
            let cache_after = reclaimable_build_cache(ctx)?;
            ensure!(
                cache_before.iter().any(|id| !cache_after.contains(id)),
                "docker-build-cache removed no reclaimable BuildKit record: before={cache_before:?}, after={cache_after:?}"
            );
        }
    }
    // A short age window proves the `until` filter on this pinned Engine,
    // including its old-record side. The earlier one-hour run proved fresh
    // records survive; a new offline build supplies records to age here.
    ctx.scratch.remove_file("docker-source/payload")?;
    ctx.scratch
        .write_file("docker-source/payload", b"third\n", 0o644, TESTER)?;
    docker_ok(
        ctx,
        &[
            "build",
            "--network=none",
            "-t",
            "diskgc-aged:1",
            "/tmp/docker-source",
        ],
        false,
    )?;
    let aged_before = reclaimable_build_cache(ctx)?;
    ensure!(
        !aged_before.is_empty(),
        "third BuildKit build produced no reclaimable cache"
    );
    // BuildKit updates cache last-use metadata asynchronously after the
    // build command exits. Give its one-second cutoff ample separation.
    std::thread::sleep(Duration::from_secs(10));
    ctx.scratch.write_file(
        "docker-idle.yaml",
        b"version: 1\nprofiles:\n  - name: idle-1s\n    where: {idle_for: 1s}\n",
        0o644,
        TESTER,
    )?;
    let aged_args = [
        "ensure",
        "--json",
        "--config",
        "/tmp/docker-idle.yaml",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:docker-build-cache && filter:idle-1s",
        "/var/lib/docker",
    ];
    let aged_oracle = Oracle::arm(
        Fixture::new()
            .concurrent_writer("/var/lib/docker")
            .concurrent_writer("/var/lib/containerd")
            .concurrent_writer("/run/docker"),
    )?;
    let aged = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &aged_args,
    )?;
    aged_oracle.judge(&aged)?.assert_clean()?;
    aged.expect_code(1)?;
    let aged_report = report(&aged)?;
    let aged_after = reclaimable_build_cache(ctx)?;
    ensure!(
        aged_before.iter().any(|id| !aged_after.contains(id)),
        "one-second idle filter removed no aged BuildKit record: report={aged_report}, before={aged_before:?}, after={aged_after:?}"
    );
    ensure!(
        !docker_output(ctx, &["image", "inspect", &dangling], false)?
            .status
            .success(),
        "dangling image {dangling} survived"
    );
    ensure!(
        docker_ok(
            ctx,
            &["image", "inspect", "diskgc-built:1", "--format", "{{.Id}}"],
            false
        )? == held_image,
        "held image changed"
    );
    ensure!(
        docker_ok(
            ctx,
            &[
                "container",
                "inspect",
                "diskgc-built-held",
                "--format",
                "{{.Image}}"
            ],
            false
        )? == held_image,
        "stopped container changed"
    );
    ensure!(
        docker_ok(
            ctx,
            &[
                "volume",
                "inspect",
                "diskgc-built-volume",
                "--format",
                "{{.Name}}"
            ],
            false
        )? == "diskgc-built-volume",
        "volume changed"
    );
    Ok(())
}

fn docker_process_exited(pid: u32) -> Result<bool, String> {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(status) => Ok(status
            .lines()
            .any(|line| line.starts_with("State:\tZ") || line.starts_with("State:\tX"))),
        Err(error) => Err(format!("read process {pid} status: {error}")),
    }
}

fn docker_lock_held() -> Result<bool, String> {
    let fd = rustix::fs::open(
        HOST_LOCK,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| format!("open {HOST_LOCK}: {error}"))?;
    match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(error) => Err(format!("flock {HOST_LOCK}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podman_compatibility_response_fails_docker_identity_check() {
        let podman = serde_json::json!({
            "Platform": {"Name": "Podman Engine"},
            "Version": ENGINE_VERSION
        });
        assert!(validate_server_identity(&podman, ENGINE_VERSION).is_err());
        let docker = serde_json::json!({
            "Platform": {"Name": "Docker Engine - Community"},
            "Version": ENGINE_VERSION
        });
        assert!(validate_server_identity(&docker, ENGINE_VERSION).is_ok());
        assert!(validate_server_identity(&docker, "0.0.0").is_err());
    }
}
