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

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Marker file baked into the e2e base image.
pub const MARKER_PATH: &str = "/.diskgc-test-sandbox";

/// Marker Podman creates in every container it starts.
pub const CONTAINERENV_PATH: &str = "/run/.containerenv";

/// Exact bytes the image marker must hold.
pub const MARKER_CONTENT: &[u8] = include_bytes!("sandbox-marker");

/// Why the current root filesystem is not a diskgc test sandbox.
#[derive(Debug)]
pub enum NotInSandbox {
    /// The image marker cannot be read (usually: it does not exist).
    MarkerUnreadable { path: PathBuf, error: io::Error },
    /// The image marker exists but holds other bytes than [`MARKER_CONTENT`]
    /// (a marker from another tool, or a stale image).
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
            Self::MarkerMismatch { path, len } => write!(
                f,
                "sandbox marker {} holds {len} bytes that do not match the constant compiled \
                 into this binary (stale or foreign image)",
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

/// [`check`] with the marker paths taken relative to `root`, so the decision
/// can be tested without a container.
fn check_under(root: &Path) -> Result<(), NotInSandbox> {
    let marker = root.join(MARKER_PATH.trim_start_matches('/'));
    let content = std::fs::read(&marker).map_err(|error| NotInSandbox::MarkerUnreadable {
        path: marker.clone(),
        error,
    })?;
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

    #[test]
    fn missing_containerenv_refuses() {
        let root = TempRoot::new("no-containerenv");
        root.write(".diskgc-test-sandbox", MARKER_CONTENT);
        assert!(matches!(
            check_under(&root.0),
            Err(NotInSandbox::ContainerEnvMissing { .. })
        ));
    }
}
