//! Source anchors for build systems whose output sits in its source tree
//! (Buildroot `output/`, a Yocto TMPDIR) or names it (a Bazel output base).
//! Each reads only inert generated files, through the candidate fd, and
//! never runs the tool. The tree the output would be rebuilt from must still
//! hold its build description on the output's filesystem; the trees it also
//! builds from (Yocto layers) must survive outside the output, and a
//! Buildroot output that names a br2-external tree, which only the output
//! records, is kept. Missing or ambiguous evidence keeps the output (`Ok(None)`); an
//! unsafe or unreadable path is a visible refusal. TASK-101, pending Mark's
//! admission.

use std::path::{Path, PathBuf};

use super::{SourceEvidence, normal_absolute, unreachable_build};
use crate::engine::{Identity, Reason, Skip};
use crate::fs_kind::discover::check_deadline;
use crate::host::{Access, FileType, Host, MonoTime, Stat};
use crate::outcome::Judged;
use crate::resolve::{ResolveError, Resolved, Want, open_beneath, resolve_in};

/// Buildroot's default `O=$(CURDIR)/output`, whose `.config` stays in the
/// source top (Makefile `CONFIG_DIR`). An output elsewhere holds its own
/// `.config` and is never a candidate.
const BUILDROOT_OUTPUT: &str = "output";
pub(super) const BUILDROOT_MARKER: &str = ".br2-external.mk";
const MAX_BR2_EXTERNAL_BYTES: u64 = 1024 * 1024;
const BUILDROOT_BUILD_FILE: &str = "Config.in";
const BUILDROOT_TREE: &[&str] = &[
    "Makefile",
    ".config",
    "package/Config.in",
    "support/scripts/br2-external",
];

/// OE-core's sanity check writes TMPDIR's own path here (meta/lib/oe/
/// sanity.py) and refuses to build in a TMPDIR that moved.
pub(super) const YOCTO_MARKER: &str = "saved_tmpdir";
const MAX_SAVED_TMPDIR_BYTES: u64 = 4096;
const YOCTO_ABI: &str = "abi_version";
const YOCTO_LAYERS: &str = "conf/bblayers.conf";
const MAX_BBLAYERS_BYTES: u64 = 1024 * 1024;
const YOCTO_TREE: &[&str] = &["conf/local.conf"];
const YOCTO_LAYER_CONF: &str = "conf/layer.conf";

/// Bazel writes the workspace path, without a newline, here
/// (BlazeWorkspace.writeDoNotBuildHereFile).
pub(super) const BAZEL_MARKER: &str = "DO_NOT_BUILD_HERE";
const MAX_BAZEL_MARKER_BYTES: u64 = 4096;
/// The files that make a directory a Bazel workspace root, in Bazel's order
/// of preference; the first present one is the recorded identity.
const BAZEL_BOUNDARY: &[&str] = &["MODULE.bazel", "REPO.bazel", "WORKSPACE.bazel", "WORKSPACE"];

pub(super) fn buildroot<H: Host>(
    host: &H,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    if !named(candidate, BUILDROOT_OUTPUT) {
        return Ok(Ok(None));
    }
    // An output holding .config is an O= build: the configuration exists
    // only there.
    if judged!(entry(
        host,
        candidate,
        candidate_fd,
        candidate,
        mount_id,
        ".config",
        deadline
    ))
    .is_some()
    {
        return Ok(Ok(None));
    }
    let Some((bytes, _)) = judged!(read_regular(
        host,
        candidate,
        candidate_fd,
        candidate,
        mount_id,
        BUILDROOT_MARKER,
        MAX_BR2_EXTERNAL_BYTES,
        deadline
    )) else {
        return Ok(Ok(None));
    };
    let Some(externals) = br2_externals(&bytes) else {
        return Ok(Ok(None));
    };
    // Buildroot records BR2_EXTERNAL only in this generated file. Once the
    // output is gone, the next `make` without `BR2_EXTERNAL=` rewrites the
    // top-level .config without any external tree's options: the user's
    // configuration, lost (TASK-354, seen with Buildroot 2026.08.1). An
    // output that names an external tree is kept.
    if !externals.is_empty() {
        return Ok(Ok(None));
    }
    let Some(top) = candidate.parent() else {
        return Ok(Ok(None));
    };
    source_tree(
        host,
        candidate,
        top,
        mount_id,
        BUILDROOT_MARKER,
        &[BUILDROOT_BUILD_FILE],
        BUILDROOT_TREE,
        deadline,
    )
}

