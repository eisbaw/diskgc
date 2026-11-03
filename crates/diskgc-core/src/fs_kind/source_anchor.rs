//! Source evidence for a build output. A marker alone cannot establish
//! rebuildability: the recorded source must still exist outside the output.
//! This check runs in discovery and again during judgement under the
//! candidate lock, with no symlink followed at either location.

use serde::Deserialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::discover::check_deadline;
use super::judge::unreachable_root;
use crate::engine::{Identity, Reason, Skip};
use crate::host::{Access, Base, FileType, Host, MonoTime};
use crate::outcome::{Judged, Stop};
use crate::profile::SourceAnchor;
use crate::resolve::{ResolveError, Resolved, Want, open_beneath, resolve_in};

// TASK-101: build trees whose source is a whole tree beside or named by the
// output (OpenWrt, Buildroot, Yocto, Bazel); pending Mark's admission.
mod build_trees;
use build_trees::{bazel, buildroot, openwrt, yocto};

const CMAKE_CACHE: &str = "CMakeCache.txt";
const CMAKE_SOURCE_KEY: &[u8] = b"CMAKE_HOME_DIRECTORY:INTERNAL=";
const MAX_CMAKE_CACHE_BYTES: u64 = 2 * 1024 * 1024;
const MESON_INFO: &str = "meson-info/meson-info.json";
const MESON_CORE_DATA: &str = "meson-private/coredata.dat";
const MAX_MESON_INFO_BYTES: u64 = 1024 * 1024;
const AUTOTOOLS_STATUS: &str = "config.status";
const MAX_AUTOTOOLS_STATUS_BYTES: u64 = 2 * 1024 * 1024;

/// Deserialize into structs so duplicate `directories`, `source` or `build`
/// fields fail instead of letting a JSON map silently keep one value.
#[derive(Deserialize)]
struct MesonInfo {
    directories: MesonDirectories,
    error: bool,
}

#[derive(Deserialize)]
struct MesonDirectories {
    source: String,
    build: String,
    info: String,
}

/// Evidence recorded when a build output is found. The source file's
/// identity must agree at every later judgement, including under the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvidence {
    pub marker: &'static str,
    pub source: PathBuf,
    pub source_file: Identity,
}

/// Evidence the caller expects from a particular candidate. Discovery and
/// locked judgement use the same values and differ only in how they reach it.
#[derive(Clone, Copy)]
pub(super) struct AnchorCheck<'a> {
    kind: SourceAnchor,
    root: &'a Path,
    candidate: &'a Path,
    expected: Identity,
    deadline: Option<MonoTime>,
}

impl<'a> AnchorCheck<'a> {
    pub(super) fn new(
        kind: SourceAnchor,
        root: &'a Path,
        candidate: &'a Path,
        expected: Identity,
        deadline: Option<MonoTime>,
    ) -> Self {
        Self {
            kind,
            root,
            candidate,
            expected,
            deadline,
        }
    }
}

pub(super) fn check<H: Host>(
    host: &H,
    request: AnchorCheck<'_>,
    parent: &Resolved<H::Fd>,
    name: &OsStr,
) -> Judged<Option<SourceEvidence>> {
    let AnchorCheck {
        candidate,
        expected,
        deadline,
        ..
    } = request;
    check_deadline(host, deadline, candidate)?;
    let (candidate_fd, candidate_stat) = match open_beneath(
        host,
        &parent.fd,
        parent.mount.id,
        Path::new(name),
        Access::Path,
        Want::Directory,
    ) {
        Ok(opened) => opened,
        Err(error) => return Ok(Err(unreachable_build(candidate, candidate, error)?)),
    };
    if Identity::of(&candidate_stat) != Some(expected) {
        return Ok(Err(Skip::new(
            Reason::Changed,
            candidate.to_owned(),
            "the candidate changed before its source anchor was read",
        )));
    }
    check_opened(host, request, &candidate_fd, parent.mount.id)
}

