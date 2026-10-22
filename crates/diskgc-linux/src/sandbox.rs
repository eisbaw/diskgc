//! Markers of the rootless test sandbox.
//!
//! One definition serves two checks that must agree: the run-time interlock
//! of the destructive e2e tests (docs/testing.md, Safety interlock, layer 2)
//! and the pre-release sandbox gate of the deleting primitives
//! (docs/safety.md, Pre-release sandbox gate). Both require the image marker
//! `/.diskgc-test-sandbox`, holding exactly [`MARKER_CONTENT`], and Podman's
//! `/run/.containerenv`.
//!
//! [`MARKER_CONTENT`] is the file `sandbox-marker` next to this module; the
//! flake copies that same file byte for byte into the e2e base image, so the
//! constant has a single source.
//!
//! The gate is [`permit`]: every deleting syscall wrapper of this crate
//! takes the [`Permit`] it returns, and nothing else can make one. Building
//! with the cargo feature `release-without-sandbox-gate` (defined in this
//! crate's Cargo.toml, enabled only by release packages, 0.1 onwards) makes
//! [`permit`] always grant; nothing else removes the gate.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Marker file baked into the e2e base image.
pub const MARKER_PATH: &str = "/.diskgc-test-sandbox";

/// Marker Podman creates in every container it starts.
pub const CONTAINERENV_PATH: &str = "/run/.containerenv";

/// Exact bytes the image marker must hold.
pub const MARKER_CONTENT: &[u8] = include_bytes!("sandbox-marker");

/// A test-only wall-clock advance, written inside a disposable test image.
/// This name and its reader are omitted from feature-built release binaries.
#[cfg(not(feature = "release-without-sandbox-gate"))]
pub const CLOCK_OFFSET_PATH: &str = "/.diskgc-test-clock-offset";

#[cfg(not(feature = "release-without-sandbox-gate"))]
const MAX_CLOCK_OFFSET_SECONDS: u64 = 366 * 24 * 60 * 60;

/// Read a fixed positive number of seconds only after both sandbox markers
/// have passed. A missing file means no offset; an existing but invalid file
/// is an error, so it cannot silently change a deletion decision. The file
/// is opened without following a final symlink or blocking on a FIFO.
#[cfg(not(feature = "release-without-sandbox-gate"))]
pub(crate) fn clock_offset() -> Result<u64, String> {
    clock_offset_under(Path::new("/"))
}

#[cfg(not(feature = "release-without-sandbox-gate"))]
fn clock_offset_under(root: &Path) -> Result<u64, String> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Read;

    // The offset is meaningless outside the marked test image, including
    // when a copied offset file happens to exist on a development host.
    if check_under(root).is_err() {
        return Ok(0);
    }
    let path = root.join(CLOCK_OFFSET_PATH.trim_start_matches('/'));
    let fd = match rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(0),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let stat =
        rustix::fs::fstat(&fd).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    // Read one byte past the accepted bound. Stopping exactly at the bound
    // would accept a valid prefix followed by arbitrary trailing bytes.
    std::fs::File::from(fd)
        .take(13)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() > 12 {
        return Err(format!("{} exceeds 12 bytes", path.display()));
    }
    let Some(digits) = bytes.strip_suffix(b"\n") else {
        return Err(format!(
            "{} must contain decimal seconds and one newline",
            path.display()
        ));
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(format!(
            "{} must contain decimal seconds and one newline",
            path.display()
        ));
    }
    let seconds = std::str::from_utf8(digits)
        .expect("ASCII digits")
        .parse::<u64>()
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if seconds == 0 || seconds > MAX_CLOCK_OFFSET_SECONDS {
        return Err(format!(
            "{} must be between 1 and {MAX_CLOCK_OFFSET_SECONDS} seconds",
            path.display()
        ));
    }
    Ok(seconds)
}