pub(super) fn yocto<H: Host>(
    host: &H,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    let Some((saved, _)) = judged!(read_regular(
        host,
        candidate,
        candidate_fd,
        candidate,
        mount_id,
        YOCTO_MARKER,
        MAX_SAVED_TMPDIR_BYTES,
        deadline
    )) else {
        return Ok(Ok(None));
    };
    // A copied or moved TMPDIR records its old path; bitbake refuses it.
    let recorded = saved.strip_suffix(b"\n").unwrap_or(&saved);
    if recorded != candidate.as_os_str().as_encoded_bytes() {
        return Ok(Ok(None));
    }
    if judged!(regular_file(
        host,
        candidate,
        candidate_fd,
        candidate,
        mount_id,
        YOCTO_ABI,
        deadline
    ))
    .is_none()
    {
        return Ok(Ok(None));
    }
    // TOPDIR, the build directory: TMPDIR's parent by default. A TMPDIR
    // configured elsewhere has no conf/ beside it and is kept.
    let Some(topdir) = candidate.parent() else {
        return Ok(Ok(None));
    };
    let Some(dir) = judged!(open_source_dir(host, candidate, topdir, mount_id, deadline)) else {
        return Ok(Ok(None));
    };
    if !outside(candidate, topdir, YOCTO_LAYERS)
        || YOCTO_TREE
            .iter()
            .any(|file| !outside(candidate, topdir, file))
    {
        return Ok(Ok(None));
    }
    for file in YOCTO_TREE {
        if judged!(regular_file(
            host,
            candidate,
            &dir.fd,
            topdir,
            dir.mount.id,
            file,
            deadline
        ))
        .is_none()
        {
            return Ok(Ok(None));
        }
    }
    let Some((bytes, stat)) = judged!(read_regular(
        host,
        candidate,
        &dir.fd,
        topdir,
        dir.mount.id,
        YOCTO_LAYERS,
        MAX_BBLAYERS_BYTES,
        deadline
    )) else {
        return Ok(Ok(None));
    };
    let Some(layers) = bblayers(&bytes) else {
        return Ok(Ok(None));
    };
    for layer in &layers {
        if !judged!(survives(host, candidate, layer, YOCTO_LAYER_CONF, deadline)) {
            return Ok(Ok(None));
        }
    }
    evidence(YOCTO_MARKER, topdir, &stat, YOCTO_LAYERS)
}

pub(super) fn bazel<H: Host>(
    host: &H,
    candidate: &Path,
    candidate_fd: &H::Fd,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    let Some((bytes, _)) = judged!(read_regular(
        host,
        candidate,
        candidate_fd,
        candidate,
        mount_id,
        BAZEL_MARKER,
        MAX_BAZEL_MARKER_BYTES,
        deadline
    )) else {
        return Ok(Ok(None));
    };
    let Some(workspace) = bazel_workspace(&bytes, candidate) else {
        return Ok(Ok(None));
    };
    source_tree(
        host,
        candidate,
        &workspace,
        mount_id,
        BAZEL_MARKER,
        BAZEL_BOUNDARY,
        &[],
        deadline,
    )
}

/// The recorded workspace: one normalized absolute path, exactly as Bazel
/// wrote it, neither inside the output base nor containing it.
fn bazel_workspace(bytes: &[u8], candidate: &Path) -> Option<PathBuf> {
    let workspace = PathBuf::from(std::str::from_utf8(bytes).ok()?);
    let usable = !bytes.iter().any(u8::is_ascii_control)
        && normal_absolute(&workspace)
        && !workspace.starts_with(candidate)
        && !candidate.starts_with(&workspace);
    usable.then_some(workspace)
}

/// Buildroot's generated `BR2_EXTERNAL ?=` line (support/scripts/
/// br2-external, do_mk): exactly one, listing canonical absolute paths.
/// Any other assignment to `BR2_EXTERNAL` is not the generated form.
fn br2_externals(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    let mut found = None;
    for line in std::str::from_utf8(bytes).ok()?.lines() {
        let Some(rest) = line.strip_prefix("BR2_EXTERNAL") else {
            continue;
        };
        if rest.starts_with('_') {
            continue;
        }
        let paths = rest.strip_prefix(" ?=")?;
        if found.is_some() {
            return None;
        }
        let paths: Vec<PathBuf> = paths
            .split(' ')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
        if !paths.iter().all(|path| normal_absolute(path)) {
            return None;
        }
        found = Some(paths);
    }
    found
}

