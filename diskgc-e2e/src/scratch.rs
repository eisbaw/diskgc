//! Scratch mounts and the fixture operations that touch them (docs/testing.md,
//! Safety interlock, layer 3).
//!
//! Each test gets fresh tmpfs mounts at the container's `/tmp` (where
//! diskgc's filesystem profiles look), `/run/diskgc` (the host lock) and
//! `/home/tester`, plus a private records tmpfs for the harness: objects on a
//! tmpfs have stable inode numbers and birth times, unlike the overlay root.
//! Every fixture operation goes through [`Scratch`], which checks **by mount
//! id** that the path it is about to touch lies on that mount and aborts the
//! test process otherwise ([`interlock::abort_outside_scratch`]). The mount id
//! (not the device) is the right key: a bind mount of the scratch filesystem
//! elsewhere has the same device but a different mount id, and is refused.
//!
//! Paths given to the operations are relative to the scratch root; absolute
//! paths, `.` and `..` are refused lexically, and symlinks in the parent are
//! caught by the mount-id check on the directory the kernel actually
//! resolves to. Final components are never followed (`O_EXCL`, `lchown`,
//! `AT_SYMLINK_NOFOLLOW`).

use std::ffi::CString;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags};
use rustix::mount::MountFlags;

use crate::{Owner, info, interlock};

/// Mount id of the mount `path` is on (`statx` `STATX_MNT_ID`, Linux 5.8+).
/// With `follow`, a final symlink is followed (used for parents, which the
/// kernel follows when creating an entry in them).
pub fn mount_id(path: &Path, follow: bool) -> Result<u64, String> {
    let flags = if follow {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    let stx = rustix::fs::statx(CWD, path, flags, StatxFlags::MNT_ID)
        .map_err(|e| format!("statx {}: {e}", path.display()))?;
    if stx.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(format!(
            "statx {}: kernel did not report a mount id (needs Linux 5.8+)",
            path.display()
        ));
    }
    Ok(stx.stx_mnt_id)
}