/// Why the current root filesystem is not a diskgc test sandbox.
#[derive(Debug)]
pub enum NotInSandbox {
    /// The image marker cannot be read (usually: it does not exist).
    MarkerUnreadable { path: PathBuf, error: io::Error },
    /// The image marker is not a regular file (a FIFO, a device, a
    /// directory): something else stands at its path.
    MarkerNotRegular { path: PathBuf },
    /// The image marker exists but holds other bytes than [`MARKER_CONTENT`]
    /// (a marker from another tool, or a stale image). `len` is how many
    /// bytes were read: at most one more than the constant.
    MarkerMismatch { path: PathBuf, len: usize },
    /// Podman's container marker is missing.
    ContainerEnvMissing { path: PathBuf, error: io::Error },
}

impl fmt::Display for NotInSandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MarkerUnreadable { path, error } => {
                write!(f, "sandbox marker {} unreadable: {error}", path.display())
            }
            Self::MarkerNotRegular { path } => {
                write!(f, "sandbox marker {} is not a regular file", path.display())
            }
            Self::MarkerMismatch { path, len } => write!(
                f,
                "sandbox marker {} does not hold exactly the constant compiled into this \
                 binary ({len} bytes read; stale or foreign image)",
                path.display()
            ),
            Self::ContainerEnvMissing { path, error } => {
                write!(f, "container marker {} missing: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for NotInSandbox {}

/// Checks that this process runs inside a diskgc test container.
pub fn check() -> Result<(), NotInSandbox> {
    check_under(Path::new("/"))
}

/// Proof that the pre-release sandbox gate allows deleting in this process.
/// Only [`permit`] makes one (the field is private to this module), and the
/// deleting syscall wrappers require one, so a deleting method cannot skip
/// the gate (docs/safety.md, Pre-release sandbox gate).
#[derive(Debug)]
pub(crate) struct Permit(());

/// The pre-release sandbox gate: a [`Permit`] inside a diskgc test container
/// ([`check`]), otherwise why not (which marker is missing or wrong).
///
/// Decided once per process, at the first deleting call, and kept: the
/// markers are part of the container image and of Podman's setup, and do not
/// change while diskgc runs, so re-reading two files before every one of
/// thousands of unlinks would only add syscalls (and strace noise in the e2e
/// tests). A refusal is kept too, so a process never deletes after having
/// been refused.
pub(crate) fn permit() -> Result<Permit, &'static str> {
    status().map(|()| Permit(()))
}

/// The gate's decision without a [`Permit`]: what [`permit`] answers, for
/// a caller that only needs to know (a reclaim that must fail fast before
/// it measures, locks or plans; task-50 review H4). The same cached
/// decision: asking first does not change what a later deleting call is
/// told.
#[cfg(not(feature = "release-without-sandbox-gate"))]
pub fn status() -> Result<(), &'static str> {
    static DECISION: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    match DECISION.get_or_init(|| check().map_err(|why| why.to_string())) {
        Ok(()) => Ok(()),
        Err(why) => Err(why),
    }
}

/// A release build (0.1 onwards): the gate is compiled out and every call is
/// permitted. Only the release package enables this feature.
#[cfg(feature = "release-without-sandbox-gate")]
pub fn status() -> Result<(), &'static str> {
    Ok(())
}

/// [`check`] with the marker paths taken relative to `root`, so the decision
/// can be tested without a container.
fn check_under(root: &Path) -> Result<(), NotInSandbox> {
    let marker = root.join(MARKER_PATH.trim_start_matches('/'));
    let content = read_marker(&marker)?;
    if content != MARKER_CONTENT {
        return Err(NotInSandbox::MarkerMismatch {
            path: marker,
            len: content.len(),
        });
    }
    let containerenv = root.join(CONTAINERENV_PATH.trim_start_matches('/'));
    std::fs::symlink_metadata(&containerenv).map_err(|error| {
        NotInSandbox::ContainerEnvMissing {
            path: containerenv,
            error,
        }
    })?;
    Ok(())
}