/// Recheck the source through the candidate fd the caller has already
/// reached. Under reclaim this is the locked directory, not its current name.
pub(super) fn check_opened<H: Host>(
    host: &H,
    request: AnchorCheck<'_>,
    candidate_fd: &H::Fd,
    mount_id: u64,
) -> Judged<Option<SourceEvidence>> {
    let AnchorCheck {
        kind,
        root,
        candidate,
        expected,
        deadline,
    } = request;
    check_deadline(host, deadline, candidate)?;
    let stat = host
        .statx(Base::Dir(candidate_fd), Path::new(""))
        .map_err(|error| Stop::Failed {
            path: candidate.to_owned(),
            error,
        })?;
    if Identity::of(&stat) != Some(expected) {
        return Ok(Err(Skip::new(
            Reason::Changed,
            candidate.to_owned(),
            "the reached candidate is not the one whose source was discovered",
        )));
    }
    match kind {
        SourceAnchor::Cmake => cmake(host, root, candidate, candidate_fd, mount_id, deadline),
        SourceAnchor::Meson => meson(host, root, candidate, candidate_fd, mount_id, deadline),
        SourceAnchor::Autotools => {
            autotools(host, root, candidate, candidate_fd, mount_id, deadline)
        }
        SourceAnchor::Openwrt => openwrt(host, candidate, candidate_fd, mount_id, deadline),
        SourceAnchor::Buildroot => buildroot(host, candidate, candidate_fd, mount_id, deadline),
        SourceAnchor::Yocto => yocto(host, candidate, candidate_fd, mount_id, deadline),
        SourceAnchor::Bazel => bazel(host, candidate, candidate_fd, mount_id, deadline),
    }
}

/// One CMake candidate. `None` means its marker does not establish a
/// surviving source; an unsafe or unreadable path is a visible refusal.
pub(super) fn cmake<H: Host>(
    host: &H,
    root: &Path,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    let cache_path = candidate.join(CMAKE_CACHE);
    check_deadline(host, deadline, &cache_path)?;
    let (cache_fd, cache_stat) = match open_beneath(
        host,
        candidate_fd,
        mount_id,
        Path::new(CMAKE_CACHE),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &cache_path, error)?)),
    };
    if cache_stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(None));
    }
    let Some(size) = cache_stat.size else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            cache_path,
            "CMakeCache.txt size was not reported",
        )));
    };
    if size > MAX_CMAKE_CACHE_BYTES {
        return Ok(Err(Skip::new(
            Reason::TooManyEntries,
            cache_path,
            "CMakeCache.txt exceeds the 2 MiB source-anchor read limit",
        )));
    }
    let bytes = match host.read_head(&cache_fd, MAX_CMAKE_CACHE_BYTES as usize + 1) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::Unreadable,
                cache_path,
                format!("cannot read CMakeCache.txt: {error}"),
            )));
        }
    };
    if bytes.len() as u64 != size {
        return Ok(Err(Skip::new(
            Reason::Changed,
            cache_path,
            "CMakeCache.txt changed while its source anchor was read",
        )));
    }
    let Some(source) = cmake_source(&bytes, root, candidate) else {
        return Ok(Ok(None));
    };
    verify_source_file(
        host,
        candidate,
        source,
        mount_id,
        CMAKE_CACHE,
        "CMakeLists.txt",
        deadline,
    )
}

fn verify_source_file<H: Host>(
    host: &H,
    candidate: &Path,
    source: PathBuf,
    mount_id: u64,
    marker: &'static str,
    build_file: &'static str,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    check_deadline(host, deadline, &source)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::UnsupportedFs,
                source,
                format!("cannot check source mounts: {error}"),
            )));
        }
    };
    let source_dir = match resolve_in(host, &mounts, &source, Want::Directory) {
        Ok(dir) => dir,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &source, error)?)),
    };
    if source_dir.mount.id != mount_id {
        return Ok(Err(Skip::new(
            Reason::OtherFs,
            source,
            "the recorded build source is on another filesystem",
        )));
    }
    let source_file_path = source.join(build_file);
    check_deadline(host, deadline, &source_file_path)?;
    let (_, source_file_stat) = match open_beneath(
        host,
        &source_dir.fd,
        source_dir.mount.id,
        Path::new(build_file),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &source_file_path, error)?)),
    };
    if source_file_stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(None));
    }
    let Some(source_file) = Identity::of(&source_file_stat) else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            source_file_path,
            format!("{build_file} identity was not reported"),
        )));
    };
    Ok(Ok(Some(SourceEvidence {
        marker,
        source,
        source_file,
    })))
}