/// The layers of `BBLAYERS ?= "..."` (or `=`) in bblayers.conf, as
/// oe-setup-builddir writes it: absolute paths, possibly over
/// backslash-continued lines. Anything else that could change the layer
/// set is not read: no layers, no evidence. That is an expansion or append
/// (`${TOPDIR}`, `+=`, `:append`), any other line naming `BBLAYERS`
/// (`export BBLAYERS += ...`), and a directive that parses or inherits
/// another file (`include`, `include_all`, `require`, `inherit`,
/// `addfragments`), whose content this parser never sees.
fn bblayers(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    const DIRECTIVES: &[&str] = &[
        "include",
        "include_all",
        "require",
        "inherit",
        "addfragments",
        "export",
        "unset",
    ];
    let mut value: Option<String> = None;
    let mut lines = std::str::from_utf8(bytes).ok()?.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let first = trimmed
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        if DIRECTIVES.contains(&first) {
            return None;
        }
        let Some(rest) = trimmed.strip_prefix("BBLAYERS") else {
            if names_bblayers(trimmed) {
                return None;
            }
            continue;
        };
        if rest.starts_with('_') {
            continue;
        }
        let mut current = rest
            .strip_prefix(" ?= \"")
            .or_else(|| rest.strip_prefix(" = \""))?;
        if value.is_some() {
            return None;
        }
        let mut collected = String::new();
        loop {
            if let Some((inside, after)) = current.split_once('"') {
                if !after.trim().is_empty() {
                    return None;
                }
                collected.push_str(inside);
                break;
            }
            collected.push_str(current.strip_suffix('\\')?);
            collected.push(' ');
            current = lines.next()?;
        }
        value = Some(collected);
    }
    let layers: Vec<PathBuf> = value?.split_whitespace().map(PathBuf::from).collect();
    let literal = |layer: &PathBuf| {
        normal_absolute(layer)
            && !layer
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|byte| matches!(byte, b'$' | b'\\' | b'\'' | b'#'))
    };
    (!layers.is_empty() && layers.iter().all(literal)).then_some(layers)
}

/// `BBLAYERS` as a whole word (not `BBLAYERS_NON_REMOVABLE`).
fn names_bblayers(line: &str) -> bool {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = line.as_bytes();
    line.match_indices("BBLAYERS").any(|(at, found)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + found.len()).copied();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

fn named(candidate: &Path, wanted: &str) -> bool {
    candidate.file_name().is_some_and(|name| name == wanted)
}

/// The tree a build output came from, on the output's filesystem: every
/// one of `tree_files` is a regular file, and the first regular one of
/// `build_files` is the source identity recorded and rechecked.
#[allow(clippy::too_many_arguments)]
fn source_tree<H: Host>(
    host: &H,
    candidate: &Path,
    source: &Path,
    mount_id: u64,
    marker: &'static str,
    build_files: &[&str],
    tree_files: &[&str],
    deadline: Option<MonoTime>,
) -> Judged<Option<SourceEvidence>> {
    let Some(dir) = judged!(open_source_dir(host, candidate, source, mount_id, deadline)) else {
        return Ok(Ok(None));
    };
    if tree_files
        .iter()
        .chain(build_files)
        .any(|file| !outside(candidate, source, file))
    {
        return Ok(Ok(None));
    }
    for file in tree_files {
        if judged!(regular_file(
            host,
            candidate,
            &dir.fd,
            source,
            dir.mount.id,
            file,
            deadline
        ))
        .is_none()
        {
            return Ok(Ok(None));
        }
    }
    for file in build_files {
        if let Some((_, stat)) = judged!(regular_file(
            host,
            candidate,
            &dir.fd,
            source,
            dir.mount.id,
            file,
            deadline
        )) {
            return evidence(marker, source, &stat, file);
        }
    }
    Ok(Ok(None))
}

/// A source file the output would be rebuilt from must not be deleted
/// with it (a Yocto TMPDIR set to `${TOPDIR}/conf`).
fn outside(candidate: &Path, source: &Path, file: &str) -> bool {
    !source.join(file).starts_with(candidate)
}

fn evidence(
    marker: &'static str,
    source: &Path,
    stat: &Stat,
    file: &str,
) -> Judged<Option<SourceEvidence>> {
    let Some(source_file) = Identity::of(stat) else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            source.join(file),
            format!("{file} identity was not reported"),
        )));
    };
    Ok(Ok(Some(SourceEvidence {
        marker,
        source: source.to_owned(),
        source_file,
    })))
}

