//! Native Docker Engine smoke in a disposable dind image. Podman is only the
//! outer container runtime; every `docker` command here is the pinned
//! `/usr/local/bin/docker` client talking to the child `/usr/local/bin/dockerd`
//! over a socket created inside this container.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use diskgc_core::docker_backend::{DockerInfo, ImageStore};
use serde_json::Value;

use crate::oracle::{Fixture, Oracle};
use crate::scratch;
use crate::suite::{Ctx, TestResult};
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
        .env("DOCKER_CONFIG", "/tmp/docker-client")
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

    let tar = Command::new("/bin/tar")
        .args(["-C", "/tmp/docker-source", "-cf", "-", "payload"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("archive Docker payload: {error}"))?;
    let mut tar = tar;
    let tar_stream = tar.stdout.take().ok_or("tar has no stdout pipe")?;
    let import = Command::new(DOCKER)
        .args(["import", "-", IMAGE])
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .env("DOCKER_CONFIG", "/tmp/docker-client")
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