/// Meson's generated metadata records both the source and build roots. A
/// matching `build` path prevents a copied output from borrowing another
/// project's source evidence. The marker is read through the candidate fd,
/// including during reclaim after the directory is locked.
fn meson<H: Host>(
    host: &H,
    root: &Path,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    let core_data_path = candidate.join(MESON_CORE_DATA);
    check_deadline(host, deadline, &core_data_path)?;
    let (_, core_data_stat) = match open_beneath(
        host,
        candidate_fd,
        mount_id,
        Path::new(MESON_CORE_DATA),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &core_data_path, error)?)),
    };
    if core_data_stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(None));
    }

    let info_path = candidate.join(MESON_INFO);
    check_deadline(host, deadline, &info_path)?;
    let (info_fd, info_stat) = match open_beneath(
        host,
        candidate_fd,
        mount_id,
        Path::new(MESON_INFO),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &info_path, error)?)),
    };
    if info_stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(None));
    }
    let Some(size) = info_stat.size else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            info_path,
            "Meson info size was not reported",
        )));
    };
    if size > MAX_MESON_INFO_BYTES {
        return Ok(Err(Skip::new(
            Reason::TooManyEntries,
            info_path,
            "Meson info exceeds the 1 MiB source-anchor read limit",
        )));
    }
    let bytes = match host.read_head(&info_fd, MAX_MESON_INFO_BYTES as usize + 1) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::Unreadable,
                info_path,
                format!("cannot read Meson info: {error}"),
            )));
        }
    };
    if bytes.len() as u64 != size {
        return Ok(Err(Skip::new(
            Reason::Changed,
            info_path,
            "Meson info changed while its source anchor was read",
        )));
    }
    let info: MesonInfo = match serde_json::from_slice(&bytes) {
        Ok(info) => info,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::UnsupportedLayout,
                info_path,
                format!("invalid Meson source metadata: {error}"),
            )));
        }
    };
    if info.error {
        return Ok(Err(Skip::new(
            Reason::UnsupportedLayout,
            info_path,
            "Meson reports an incomplete configuration",
        )));
    }
    let source = PathBuf::from(info.directories.source);
    let build = PathBuf::from(info.directories.build);
    let info_dir = PathBuf::from(info.directories.info);
    if !normal_absolute(&source)
        || !source.starts_with(root)
        || source == candidate
        || !candidate.starts_with(&source)
        || !normal_absolute(&build)
        || build != candidate
        || !normal_absolute(&info_dir)
        || info_dir != candidate.join("meson-info")
    {
        return Ok(Err(Skip::new(
            Reason::UnsupportedLayout,
            info_path,
            "Meson source, build or info path does not identify this out-of-source output",
        )));
    }
    verify_source_file(
        host,
        candidate,
        source,
        mount_id,
        MESON_INFO,
        "meson.build",
        deadline,
    )
}

/// GNU Autoconf records both the current build directory (`ac_pwd`) and
/// its source directory (`srcdir`) in generated config.status. Never run the
/// shell script: read two deliberately narrow assignments as inert bytes.
fn autotools<H: Host>(
    host: &H,
    root: &Path,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    for generated in ["config.log", "Makefile"] {
        let path = candidate.join(generated);
        check_deadline(host, deadline, &path)?;
        let (_, stat) = match open_beneath(
            host,
            candidate_fd,
            mount_id,
            Path::new(generated),
            Access::Path,
            Want::Any,
        ) {
            Ok(opened) => opened,
            Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
            Err(error) => return Ok(Err(unreachable_build(candidate, &path, error)?)),
        };
        if stat.file_type != Some(FileType::Regular) {
            return Ok(Ok(None));
        }
    }

    let status_path = candidate.join(AUTOTOOLS_STATUS);
    check_deadline(host, deadline, &status_path)?;
    let (status_fd, status_stat) = match open_beneath(
        host,
        candidate_fd,
        mount_id,
        Path::new(AUTOTOOLS_STATUS),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(unreachable_build(candidate, &status_path, error)?)),
    };
    if status_stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(None));
    }
    let Some(size) = status_stat.size else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            status_path,
            "config.status size was not reported",
        )));
    };
    if size > MAX_AUTOTOOLS_STATUS_BYTES {
        return Ok(Err(Skip::new(
            Reason::TooManyEntries,
            status_path,
            "config.status exceeds the 2 MiB source-anchor read limit",
        )));
    }
    let bytes = match host.read_head(&status_fd, MAX_AUTOTOOLS_STATUS_BYTES as usize + 1) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::Unreadable,
                status_path,
                format!("cannot read config.status: {error}"),
            )));
        }
    };
    if bytes.len() as u64 != size {
        return Ok(Err(Skip::new(
            Reason::Changed,
            status_path,
            "config.status changed while its source anchor was read",
        )));
    }
    let Some(source) = autotools_source(&bytes, root, candidate) else {
        return Ok(Ok(None));
    };
    verify_source_file(
        host,
        candidate,
        source,
        mount_id,
        AUTOTOOLS_STATUS,
        "configure.ac",
        deadline,
    )
}