/// A tree the build also reads (a Yocto layer): outside the
/// output, reached without symlinks, still holding regular `file`. It may
/// lie outside the bound root and on another filesystem: it is only read.
fn survives<H: Host>(
    host: &H,
    candidate: &Path,
    tree: &Path,
    file: &str,
    deadline: Option<MonoTime>,
) -> Judged<bool> {
    if tree.starts_with(candidate) {
        return Ok(Ok(false));
    }
    let Some(dir) = judged!(open_dir(host, candidate, tree, deadline)) else {
        return Ok(Ok(false));
    };
    let found = judged!(regular_file(
        host,
        candidate,
        &dir.fd,
        tree,
        dir.mount.id,
        file,
        deadline
    ));
    Ok(Ok(found.is_some()))
}

fn open_source_dir<H: Host>(
    host: &H,
    candidate: &Path,
    source: &Path,
    mount_id: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<Resolved<H::Fd>>> {
    let Some(dir) = judged!(open_dir(host, candidate, source, deadline)) else {
        return Ok(Ok(None));
    };
    if dir.mount.id != mount_id {
        return Ok(Err(Skip::new(
            Reason::OtherFs,
            source.to_owned(),
            "the recorded build source is on another filesystem",
        )));
    }
    Ok(Ok(Some(dir)))
}

/// A directory by absolute path, no symlink followed and no unsafe mount
/// crossed. `None`: it does not exist.
fn open_dir<H: Host>(
    host: &H,
    candidate: &Path,
    path: &Path,
    deadline: Option<MonoTime>,
) -> Judged<Option<Resolved<H::Fd>>> {
    check_deadline(host, deadline, path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::UnsupportedFs,
                path.to_owned(),
                format!("cannot check source mounts: {error}"),
            )));
        }
    };
    match resolve_in(host, &mounts, path, Want::Directory) {
        Ok(dir) => Ok(Ok(Some(dir))),
        Err(ResolveError::Missing { .. }) => Ok(Ok(None)),
        Err(error) => Ok(Err(unreachable_build(candidate, path, error)?)),
    }
}

/// An entry `rel` below `dir` (reached as `dir_path`) of any type, without
/// following symlinks. `None`: absent.
pub(super) fn entry<H: Host>(
    host: &H,
    candidate: &Path,
    dir: &H::Fd,
    dir_path: &Path,
    mount_id: u64,
    rel: &str,
    deadline: Option<MonoTime>,
) -> Judged<Option<(H::Fd, Stat)>> {
    let path = dir_path.join(rel);
    check_deadline(host, deadline, &path)?;
    match open_beneath(host, dir, mount_id, Path::new(rel), Access::Path, Want::Any) {
        Ok(opened) => Ok(Ok(Some(opened))),
        Err(ResolveError::Missing { .. }) => Ok(Ok(None)),
        Err(error) => Ok(Err(unreachable_build(candidate, &path, error)?)),
    }
}

/// A regular file `rel` below `dir`. `None`: absent or of another type.
fn regular_file<H: Host>(
    host: &H,
    candidate: &Path,
    dir: &H::Fd,
    dir_path: &Path,
    mount_id: u64,
    rel: &str,
    deadline: Option<MonoTime>,
) -> Judged<Option<(H::Fd, Stat)>> {
    let found = judged!(entry(
        host, candidate, dir, dir_path, mount_id, rel, deadline
    ));
    Ok(Ok(found.filter(|(_, stat)| {
        stat.file_type == Some(FileType::Regular)
    })))
}