/// Reads the image marker at `path`: a regular file reached without
/// following a final symlink (`O_NOFOLLOW`), opened without blocking (a FIFO
/// at the path must not hang the check), and at most one byte more than
/// [`MARKER_CONTENT`] (enough to tell a longer file apart, never a large
/// read).
fn read_marker(path: &Path) -> Result<Vec<u8>, NotInSandbox> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Read;
    let unreadable = |error: io::Error| NotInSandbox::MarkerUnreadable {
        path: path.to_owned(),
        error,
    };
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| unreadable(e.into()))?;
    let stat = rustix::fs::fstat(&fd).map_err(|e| unreadable(e.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(NotInSandbox::MarkerNotRegular {
            path: path.to_owned(),
        });
    }
    let limit = MARKER_CONTENT.len() as u64 + 1;
    let mut content = Vec::new();
    std::fs::File::from(fd)
        .take(limit)
        .read_to_end(&mut content)
        .map_err(unreadable)?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "diskgc-linux-sandbox-test-{}-{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(dir.join("run")).expect("create temp root");
            Self(dir)
        }
        fn write(&self, rel: &str, content: &[u8]) {
            std::fs::write(self.0.join(rel), content).expect("write marker");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The cargo feature that removes the gate.
    const RELEASE_FLAG: &str = "release-without-sandbox-gate";

    /// Cargo reads `.cargo/config` or `.cargo/config.toml` in the working
    /// directory and its ancestors. Search the checkout tree so a command run
    /// from a nested crate cannot silently enable the release-only feature.
    fn cargo_configs_under(repo: &Path) -> Vec<PathBuf> {
        let mut pending = vec![PathBuf::new()];
        let mut configs = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(repo.join(&dir))
                .unwrap_or_else(|e| panic!("list {}: {e}", repo.join(&dir).display()))
            {
                let entry = entry.expect("directory entry");
                let file_type = entry.file_type().expect("entry type");
                let name = entry.file_name();
                let child = dir.join(&name);
                if name == ".cargo" && (file_type.is_dir() || file_type.is_symlink()) {
                    // Cargo follows a symlinked `.cargo` directory. Inspect
                    // only its two config filenames; do not recursively walk
                    // an arbitrary symlink target outside the checkout.
                    for name in ["config.toml", "config"] {
                        let rel = child.join(name);
                        if repo.join(&rel).is_file() {
                            configs.push(rel);
                        }
                    }
                }
                if !file_type.is_dir()
                    || name == ".git"
                    || name == "target"
                    || (dir.as_os_str().is_empty() && name == "tmp")
                {
                    continue; // No symlink recursion or generated output.
                }
                pending.push(child);
            }
        }
        configs.sort();
        configs
    }

    fn gate_enablers(repo: &Path, scanned: &[PathBuf]) -> Vec<String> {
        let mut found = Vec::new();
        for rel in scanned {
            let text = std::fs::read_to_string(repo.join(rel))
                .unwrap_or_else(|e| panic!("read {}: {e}", rel.display()));
            for (n, line) in text.lines().enumerate() {
                if line.contains(RELEASE_FLAG) || line.contains("all-features") {
                    found.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
                }
            }
        }
        found
    }

    /// AC #3 of task-50, the compiled half: this test build (`just check`,
    /// and the flake's workspace-tests on the musl target with the features
    /// the manifests give) has the gate.
    #[test]
    // The constant is the point: it is what the build configuration decided.
    #[allow(clippy::assertions_on_constants)]
    fn the_gate_is_compiled_into_this_build() {
        assert!(
            !cfg!(feature = "release-without-sandbox-gate"),
            "this build removed the pre-release sandbox gate"
        );
    }

    /// AC #3 of task-50, the configuration half: the feature that removes the
    /// gate is defined once, in diskgc-linux's manifest, is not a default
    /// feature, and only the dedicated feature-built exclusion control in
    /// the flake enables it. The default package, checks, `just` and plain
    /// `cargo` all build with the gate. TASK-67 must change this test when
    /// it enables the feature in the actual release package.
    ///
    /// flake.nix, the justfile and diskgc-e2e's manifest are not part of the
    /// flake's filtered build source, so the flake's workspace-tests cannot
    /// scan them (the test prints which, visible with `--nocapture`); in a
    /// git checkout they must be present and are scanned, so `just check`,
    /// which gates every commit, covers them.
    #[test]
    fn sandbox_gate_flag_is_enabled_nowhere() {
        let flag = RELEASE_FLAG;
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = crate_dir.join("../..");
        let checkout = repo.join(".git").exists();

        // The definition: exactly one line naming the flag, and it is that.
        let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("manifest");
        let naming: Vec<&str> = manifest
            .lines()
            .filter(|l| l.contains(flag) && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(
            naming,
            [format!("{flag} = []").as_str()],
            "diskgc-linux/Cargo.toml must define the flag once and enable it nowhere \
             (not in `default`)"
        );

        let mut scanned = vec![PathBuf::from("Cargo.toml")];
        for entry in std::fs::read_dir(repo.join("crates")).expect("list crates") {
            let manifest = entry.expect("crate entry").path().join("Cargo.toml");
            let rel = manifest.strip_prefix(&repo).expect("below repo").to_owned();
            if manifest.exists() && rel != Path::new("crates/diskgc-linux/Cargo.toml") {
                scanned.push(rel);
            }
        }
        assert!(
            scanned.contains(&PathBuf::from("crates/diskgc/Cargo.toml")),
            "the binary's manifest was not found: {scanned:?}"
        );
        let mut not_seen = Vec::new();
        for rel in ["diskgc-e2e/Cargo.toml", "flake.nix", "justfile"] {
            if repo.join(rel).exists() {
                scanned.push(rel.into());
            } else {
                assert!(
                    !checkout,
                    "{rel} missing from the git checkout {}",
                    repo.display()
                );
                not_seen.push(rel);
            }
        }
        scanned.extend(cargo_configs_under(&repo));

        let found = gate_enablers(&repo, &scanned);
        let allowed_control = "cargoExtraArgs = \"--locked --package diskgc --features diskgc-linux/release-without-sandbox-gate\";";
        let unexpected: Vec<&String> = found
            .iter()
            .filter(|line| !line.starts_with("flake.nix:") || !line.ends_with(allowed_control))
            .collect();
        assert!(
            unexpected.is_empty() && found.len() <= 1,
            "the pre-release sandbox gate has an unexpected build enabler:\n{}",
            found.join("\n")
        );
        if checkout {
            assert_eq!(found.len(), 1, "the feature-built control vanished");
            let flake = std::fs::read_to_string(repo.join("flake.nix")).expect("flake");
            let control = flake
                .split_once("diskgcWithoutGate = craneLib.buildPackage")
                .expect("the feature-built control")
                .1
                .split_once("      );")
                .expect("the end of the control build")
                .0;
            assert!(
                control.contains(allowed_control),
                "only the named control may enable the feature"
            );
        }
        if !not_seen.is_empty() {
            println!(
                "NOT SCANNED here (not in this source tree; `just check` scans them in a git \
                 checkout): {not_seen:?}"
            );
        }
    }

    #[test]
    fn nested_cargo_config_enabling_release_feature_is_detected() {
        let root = TempRoot::new("nested-cargo-config");
        let cargo_dir = root.0.join("crates/example/.cargo");
        std::fs::create_dir_all(&cargo_dir).expect("create nested Cargo config directory");
        root.write(
            "crates/example/.cargo/config.toml",
            b"[build]\nrustflags = ['--cfg', 'feature=\"release-without-sandbox-gate\"']\n",
        );
        let alias_dir = root.0.join("crates/another/.cargo");
        std::fs::create_dir_all(&alias_dir).expect("create nested Cargo alias directory");
        root.write(
            "crates/another/.cargo/config",
            b"[alias]\ncheck-all = 'check --all-features'\n",
        );
        let configs = cargo_configs_under(&root.0);
        let found = gate_enablers(&root.0, &configs);
        assert_eq!(found.len(), 2, "nested configs must fail the gate scan");
        assert!(
            found
                .iter()
                .any(|line| line.starts_with("crates/example/.cargo/config.toml:2:")),
            "nested rustflags location must be reported: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|line| line.starts_with("crates/another/.cargo/config:2:")),
            "nested alias location must be reported: {found:?}"
        );
    }

    #[test]
    fn generated_root_tmp_is_excluded_from_cargo_config_scan() {
        let root = TempRoot::new("generated-tmp-scan");
        std::fs::create_dir_all(root.0.join("tmp/system/.cargo"))
            .expect("create generated scratch directory");
        root.write("tmp/system/.cargo/config.toml", b"[build]\n");
        std::fs::create_dir_all(root.0.join("crates/example/.cargo"))
            .expect("create tracked config directory");
        root.write("crates/example/.cargo/config.toml", b"[build]\n");

        assert_eq!(
            cargo_configs_under(&root.0),
            vec![PathBuf::from("crates/example/.cargo/config.toml")]
        );
    }

    #[test]
    fn symlinked_cargo_config_enabling_release_feature_is_detected() {
        use std::os::unix::fs::symlink;

        let root = TempRoot::new("symlinked-cargo-config");
        let linked_config = root.0.join("shared-cargo");
        std::fs::create_dir_all(&linked_config).expect("create linked Cargo config directory");
        root.write(
            "shared-cargo/config.toml",
            b"[build]\nrustflags = ['--cfg', 'feature=\"release-without-sandbox-gate\"']\n",
        );
        let crate_dir = root.0.join("crates/example");
        std::fs::create_dir_all(&crate_dir).expect("create crate directory");
        symlink(&linked_config, crate_dir.join(".cargo")).expect("link Cargo config directory");

        let found = gate_enablers(&root.0, &cargo_configs_under(&root.0));
        assert_eq!(found.len(), 1, "symlinked config must fail the gate scan");
        assert!(
            found[0].starts_with("crates/example/.cargo/config.toml:2:"),
            "symlinked Cargo config location must be reported: {found:?}"
        );
    }

    #[test]
    fn marker_content_is_non_trivial() {
        // An empty or whitespace marker would match files created by accident.
        assert!(MARKER_CONTENT.len() > 32);
        assert!(MARKER_CONTENT.starts_with(b"diskgc-test-sandbox/"));
    }

    #[test]
    fn both_markers_with_the_right_content_pass() {
        let root = TempRoot::new("ok");
        root.write(".diskgc-test-sandbox", MARKER_CONTENT);
        root.write("run/.containerenv", b"");
        check_under(&root.0).expect("both markers present");
    }

    #[test]
    fn missing_image_marker_refuses() {
        let root = TempRoot::new("no-marker");
        root.write("run/.containerenv", b"");
        assert!(matches!(
            check_under(&root.0),
            Err(NotInSandbox::MarkerUnreadable { .. })
        ));
    }

    #[test]
    fn wrong_marker_content_refuses() {
        let root = TempRoot::new("wrong-marker");
        root.write(".diskgc-test-sandbox", b"something else\n");
        root.write("run/.containerenv", b"");
        assert!(matches!(
            check_under(&root.0),
            Err(NotInSandbox::MarkerMismatch { .. })
        ));
    }

    /// Near misses of the marker content are refused: an exact comparison,
    /// not a trimmed or prefix one.
    #[test]
    fn near_miss_marker_contents_refuse() {
        let without_newline = MARKER_CONTENT
            .strip_suffix(b"\n")
            .expect("ends in a newline");
        let extra_line = [MARKER_CONTENT, b"more\n"].concat();
        let prefix = b"diskgc-test-sandbox/";
        assert!(MARKER_CONTENT.starts_with(prefix));
        let other_uuid: Vec<u8> = MARKER_CONTENT
            .iter()
            .enumerate()
            .map(|(i, &b)| match b {
                b'0'..=b'9' if i >= prefix.len() => b'0' + (b - b'0' + 1) % 10,
                b'a'..=b'f' if i >= prefix.len() => b'a' + (b - b'a' + 1) % 6,
                _ => b,
            })
            .collect();
        assert_ne!(other_uuid, MARKER_CONTENT);
        assert_eq!(other_uuid.len(), MARKER_CONTENT.len());
        for (name, content) in [
            ("no-newline", without_newline),
            ("extra-line", &extra_line[..]),
            ("empty", &b""[..]),
            ("other-uuid", &other_uuid[..]),
        ] {
            let root = TempRoot::new(name);
            root.write(".diskgc-test-sandbox", content);
            root.write("run/.containerenv", b"");
            assert!(
                matches!(
                    check_under(&root.0),
                    Err(NotInSandbox::MarkerMismatch { .. })
                ),
                "{name}: {:?}",
                check_under(&root.0)
            );
        }
    }

    /// A symlink at the marker path is not followed, even to a correct
    /// marker.
    #[test]
    fn a_symlinked_marker_refuses() {
        let root = TempRoot::new("symlink-marker");
        root.write("real-marker", MARKER_CONTENT);
        std::os::unix::fs::symlink(
            root.0.join("real-marker"),
            root.0.join(".diskgc-test-sandbox"),
        )
        .expect("symlink");
        root.write("run/.containerenv", b"");
        let err = check_under(&root.0).expect_err("symlink followed");
        assert!(
            matches!(&err, NotInSandbox::MarkerUnreadable { error, .. }
                if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())),
            "{err}"
        );
    }

    /// A FIFO at the marker path is refused at once, without waiting for a
    /// writer, as not a regular file.
    #[test]
    fn a_fifo_marker_refuses_without_blocking() {
        let root = TempRoot::new("fifo-marker");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            root.0.join(".diskgc-test-sandbox"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        root.write("run/.containerenv", b"");
        let dir = root.0.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // A blocking open would never return; the thread is then left behind.
        std::thread::spawn(move || {
            let _ = tx.send(check_under(&dir).map_err(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the check blocked on a FIFO");
        assert!(
            matches!(&got, Err(why) if why.contains("not a regular file")),
            "{got:?}"
        );
    }

    /// The read is bounded: a huge file is not read whole.
    #[test]
    fn the_marker_read_is_bounded() {
        let root = TempRoot::new("big-marker");
        let big = [MARKER_CONTENT, &vec![b'x'; 1 << 20][..]].concat();
        root.write(".diskgc-test-sandbox", &big);
        root.write("run/.containerenv", b"");
        match check_under(&root.0) {
            Err(NotInSandbox::MarkerMismatch { len, .. }) => {
                assert_eq!(len, MARKER_CONTENT.len() + 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_containerenv_refuses() {
        let root = TempRoot::new("no-containerenv");
        root.write(".diskgc-test-sandbox", MARKER_CONTENT);
        assert!(matches!(
            check_under(&root.0),
            Err(NotInSandbox::ContainerEnvMissing { .. })
        ));
    }

    #[cfg(not(feature = "release-without-sandbox-gate"))]
    #[test]
    fn clock_offset_requires_both_markers_and_valid_bounded_file() {
        let root = TempRoot::new("clock-offset");
        root.write(".diskgc-test-clock-offset", b"172800\n");
        assert_eq!(clock_offset_under(&root.0).unwrap(), 0);
        root.write(".diskgc-test-sandbox", MARKER_CONTENT);
        assert_eq!(clock_offset_under(&root.0).unwrap(), 0);
        root.write("run/.containerenv", b"");
        assert_eq!(clock_offset_under(&root.0).unwrap(), 172800);

        for bad in [
            b"0\n".as_slice(),
            b"-1\n",
            b"36600000\n",
            b"2 days\n",
            b"1",
            b"1\n\n",
            b"00000000001\nX",
            b"00000000001\nXtrailing",
        ] {
            root.write(".diskgc-test-clock-offset", bad);
            assert!(clock_offset_under(&root.0).is_err(), "accepted {bad:?}");
        }
        std::fs::remove_file(root.0.join(".diskgc-test-clock-offset")).unwrap();
        assert_eq!(clock_offset_under(&root.0).unwrap(), 0);
    }

    #[cfg(not(feature = "release-without-sandbox-gate"))]
    #[test]
    fn clock_offset_refuses_symlink_and_fifo_without_blocking() {
        let root = TempRoot::new("clock-offset-special");
        root.write(".diskgc-test-sandbox", MARKER_CONTENT);
        root.write("run/.containerenv", b"");
        root.write("target", b"172800\n");
        let offset = root.0.join(".diskgc-test-clock-offset");
        std::os::unix::fs::symlink(root.0.join("target"), &offset).unwrap();
        assert!(clock_offset_under(&root.0).is_err());
        std::fs::remove_file(&offset).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &offset,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let dir = root.0.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(clock_offset_under(&dir)).unwrap());
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
    }
}