fn autotools_source(bytes: &[u8], root: &Path, candidate: &Path) -> Option<PathBuf> {
    // Autoconf 2.73 emits these as single-quoted shell assignments. Unusual
    // shell escaping is refused, so parsing can never reinterpret script code.
    fn assignment<'a>(bytes: &'a [u8], key: &[u8]) -> Option<&'a str> {
        let mut found = None;
        let assignment_prefix = &key[..key.len() - 1];
        for line in bytes.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.starts_with(assignment_prefix) && !line.starts_with(key) {
                return None;
            }
            if let Some(value) = line.strip_prefix(key) {
                if found.is_some() || !value.ends_with(b"'") {
                    return None;
                }
                let value = &value[..value.len() - 1];
                if value.contains(&b'\'') || value.contains(&0) {
                    return None;
                }
                found = Some(std::str::from_utf8(value).ok()?);
            }
        }
        found
    }

    if assignment(bytes, b"ac_pwd='")? != candidate.to_str()? {
        return None;
    }
    let srcdir = assignment(bytes, b"srcdir='")?;
    let source = if srcdir.starts_with('/') {
        PathBuf::from(srcdir)
    } else {
        let mut source = candidate.to_owned();
        for segment in srcdir.split('/') {
            if segment != ".." {
                return None;
            }
            source = source.parent()?.to_owned();
        }
        source
    };
    if !normal_absolute(&source)
        || !source.starts_with(root)
        || source == candidate
        || !candidate.starts_with(&source)
    {
        return None;
    }
    Some(source)
}

fn cmake_source(bytes: &[u8], root: &Path, candidate: &Path) -> Option<PathBuf> {
    let mut source = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(path) = line.strip_prefix(CMAKE_SOURCE_KEY) {
            if source.is_some() {
                return None;
            }
            let path = std::str::from_utf8(path).ok()?;
            let path = PathBuf::from(path);
            if !normal_absolute(&path)
                || !path.starts_with(root)
                || path == candidate
                || !candidate.starts_with(&path)
            {
                return None;
            }
            source = Some(path);
        }
    }
    source
}

fn normal_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.first() == Some(&b'/')
        && !bytes.contains(&0)
        && bytes[1..]
            .split(|byte| *byte == b'/')
            .all(|segment| !segment.is_empty() && segment != b"." && segment != b"..")
}

fn unreachable_build(
    candidate: &Path,
    at: &Path,
    error: ResolveError,
) -> Result<Skip, crate::outcome::Stop> {
    let skip = unreachable_root(at, error)?;
    Ok(Skip::new(
        skip.reason,
        candidate.to_owned(),
        format!(
            "cannot verify build source at {}: {}",
            at.display(),
            skip.detail
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_normal_ancestor_source_is_accepted() {
        let root = Path::new("/projects");
        let build = Path::new("/projects/app/output");
        let source = |value: &str| {
            cmake_source(
                format!("CMAKE_HOME_DIRECTORY:INTERNAL={value}\n").as_bytes(),
                root,
                build,
            )
        };
        assert_eq!(
            source("/projects/app"),
            Some(PathBuf::from("/projects/app"))
        );
        for invalid in [
            "/projects/app/output",
            "/projects/other",
            "/elsewhere/app",
            "/projects/app/../app",
            "/projects//app",
            "projects/app",
        ] {
            assert_eq!(source(invalid), None, "{invalid}");
        }
        assert_eq!(
            cmake_source(
                b"CMAKE_HOME_DIRECTORY:INTERNAL=/projects/app\nCMAKE_HOME_DIRECTORY:INTERNAL=/projects/app\n",
                root,
                build,
            ),
            None
        );
    }

    #[test]
    fn autotools_requires_exact_build_path_and_strict_ancestor_source() {
        let root = Path::new("/projects");
        let build = Path::new("/projects/app/output");
        let status = |pwd: &str, srcdir: &str| {
            autotools_source(
                format!("ac_pwd='{pwd}'\nsrcdir='{srcdir}'\n").as_bytes(),
                root,
                build,
            )
        };
        assert_eq!(status("/projects/app/output", ".."), Some(root.join("app")));
        for (pwd, srcdir) in [
            ("/projects/app/other", ".."),
            ("/projects/app/output", "."),
            ("/projects/app/output", "../other"),
            ("/projects/app/output", "../../.."),
            ("/projects/app/output", "/projects/other"),
            ("/projects/app/output", "/projects/app/output"),
            ("/projects/app/output", "/projects/app/../app"),
        ] {
            assert_eq!(status(pwd, srcdir), None, "{pwd} {srcdir}");
        }
        assert_eq!(
            autotools_source(
                b"ac_pwd='/projects/app/output'\nac_pwd='/projects/app/output'\nsrcdir='..'\n",
                root,
                build
            ),
            None,
        );
        assert_eq!(
            autotools_source(
                b"ac_pwd='/projects/app/output'\nac_pwd=other\nsrcdir='..'\n",
                root,
                build
            ),
            None,
        );
    }
}