/// The bytes of regular file `rel` below `dir`, at most `limit`, and its
/// status. An unknown or excessive size, a failed read or a size change
/// during the read refuses.
#[allow(clippy::too_many_arguments)]
fn read_regular<H: Host>(
    host: &H,
    candidate: &Path,
    dir: &H::Fd,
    dir_path: &Path,
    mount_id: u64,
    rel: &str,
    limit: u64,
    deadline: Option<MonoTime>,
) -> Judged<Option<(Vec<u8>, Stat)>> {
    let path = dir_path.join(rel);
    let Some((fd, stat)) = judged!(regular_file(
        host, candidate, dir, dir_path, mount_id, rel, deadline
    )) else {
        return Ok(Ok(None));
    };
    let Some(size) = stat.size else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            path,
            format!("{rel} size was not reported"),
        )));
    };
    if size > limit {
        return Ok(Err(Skip::new(
            Reason::TooManyEntries,
            path,
            format!("{rel} exceeds the {limit}-byte source-anchor read limit"),
        )));
    }
    let bytes = match host.read_head(&fd, limit as usize + 1) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::Unreadable,
                path,
                format!("cannot read {rel}: {error}"),
            )));
        }
    };
    if bytes.len() as u64 != size {
        return Ok(Err(Skip::new(
            Reason::Changed,
            path,
            format!("{rel} changed while its source anchor was read"),
        )));
    }
    Ok(Ok(Some((bytes, stat))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn br2_externals_reads_only_the_generated_line() {
        let parse = |text: &str| br2_externals(text.as_bytes());
        assert_eq!(parse("BR2_EXTERNAL ?=\n"), Some(Vec::new()));
        assert_eq!(
            parse("BR2_EXTERNAL ?= /a/one /b/two\nBR2_EXTERNAL_DIRS += /a/one\n"),
            Some(vec![PathBuf::from("/a/one"), PathBuf::from("/b/two")])
        );
        for refused in [
            "",
            "export BR2_EXTERNAL_NAMES = \n",
            "BR2_EXTERNAL ?= /a\nBR2_EXTERNAL ?= /a\n",
            "BR2_EXTERNAL = /a\n",
            "BR2_EXTERNAL ?= /a\nBR2_EXTERNAL += /b\n",
            "BR2_EXTERNAL ?= relative\n",
            "BR2_EXTERNAL ?= /a/../b\n",
        ] {
            assert_eq!(parse(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn bblayers_reads_only_literal_absolute_layers() {
        let parse = |text: &str| bblayers(text.as_bytes());
        let sample = "LCONF_VERSION = \"7\"\nBBPATH = \"${TOPDIR}\"\nBBFILES ?= \"\"\n\nBBLAYERS ?= \" \\\n  /src/poky/meta \\\n  /src/poky/meta-poky \\\n  \"\nBBLAYERS_NON_REMOVABLE ?= \"/src/poky/meta\"\n";
        assert_eq!(
            parse(sample),
            Some(vec![
                PathBuf::from("/src/poky/meta"),
                PathBuf::from("/src/poky/meta-poky")
            ])
        );
        assert_eq!(
            parse("BBLAYERS = \"/a /b\"\n"),
            Some(vec![PathBuf::from("/a"), PathBuf::from("/b")])
        );
        for refused in [
            "",
            "BBLAYERS ?= \"\"\n",
            "BBLAYERS ?= \"${TOPDIR}/../meta\"\n",
            "BBLAYERS ?= \"/a\"\nBBLAYERS += \"/b\"\n",
            "BBLAYERS ?= \"/a\"\nBBLAYERS:append = \" /b\"\n",
            "BBLAYERS ?= \"/a\"\nBBLAYERS ?= \"/b\"\n",
            "BBLAYERS ?= \" \\\n  /a \n  \"\n",
            "BBLAYERS ?= \" \\\n  /a \\\n",
            "BBLAYERS ?= \"relative/meta\"\n",
            "BBLAYERS ?= \"/a\" # trailing\n",
            "BBLAYERS ?= \"/a\"\nrequire conf/bsp-layers.conf\n",
            "BBLAYERS ?= \"/a\"\ninclude conf/more.conf\n",
            "include_all conf/more.conf\nBBLAYERS ?= \"/a\"\n",
            "BBLAYERS ?= \"/a\"\nexport BBLAYERS += \"/b\"\n",
            "BBLAYERS ?= \"/a\"\n  export BBLAYERS\n",
            "BBLAYERS ?= \"/a\"\nFOO = \"${BBLAYERS}\"\n",
            "BBLAYERS ?= \"/a\"\ninherit something\n",
            "BBLAYERS ?= \"/a\"\naddfragments conf/fragments FRAGS\n",
        ] {
            assert_eq!(parse(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn bazel_workspace_is_exact_and_outside_the_output_base() {
        let base = Path::new("/home/u/.cache/bazel/_bazel_u/0123");
        assert_eq!(
            bazel_workspace(b"/home/u/src/ws", base),
            Some(PathBuf::from("/home/u/src/ws"))
        );
        for refused in [
            &b"src/ws"[..],
            b"/home/u/src/ws\n",
            b"/home/u/src/../ws",
            b"/home/u/.cache/bazel/_bazel_u/0123/ws",
            b"/home/u/.cache",
            b"\xff",
        ] {
            assert_eq!(bazel_workspace(refused, base), None, "{refused:?}");
        }
    }
}