/// Whether `path` is the root of a mount (`STATX_ATTR_MOUNT_ROOT`).
fn is_mount_root(path: &Path) -> Result<bool, String> {
    let stx = rustix::fs::statx(
        CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS,
    )
    .map_err(|e| format!("statx {}: {e}", path.display()))?;
    if !stx
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
    {
        return Err(format!(
            "statx {}: kernel does not report STATX_ATTR_MOUNT_ROOT",
            path.display()
        ));
    }
    Ok(stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
}

/// A tmpfs the harness mounted, and the checked operations on it.
#[derive(Debug)]
pub struct Scratch {
    root: PathBuf,
    mnt_id: u64,
}

impl Scratch {
    /// Mounts a fresh tmpfs over the existing directory `root` (baked into
    /// the image) with the given octal `mode` and `owner` for its root
    /// directory.
    pub fn mount(root: &str, mode: &str, owner: Owner) -> Result<Self, String> {
        let root = PathBuf::from(root);
        let covered = mount_id(&root, false)?;
        let data = CString::new(format!("mode={mode},uid={},gid={}", owner.uid, owner.gid))
            .expect("mount options have no NUL");
        rustix::mount::mount(
            "diskgc-e2e-scratch",
            &root,
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            data.as_c_str(),
        )
        .map_err(|e| format!("mount tmpfs at {}: {e}", root.display()))?;
        let mnt_id = mount_id(&root, false)?;
        if mnt_id == covered {
            return Err(format!(
                "mounting tmpfs at {} did not change its mount id ({mnt_id})",
                root.display()
            ));
        }
        info!("scratch tmpfs at {} (mount id {mnt_id})", root.display());
        Ok(Self { root, mnt_id })
    }

    /// Uses a scratch mount the harness already made in this mount namespace
    /// (probe subprocesses of a test). Refuses unless `root` is a mount root
    /// that is not the container's root mount.
    pub fn attach(root: &str) -> Result<Self, String> {
        let root = PathBuf::from(root);
        if !is_mount_root(&root)? {
            return Err(format!("{} is not a mount point", root.display()));
        }
        let mnt_id = mount_id(&root, false)?;
        if mnt_id == mount_id(Path::new("/"), false)? {
            return Err(format!("{} is on the root mount", root.display()));
        }
        Ok(Self { root, mnt_id })
    }

    /// The scratch root, e.g. `/tmp`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The scratch mount's id.
    pub fn mnt_id(&self) -> u64 {
        self.mnt_id
    }

    /// Absolute path of `rel` below the scratch root, for handing to code
    /// under test. Lexical check only: it touches nothing.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.join_checked("path", rel)
    }

    fn join_checked(&self, op: &str, rel: &str) -> PathBuf {
        let rel_path = Path::new(rel);
        let lexically_below = !rel.is_empty()
            && rel_path
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !lexically_below {
            interlock::abort_outside_scratch(
                op,
                rel_path,
                &format!(
                    "fixture paths must be relative to the scratch root {} without '.' or '..'",
                    self.root.display()
                ),
            );
        }
        self.root.join(rel_path)
    }

    /// Aborts unless `path` (following a final symlink if `follow`) is on the
    /// scratch mount. An unresolvable path also aborts: the check fails
    /// closed.
    fn require_on_scratch(&self, op: &str, path: &Path, follow: bool) {
        match mount_id(path, follow) {
            Ok(id) if id == self.mnt_id => {}
            Ok(id) => interlock::abort_outside_scratch(
                op,
                path,
                &format!(
                    "on mount id {id}, not on the scratch mount {} (id {})",
                    self.root.display(),
                    self.mnt_id
                ),
            ),
            Err(e) => interlock::abort_outside_scratch(op, path, &format!("cannot check: {e}")),
        }
    }

    /// Checks the directory a new entry `rel` will be created in, and returns
    /// the entry's absolute path.
    fn for_create(&self, op: &str, rel: &str) -> PathBuf {
        let path = self.join_checked(op, rel);
        let parent = path.parent().expect("joined path has a parent");
        self.require_on_scratch(op, parent, true);
        path
    }

    /// Checks an existing entry `rel` (not following it) and returns its
    /// absolute path.
    fn for_existing(&self, op: &str, rel: &str) -> PathBuf {
        let path = self.join_checked(op, rel);
        self.require_on_scratch(op, &path, false);
        path
    }

    /// Creates directory `rel` (its parent must exist) with `mode` and owner.
    pub fn mkdir(&self, rel: &str, mode: u32, owner: Owner) -> Result<PathBuf, String> {
        let path = self.for_create("mkdir", rel);
        DirBuilder::new()
            .mode(mode)
            .create(&path)
            .map_err(|e| format!("mkdir {}: {e}", path.display()))?;
        self.finish("mkdir", &path, mode, owner)?;
        Ok(path)
    }

    /// Creates `rel` and any missing parents, each with `mode` and owner.
    pub fn mkdirs(&self, rel: &str, mode: u32, owner: Owner) -> Result<PathBuf, String> {
        let mut prefix = PathBuf::new();
        for component in Path::new(rel).components() {
            prefix.push(component);
            let prefix_str = prefix.to_str().ok_or("non-UTF-8 fixture path")?;
            if !self.join_checked("mkdirs", prefix_str).exists() {
                self.mkdir(prefix_str, mode, owner)?;
            }
        }
        Ok(self.join_checked("mkdirs", rel))
    }

    /// Creates the regular file `rel` (must not exist) with `content`, `mode`
    /// and owner.
    pub fn write_file(
        &self,
        rel: &str,
        content: &[u8],
        mode: u32,
        owner: Owner,
    ) -> Result<PathBuf, String> {
        let path = self.for_create("write_file", rel);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        file.write_all(content)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        drop(file);
        self.finish("write_file", &path, mode, owner)?;
        Ok(path)
    }

    /// Creates the symlink `rel` pointing at `target`. Only the link itself
    /// is created; the target is not touched.
    pub fn symlink(&self, rel: &str, target: &Path) -> Result<PathBuf, String> {
        let path = self.for_create("symlink", rel);
        std::os::unix::fs::symlink(target, &path)
            .map_err(|e| format!("symlink {} -> {}: {e}", path.display(), target.display()))?;
        self.require_on_scratch("symlink", &path, false);
        Ok(path)
    }

    /// Changes the mode of the existing non-symlink entry `rel`.
    pub fn chmod(&self, rel: &str, mode: u32) -> Result<(), String> {
        let path = self.for_existing("chmod", rel);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("lstat {}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "chmod {}: refusing to chmod a symlink",
                path.display()
            ));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", path.display()))
    }

    /// Renames the existing entry `from_rel` (not following it) to `to_rel`,
    /// replacing what is there (`rename(2)`): same path, new object.
    pub fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), String> {
        let from = self.for_existing("rename", from_rel);
        let to = self.for_create("rename", to_rel);
        std::fs::rename(&from, &to)
            .map_err(|e| format!("rename {} -> {}: {e}", from.display(), to.display()))?;
        self.require_on_scratch("rename", &to, false);
        Ok(())
    }

    /// Changes the owner of the existing entry `rel` (not following it).
    pub fn chown(&self, rel: &str, owner: Owner) -> Result<(), String> {
        let path = self.for_existing("chown", rel);
        std::os::unix::fs::lchown(&path, Some(owner.uid), Some(owner.gid))
            .map_err(|e| format!("lchown {}: {e}", path.display()))
    }

    /// Bind-mounts the scratch directory `source_rel` onto the scratch
    /// directory `target_rel`. The target becomes a different mount, so
    /// fixture operations below it are refused afterwards.
    pub fn bind_mount(&self, source_rel: &str, target_rel: &str) -> Result<(), String> {
        let source = self.for_existing("bind_mount", source_rel);
        let target = self.for_existing("bind_mount", target_rel);
        rustix::mount::mount_bind(&source, &target).map_err(|e| {
            format!(
                "bind mount {} on {}: {e}",
                source.display(),
                target.display()
            )
        })
    }

    /// Like [`Scratch::bind_mount`], then makes the new mount read-only
    /// (a bind remount, so the scratch filesystem itself stays writable).
    pub fn bind_mount_read_only(&self, source_rel: &str, target_rel: &str) -> Result<(), String> {
        self.bind_mount(source_rel, target_rel)?;
        let target = self.join_checked("bind_mount_read_only", target_rel);
        rustix::mount::mount_remount(&target, MountFlags::BIND | MountFlags::RDONLY, c"")
            .map_err(|e| format!("remount {} read-only: {e}", target.display()))
    }

    /// Mounts a fresh small tmpfs on the scratch directory `rel` (a mount
    /// appearing inside a candidate). Fixture operations below it are
    /// refused afterwards, as below any other mount.
    pub fn mount_tmpfs(&self, rel: &str) -> Result<(), String> {
        let target = self.for_existing("mount_tmpfs", rel);
        rustix::mount::mount(
            "diskgc-e2e-inner",
            &target,
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            c"size=1m",
        )
        .map_err(|e| format!("mount tmpfs on {}: {e}", target.display()))
    }

    /// Unmounts the mount on the scratch directory `rel` (one made by
    /// [`Scratch::mount_tmpfs`] or [`Scratch::bind_mount`]); refuses unless
    /// what is there is a mount whose mount point lies on the scratch mount.
    pub fn unmount(&self, rel: &str) -> Result<(), String> {
        let target = self.join_checked("unmount", rel);
        let parent = target.parent().expect("joined path has a parent");
        self.require_on_scratch("unmount", parent, false);
        if !is_mount_root(&target)? || mount_id(&target, false)? == self.mnt_id {
            return Err(format!("unmount {}: not a mount", target.display()));
        }
        rustix::mount::unmount(&target, rustix::mount::UnmountFlags::empty())
            .map_err(|e| format!("unmount {}: {e}", target.display()))
    }

    /// Creates the FIFO `rel` with `mode` and owner.
    pub fn fifo(&self, rel: &str, mode: u32, owner: Owner) -> Result<PathBuf, String> {
        let path = self.for_create("fifo", rel);
        rustix::fs::mknodat(
            CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(mode),
            0,
        )
        .map_err(|e| format!("mkfifo {}: {e}", path.display()))?;
        self.finish("fifo", &path, mode, owner)?;
        Ok(path)
    }

    /// Creates the Unix socket file `rel` (bound, then the listener closed:
    /// the file stays, as a crashed server's would) with owner.
    pub fn socket(&self, rel: &str, owner: Owner) -> Result<PathBuf, String> {
        let path = self.for_create("socket", rel);
        drop(
            std::os::unix::net::UnixListener::bind(&path)
                .map_err(|e| format!("bind {}: {e}", path.display()))?,
        );
        self.finish("socket", &path, 0o755, owner)?;
        Ok(path)
    }

    /// Creates a chain of `depth` nested directories, each called `name`,
    /// below the existing scratch directory `rel`, each holding a file `f`,
    /// all with owner. Works fd-relative, one level at a time, so the chain
    /// can be deeper than `PATH_MAX`, which every path-based operation here
    /// refuses; each new directory is checked to be on the scratch mount.
    pub fn deep_chain(
        &self,
        rel: &str,
        depth: usize,
        name: &str,
        owner: Owner,
    ) -> Result<(), String> {
        use std::os::fd::AsFd;
        let start = self.for_existing("deep_chain", rel);
        let open = |dir: rustix::fd::BorrowedFd<'_>, name: &str| {
            rustix::fs::openat(
                dir,
                name,
                rustix::fs::OFlags::PATH
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|e| format!("open {name} in the chain below {}: {e}", start.display()))
        };
        let uid = Some(rustix::process::Uid::from_raw(owner.uid));
        let gid = Some(rustix::process::Gid::from_raw(owner.gid));
        let mut dir = open(CWD, start.to_str().ok_or("non-UTF-8 fixture path")?)?;
        for level in 0..depth {
            let fail = |what: &str, e: rustix::io::Errno| {
                format!("{what} at level {level} below {}: {e}", start.display())
            };
            let file = rustix::fs::openat(
                &dir,
                "f",
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o644),
            )
            .map_err(|e| fail("create f", e))?;
            rustix::fs::fchown(&file, uid, gid).map_err(|e| fail("chown f", e))?;
            rustix::fs::mkdirat(&dir, name, rustix::fs::Mode::from_raw_mode(0o755))
                .map_err(|e| fail("mkdir", e))?;
            let next = open(dir.as_fd(), name)?;
            rustix::fs::chownat(&dir, name, uid, gid, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| fail("chown", e))?;
            let on = rustix::fs::statx(&next, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
                .map_err(|e| fail("statx", e))?;
            if on.stx_mnt_id != self.mnt_id {
                interlock::abort_outside_scratch(
                    "deep_chain",
                    &start,
                    &format!(
                        "level {level} is on mount {}, not the scratch",
                        on.stx_mnt_id
                    ),
                );
            }
            dir = next;
        }
        Ok(())
    }

    /// Owner and mode for a freshly created entry, then a re-check that the
    /// created object really is on the scratch mount.
    fn finish(&self, op: &str, path: &Path, mode: u32, owner: Owner) -> Result<(), String> {
        self.require_on_scratch(op, path, false);
        std::os::unix::fs::lchown(path, Some(owner.uid), Some(owner.gid))
            .map_err(|e| format!("lchown {}: {e}", path.display()))?;
        // chown may clear set-id bits and the umask narrowed the mode at
        // creation; set the requested mode last.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", path.display()))
    }
}
