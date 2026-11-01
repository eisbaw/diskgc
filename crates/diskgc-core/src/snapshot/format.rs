//! The snapshot's file format (docs/architecture.md, The tally: Snapshot,
//! Format). Pure: a [`Snapshot`] to bytes and back, with every structural
//! check made on the way in, so a loaded snapshot can never hand the index
//! a relative or non-canonical path, an index out of range or a unit
//! outside its mount (the index asserts all of those: a corrupt file must
//! be refused here, not panic there; TASK-31 review (b)).
//!
//! Binary, little-endian, paths as bytes (they need not be UTF-8):
//!
//! ```text
//! magic "DISKGCIX" | format u32 | diskgc version str | key str | uid u32
//! | boot id str | written time
//! | finders:  u32 n, n × (profile str, profile hash u64, root path,
//!                         discovered_at opt time)
//! | mounts:   u32 n, n × (dev major u32, dev minor u32, mount point path)
//! | singles:  u32 n, n × (mount u32, path, identity, finders, newest_change
//!                         time, tally opt (est_bytes u64, files u64,
//!                         tallied_at time), checked_at time,
//!                         refusal opt (reason str, at time))
//! | blocks:   u32 n, n × (mount u32, site path, site identity, finders,
//!                         family (0 | 1 name bytes), members u64,
//!                         est_bytes u64, files u64, oldest_change time,
//!                         newest_change time, tallied_at opt time)
//! | checksum u64 (FNV-1a over every byte before it)
//! ```
//!
//! `str` and `path` are a u32 length and the bytes (a path at most
//! [`MAX_PATH_BYTES`] long and [`MAX_PATH_DEPTH`] components deep); `time` is i64 seconds
//! and u32 nanoseconds of the wall clock (monotonic times are never
//! written: they mean nothing in another process or boot); `opt` is a
//! byte 0 or 1 and the value; `identity` is the device (two u32), the
//! inode (u64) and the birth (0 and a time, or 1 and the parent's inode
//! u64); `finders` is a u32 count and that many indices into the finder
//! table, ascending. Members, `est_bytes` and `files` summed over all
//! units stay within [`MAX_TOTAL`]. The checksum detects a torn or damaged file, not a
//! crafted one: the file is the user's own (mode 0600) and grants nothing
//! (the index is never an authority).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::engine::Reason;
use crate::host::{Dev, FsKey, Timestamp};
use crate::index::{Birth, Family, Finder, Identity, MountKey};

/// The file's first bytes.
pub const MAGIC: &[u8; 8] = b"DISKGCIX";

/// The format this build writes and reads; any other is ignored whole.
pub const FORMAT_VERSION: u32 = 1;

/// The longest path a snapshot may hold, in bytes (Linux's `PATH_MAX`).
pub const MAX_PATH_BYTES: usize = 4096;

/// The most components a snapshot path may have. The index's trie is
/// walked and dropped recursively, one level per component, so a loaded
/// path bounds that depth (Codex review of TASK-30: a crafted deep path
/// exhausted the stack). Real candidates sit far shallower.
pub const MAX_PATH_DEPTH: usize = 256;

/// The most the snapshot's sizes and counts may add up to, each of
/// members, `est_bytes` and `files` summed over every unit (2^62, about
/// 4.6 EiB or 4.6 × 10^18 entries): far beyond any filesystem, and low
/// enough that the index's sums over any subset (collapse, recount,
/// statistics), plus what this run adds, never overflow (Codex and
/// mped review of TASK-30: two blocks near `u64::MAX` panicked).
pub const MAX_TOTAL: u64 = 1 << 62;

/// Running sums of a snapshot's sizes and counts ([`MAX_TOTAL`]).
#[derive(Default)]
struct Totals {
    members: u64,
    est_bytes: u64,
    files: u64,
}

impl Totals {
    fn add(&mut self, members: u64, est_bytes: u64, files: u64) -> Read<()> {
        let sum = |total: u64, more: u64, what: &str| {
            total
                .checked_add(more)
                .filter(|t| *t <= MAX_TOTAL)
                .ok_or_else(|| format!("the {what} of its units add up to more than 2^62"))
        };
        self.members = sum(self.members, members, "members")?;
        self.est_bytes = sum(self.est_bytes, est_bytes, "estimated bytes")?;
        self.files = sum(self.files, files, "files")?;
        Ok(())
    }
}

/// Who wrote the snapshot, for what, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The writing build's package version: profile hashes are stable only
    /// within one build, so another version's snapshot is ignored.
    pub diskgc_version: String,
    pub key: FsKey,
    /// The writer's effective uid.
    pub uid: u32,
    pub boot_id: String,
    /// Wall-clock time of the write.
    pub written: Timestamp,
}

/// A finder and when its source was last listed completely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinderRec {
    pub finder: Finder,
    pub discovered_at: Option<Timestamp>,
}

/// What the tally learned about a single's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TallyRec {
    pub est_bytes: u64,
    pub files: u64,
    pub tallied_at: Timestamp,
}

/// A single, with wall-clock times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleRec {
    /// Index into [`Snapshot::mounts`].
    pub mount: usize,
    pub path: PathBuf,
    pub identity: Identity,
    /// Indices into [`Snapshot::finders`], ascending.
    pub finders: Vec<usize>,
    pub newest_change: Timestamp,
    pub tally: Option<TallyRec>,
    pub checked_at: Timestamp,
    pub refusal: Option<(Reason, Timestamp)>,
}

/// A block, with wall-clock times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRec {
    pub mount: usize,
    /// The site directory's path (at or below the mount point).
    pub site: PathBuf,
    pub site_identity: Identity,
    pub finders: Vec<usize>,
    pub family: Family,
    pub members: u64,
    pub est_bytes: u64,
    pub files: u64,
    pub oldest_change: Timestamp,
    pub newest_change: Timestamp,
    pub tallied_at: Option<Timestamp>,
}

/// A snapshot as written and as read back (structurally checked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub header: Header,
    pub finders: Vec<FinderRec>,
    pub mounts: Vec<MountKey>,
    pub singles: Vec<SingleRec>,
    pub blocks: Vec<BlockRec>,
}

/// FNV-1a, 64 bits.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

// ---- Writing ----

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("a snapshot count or length fits u32"));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    fn path(&mut self, p: &Path) {
        self.bytes(p.as_os_str().as_bytes());
    }
    fn time(&mut self, t: Timestamp) {
        self.0.extend_from_slice(&t.secs.to_le_bytes());
        self.u32(t.nanos);
    }
    fn opt<T>(&mut self, v: Option<T>, mut write: impl FnMut(&mut Self, T)) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                write(self, v);
            }
        }
    }
    fn identity(&mut self, id: &Identity) {
        self.u32(id.dev.major);
        self.u32(id.dev.minor);
        self.u64(id.ino);
        match id.birth {
            Birth::Time(t) => {
                self.u8(0);
                self.time(t);
            }
            Birth::ParentIno(ino) => {
                self.u8(1);
                self.u64(ino);
            }
        }
    }
    fn indices(&mut self, list: &[usize]) {
        self.len(list.len());
        for &i in list {
            self.len(i);
        }
    }
}

/// The snapshot's bytes, checksum included.
pub fn encode(s: &Snapshot) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.0.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.bytes(s.header.diskgc_version.as_bytes());
    w.bytes(s.header.key.as_str().as_bytes());
    w.u32(s.header.uid);
    w.bytes(s.header.boot_id.as_bytes());
    w.time(s.header.written);
    w.len(s.finders.len());
    for f in &s.finders {
        w.bytes(f.finder.profile.as_bytes());
        w.u64(f.finder.profile_hash);
        w.path(&f.finder.root);
        w.opt(f.discovered_at, Writer::time);
    }
    w.len(s.mounts.len());
    for m in &s.mounts {
        w.u32(m.dev.major);
        w.u32(m.dev.minor);
        w.path(&m.mount_point);
    }
    w.len(s.singles.len());
    for u in &s.singles {
        w.len(u.mount);
        w.path(&u.path);
        w.identity(&u.identity);
        w.indices(&u.finders);
        w.time(u.newest_change);
        w.opt(u.tally, |w, t| {
            w.u64(t.est_bytes);
            w.u64(t.files);
            w.time(t.tallied_at);
        });
        w.time(u.checked_at);
        w.opt(u.refusal, |w, (reason, at)| {
            w.bytes(reason.as_str().as_bytes());
            w.time(at);
        });
    }
    w.len(s.blocks.len());
    for b in &s.blocks {
        w.len(b.mount);
        w.path(&b.site);
        w.identity(&b.site_identity);
        w.indices(&b.finders);
        match &b.family {
            Family::Any => w.u8(0),
            Family::Name(name) => {
                w.u8(1);
                w.bytes(name);
            }
        }
        w.u64(b.members);
        w.u64(b.est_bytes);
        w.u64(b.files);
        w.time(b.oldest_change);
        w.time(b.newest_change);
        w.opt(b.tallied_at, Writer::time);
    }
    let sum = fnv1a(&w.0);
    w.u64(sum);
    w.0
}

/// The limits of [`decode`] that a writer could exceed with an index that
/// is valid in memory: every path's length and depth ([`MAX_PATH_BYTES`],
/// [`MAX_PATH_DEPTH`]) and the sums of members, `est_bytes` and `files`
/// ([`MAX_TOTAL`]; a collapse adds a single's counters to a block, so
/// sums at the cap grow past it). The writer refuses a snapshot that
/// fails them, as one larger than any snapshot may be, rather than write
/// one every later run would ignore (TASK-279). The rest of `decode`'s
/// checks are of structure, which [`super::restore::capture`] holds by
/// construction.
pub fn check_limits(s: &Snapshot) -> Result<(), String> {
    for f in &s.finders {
        check_path(&f.finder.root, "finder root")?;
    }
    for m in &s.mounts {
        check_path(&m.mount_point, "mount point")?;
    }
    let mut totals = Totals::default();
    for u in &s.singles {
        check_path(&u.path, "single path")?;
        if let Some(t) = &u.tally {
            totals.add(0, t.est_bytes, t.files)?;
        }
    }
    for b in &s.blocks {
        check_path(&b.site, "block site")?;
        totals.add(b.members, b.est_bytes, b.files)?;
    }
    Ok(())
}

// ---- Reading ----

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

type Read<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Read<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| format!("truncated at byte {} reading {what}", self.at))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }
    fn u8(&mut self, what: &str) -> Read<u8> {
        Ok(self.take(1, what)?[0])
    }
    fn u32(&mut self, what: &str) -> Read<u32> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self, what: &str) -> Read<u64> {
        Ok(u64::from_le_bytes(
            self.take(8, what)?.try_into().expect("8 bytes"),
        ))
    }
    fn len(&mut self, what: &str) -> Read<usize> {
        Ok(usize::try_from(self.u32(what)?).expect("u32 fits usize"))
    }
    fn bytes(&mut self, what: &str) -> Read<&'a [u8]> {
        let n = self.len(what)?;
        self.take(n, what)
    }
    fn text(&mut self, what: &str) -> Read<String> {
        let bytes = self.bytes(what)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| format!("{what} is not UTF-8"))
    }
    fn path(&mut self, what: &str) -> Read<PathBuf> {
        let path = PathBuf::from(OsStr::from_bytes(self.bytes(what)?));
        check_path(&path, what)?;
        Ok(path)
    }
    fn time(&mut self, what: &str) -> Read<Timestamp> {
        let secs = i64::from_le_bytes(self.take(8, what)?.try_into().expect("8 bytes"));
        let nanos = self.u32(what)?;
        if nanos >= 1_000_000_000 {
            return Err(format!(
                "{what}: {nanos} nanoseconds is not below one second"
            ));
        }
        Ok(Timestamp { secs, nanos })
    }
    fn opt<T>(&mut self, what: &str, read: impl FnOnce(&mut Self) -> Read<T>) -> Read<Option<T>> {
        match self.u8(what)? {
            0 => Ok(None),
            1 => read(self).map(Some),
            other => Err(format!("{what}: option tag {other}")),
        }
    }
    fn identity(&mut self, what: &str) -> Read<Identity> {
        let dev = Dev {
            major: self.u32(what)?,
            minor: self.u32(what)?,
        };
        let ino = self.u64(what)?;
        let birth = match self.u8(what)? {
            0 => Birth::Time(self.time(what)?),
            1 => Birth::ParentIno(self.u64(what)?),
            other => return Err(format!("{what}: birth tag {other}")),
        };
        Ok(Identity { dev, ino, birth })
    }
    /// Finder indices: at least one, ascending, each below `count`.
    fn indices(&mut self, count: usize, what: &str) -> Read<Vec<usize>> {
        let n = self.len(what)?;
        let mut list = Vec::new();
        for _ in 0..n {
            let i = self.len(what)?;
            if i >= count {
                return Err(format!("{what}: finder {i} of {count}"));
            }
            if list.last().is_some_and(|last| *last >= i) {
                return Err(format!("{what}: finders not ascending"));
            }
            list.push(i);
        }
        if list.is_empty() {
            return Err(format!("{what}: no finder"));
        }
        Ok(list)
    }
}

/// Whether `path` is absolute and canonical: `/`, then names joined by
/// single `/`, none `.`, `..` or empty, no NUL byte, no trailing `/`.
/// (`Path::components` would silently drop a `.` or a doubled `/`, so the
/// path is rebuilt from its components and compared byte for byte.)
fn check_path(path: &Path, what: &str) -> Read<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > MAX_PATH_BYTES {
        return Err(format!(
            "{what} of {} bytes is longer than {MAX_PATH_BYTES}",
            bytes.len()
        ));
    }
    let mut canonical: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(format!("{what} {} is not absolute", path.display()));
    }
    for (depth, c) in components.enumerate() {
        if depth == MAX_PATH_DEPTH {
            return Err(format!(
                "{what} {} is deeper than {MAX_PATH_DEPTH} components",
                path.display()
            ));
        }
        let Component::Normal(name) = c else {
            return Err(format!("{what} {} holds {c:?}", path.display()));
        };
        canonical.push(b'/');
        canonical.extend_from_slice(name.as_bytes());
    }
    if canonical.is_empty() {
        canonical.push(b'/');
    }
    if canonical != bytes || bytes.contains(&0) {
        return Err(format!("{what} {} is not canonical", path.display()));
    }
    Ok(())
}

/// Reads and checks a snapshot: magic, format version, checksum, every
/// length and index, and the structure the index relies on (canonical
/// absolute paths, units strictly below their mount point and below each
/// of their finders' roots, identities on their mount's device). Which
/// run may use it (version, key, owner, boot) is the caller's to decide.
pub fn decode(bytes: &[u8]) -> Result<Snapshot, String> {
    if bytes.len() < MAGIC.len() + 4 + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err("not a diskgc snapshot (no magic)".to_owned());
    }
    let format = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    if format != FORMAT_VERSION {
        return Err(format!(
            "format version {format}, this build reads {FORMAT_VERSION}"
        ));
    }
    let (body, sum) = bytes.split_at(bytes.len() - 8);
    let sum = u64::from_le_bytes(sum.try_into().expect("8 bytes"));
    if fnv1a(body) != sum {
        return Err("checksum mismatch (torn or damaged)".to_owned());
    }
    let mut r = Reader {
        bytes: body,
        at: MAGIC.len() + 4,
    };
    let diskgc_version = r.text("diskgc version")?;
    let key_text = r.text("filesystem key")?;
    let key = FsKey::parse(&key_text).ok_or_else(|| format!("filesystem key {key_text:?}"))?;
    let uid = r.u32("uid")?;
    let boot_id = r.text("boot id")?;
    let written = r.time("write time")?;
    let header = Header {
        diskgc_version,
        key,
        uid,
        boot_id,
        written,
    };
    let mut finders = Vec::new();
    for _ in 0..r.len("finder count")? {
        let profile = r.text("finder profile")?;
        if profile.is_empty() {
            return Err("a finder without a profile".to_owned());
        }
        let profile_hash = r.u64("finder hash")?;
        let root = r.path("finder root")?;
        let discovered_at = r.opt("discovered_at", |r| r.time("discovered_at"))?;
        finders.push(FinderRec {
            finder: Finder {
                profile: profile.into(),
                profile_hash,
                root: root.into(),
            },
            discovered_at,
        });
    }
    if finders.windows(2).any(|w| w[0].finder >= w[1].finder) {
        return Err("finders not in ascending order".to_owned());
    }
    let mut mounts = Vec::new();
    for _ in 0..r.len("mount count")? {
        let dev = Dev {
            major: r.u32("mount device")?,
            minor: r.u32("mount device")?,
        };
        let mount_point = r.path("mount point")?;
        mounts.push(MountKey { dev, mount_point });
    }
    if mounts.windows(2).any(|w| w[0] >= w[1]) {
        return Err("mounts not in ascending order".to_owned());
    }
    let mount_of = |i: usize, what: &str| -> Read<&MountKey> {
        mounts
            .get(i)
            .ok_or_else(|| format!("{what}: mount {i} of {}", mounts.len()))
    };
    let mut totals = Totals::default();
    let mut singles = Vec::new();
    for _ in 0..r.len("single count")? {
        let mount = r.len("single mount")?;
        let path = r.path("single path")?;
        let identity = r.identity("single identity")?;
        let units_finders = r.indices(finders.len(), "single finders")?;
        let at = mount_of(mount, "single")?;
        if identity.dev != at.dev {
            return Err(format!(
                "{}: identity on another device than its mount",
                path.display()
            ));
        }
        if path == at.mount_point || !path.starts_with(&at.mount_point) {
            return Err(format!(
                "{}: not strictly below its mount point {}",
                path.display(),
                at.mount_point.display()
            ));
        }
        if let Some(f) = units_finders
            .iter()
            .map(|&i| &finders[i].finder)
            .find(|f| !path.starts_with(&f.root))
        {
            return Err(format!(
                "{}: not below its finder's root {}",
                path.display(),
                f.root.display()
            ));
        }
        let newest_change = r.time("newest_change")?;
        let tally = r.opt("tally", |r| {
            Ok(TallyRec {
                est_bytes: r.u64("est_bytes")?,
                files: r.u64("files")?,
                tallied_at: r.time("tallied_at")?,
            })
        })?;
        if let Some(t) = &tally {
            totals.add(0, t.est_bytes, t.files)?;
        }
        let checked_at = r.time("checked_at")?;
        let refusal = r.opt("refusal", |r| {
            let name = r.text("refusal reason")?;
            let reason = Reason::ALL
                .into_iter()
                .find(|reason| reason.as_str() == name)
                .ok_or_else(|| format!("refusal reason {name:?}"))?;
            Ok((reason, r.time("refusal time")?))
        })?;
        singles.push(SingleRec {
            mount,
            path,
            identity,
            finders: units_finders,
            newest_change,
            tally,
            checked_at,
            refusal,
        });
    }
    let mut blocks = Vec::new();
    for _ in 0..r.len("block count")? {
        let mount = r.len("block mount")?;
        let site = r.path("block site")?;
        let site_identity = r.identity("block site identity")?;
        let block_finders = r.indices(finders.len(), "block finders")?;
        let at = mount_of(mount, "block")?;
        if site_identity.dev != at.dev || !site.starts_with(&at.mount_point) {
            return Err(format!(
                "block at {}: not on its mount {}",
                site.display(),
                at.mount_point.display()
            ));
        }
        let family = match r.u8("block family")? {
            0 => Family::Any,
            1 => Family::Name(r.bytes("block family name")?.into()),
            other => return Err(format!("block family tag {other}")),
        };
        let members = r.u64("block members")?;
        if members == 0 {
            return Err(format!("block at {} without members", site.display()));
        }
        let est_bytes = r.u64("block est_bytes")?;
        let files = r.u64("block files")?;
        totals.add(members, est_bytes, files)?;
        let oldest_change = r.time("block oldest_change")?;
        let newest_change = r.time("block newest_change")?;
        if oldest_change > newest_change {
            return Err(format!("block at {}: ages out of order", site.display()));
        }
        let tallied_at = r.opt("block tallied_at", |r| r.time("block tallied_at"))?;
        blocks.push(BlockRec {
            mount,
            site,
            site_identity,
            finders: block_finders,
            family,
            members,
            est_bytes,
            files,
            oldest_change,
            newest_change,
            tallied_at,
        });
    }
    if r.at != body.len() {
        return Err(format!("{} bytes after the last block", body.len() - r.at));
    }
    Ok(Snapshot {
        header,
        finders,
        mounts,
        singles,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 7 }
    }

    fn dev() -> Dev {
        Dev {
            major: 0,
            minor: 42,
        }
    }

    fn sample() -> Snapshot {
        let finder = |profile: &str| Finder {
            profile: profile.into(),
            profile_hash: 0xfeed,
            root: Path::new("/tmp").into(),
        };
        Snapshot {
            header: Header {
                diskgc_version: "0.1.0".to_owned(),
                key: FsKey::dev(dev()),
                uid: 1000,
                boot_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                written: ts(1_000),
            },
            finders: vec![
                FinderRec {
                    finder: finder("tmp-a"),
                    discovered_at: Some(ts(990)),
                },
                FinderRec {
                    finder: finder("tmp-b"),
                    discovered_at: None,
                },
            ],
            mounts: vec![MountKey {
                dev: dev(),
                mount_point: PathBuf::from("/tmp"),
            }],
            singles: vec![SingleRec {
                mount: 0,
                // Paths are bytes, not UTF-8.
                path: PathBuf::from(OsStr::from_bytes(b"/tmp/a\xff1")),
                identity: Identity {
                    dev: dev(),
                    ino: 12,
                    birth: Birth::Time(ts(5)),
                },
                finders: vec![0, 1],
                newest_change: ts(500),
                tally: Some(TallyRec {
                    est_bytes: 4096,
                    files: 2,
                    tallied_at: ts(995),
                }),
                checked_at: ts(996),
                refusal: Some((Reason::KeepMarker, ts(997))),
            }],
            blocks: vec![BlockRec {
                mount: 0,
                site: PathBuf::from("/tmp"),
                site_identity: Identity {
                    dev: dev(),
                    ino: 2,
                    birth: Birth::ParentIno(2),
                },
                finders: vec![1],
                family: Family::Name(b"pytest-#".to_vec().into()),
                members: 3,
                est_bytes: 9,
                files: 3,
                oldest_change: ts(1),
                newest_change: ts(2),
                tallied_at: None,
            }],
        }
    }

    #[test]
    fn a_snapshot_reads_back_as_written() {
        let s = sample();
        assert_eq!(decode(&encode(&s)), Ok(s));
    }

    #[test]
    fn any_damaged_byte_or_cut_is_refused() {
        let bytes = encode(&sample());
        for at in 0..bytes.len() {
            let mut bent = bytes.clone();
            bent[at] ^= 0x01;
            assert!(decode(&bent).is_err(), "byte {at} flipped");
        }
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "cut at {len}");
        }
    }

    /// Re-encoding with a valid checksum: the structural checks alone.
    fn refused(bend: impl FnOnce(&mut Snapshot)) -> String {
        let mut s = sample();
        bend(&mut s);
        decode(&encode(&s)).expect_err("refused")
    }

    #[test]
    fn paths_the_index_would_assert_on_are_refused() {
        for (path, why) in [
            ("tmp/a1", "is not absolute"),
            ("/tmp/../a1", "holds ParentDir"),
            ("/tmp/./a1", "is not canonical"),
            ("/tmp//a1", "is not canonical"),
            ("/tmp/a1/", "is not canonical"),
            (&format!("/tmp/{}", "d/".repeat(300)), "deeper than 256"),
            (&format!("/tmp/{}", "x".repeat(5000)), "longer than 4096"),
            ("/tmp", "not strictly below its mount point"),
            ("/etc/a1", "not strictly below its mount point"),
        ] {
            let why_got = refused(|s| s.singles[0].path = PathBuf::from(path));
            assert!(why_got.contains(why), "{path}: {why_got}");
        }
        let got = refused(|s| s.finders[0].finder.root = Path::new("/var").into());
        assert!(got.contains("not below its finder's root /var"), "{got}");
        let got = refused(|s| s.mounts[0].mount_point = PathBuf::from("tmp"));
        assert!(got.contains("is not absolute"), "{got}");
        let got = refused(|s| s.blocks[0].site = PathBuf::from("/var"));
        assert!(got.contains("not on its mount"), "{got}");
    }

    #[test]
    fn indices_identities_and_counts_out_of_shape_are_refused() {
        for (bend, why) in [
            (
                Box::new(|s: &mut Snapshot| s.singles[0].finders = vec![2])
                    as Box<dyn FnOnce(&mut Snapshot)>,
                "finder 2 of 2",
            ),
            (
                Box::new(|s| s.singles[0].finders = vec![1, 0]),
                "not ascending",
            ),
            (Box::new(|s| s.singles[0].finders = vec![]), "no finder"),
            (Box::new(|s| s.singles[0].mount = 1), "mount 1 of 1"),
            (
                Box::new(|s| s.singles[0].identity.dev = Dev { major: 0, minor: 1 }),
                "another device",
            ),
            (Box::new(|s| s.blocks[0].members = 0), "without members"),
            (
                Box::new(|s| s.blocks[0].members = u64::MAX),
                "members of its units add up",
            ),
            (
                Box::new(|s| {
                    s.blocks[0].est_bytes = MAX_TOTAL;
                }),
                "estimated bytes of its units add up",
            ),
            (
                Box::new(|s| {
                    let mut twin = s.blocks[0].clone();
                    twin.files = MAX_TOTAL - 1;
                    s.blocks[0].files = 2;
                    s.blocks.push(twin);
                }),
                "files of its units add up",
            ),
        ] {
            let got = refused(bend);
            assert!(got.contains(why), "{why}: {got}");
        }
    }

    /// TASK-279: the writer's limits are the reader's. Each bent sample
    /// is refused by [`check_limits`] exactly when [`decode`] refuses it
    /// for a limit (the sums at the cap and one past it, a path at and
    /// past each bound), so the writer never writes what the reader
    /// ignores, nor skips what it would load.
    #[test]
    fn the_writer_refuses_exactly_what_the_reader_refuses_for_a_limit() {
        let at_depth = |depth: usize| format!("/tmp{}", "/d".repeat(depth - 1));
        type Bend = Box<dyn Fn(&mut Snapshot)>;
        // Each bend, and whether the reader loads the result.
        let cases: Vec<(Bend, bool)> = vec![
            (Box::new(|_| {}), true),
            // The sums: the sample holds est_bytes 4096 + 9 and members 3.
            (Box::new(|s| s.blocks[0].est_bytes = MAX_TOTAL - 4096), true),
            (
                Box::new(|s| s.blocks[0].est_bytes = MAX_TOTAL - 4095),
                false,
            ),
            (Box::new(|s| s.blocks[0].members = MAX_TOTAL), true),
            (Box::new(|s| s.blocks[0].members = MAX_TOTAL + 1), false),
            (Box::new(|s| s.blocks[0].files = u64::MAX), false),
            (
                // files: the single's 2, the block's 3, the twin's.
                Box::new(|s| {
                    let mut twin = s.blocks[0].clone();
                    twin.files = MAX_TOTAL - 5;
                    s.blocks.push(twin);
                }),
                true,
            ),
            (
                Box::new(|s| {
                    let mut twin = s.blocks[0].clone();
                    twin.files = MAX_TOTAL - 5;
                    s.blocks[0].files = 3 + 1;
                    s.blocks.push(twin);
                }),
                false,
            ),
            // The paths, of a single, a block, a finder's root.
            (
                Box::new(move |s| s.singles[0].path = PathBuf::from(at_depth(MAX_PATH_DEPTH))),
                true,
            ),
            (
                Box::new(move |s| {
                    s.singles[0].path = PathBuf::from(at_depth(MAX_PATH_DEPTH + 1));
                }),
                false,
            ),
            (
                Box::new(|s| {
                    s.singles[0].path = PathBuf::from(format!("/tmp/{}", "x".repeat(4091)));
                }),
                true,
            ),
            (
                Box::new(|s| {
                    s.singles[0].path = PathBuf::from(format!("/tmp/{}", "x".repeat(4092)));
                }),
                false,
            ),
            (
                Box::new(move |s| s.blocks[0].site = PathBuf::from(at_depth(MAX_PATH_DEPTH + 1))),
                false,
            ),
            (
                Box::new(move |s| {
                    let root = at_depth(MAX_PATH_DEPTH + 1);
                    s.finders[0].finder.root = Path::new(&root).into();
                }),
                false,
            ),
        ];
        for (i, (bend, loads)) in cases.iter().enumerate() {
            let mut s = sample();
            bend(&mut s);
            let written = check_limits(&s);
            let read = decode(&encode(&s));
            assert_eq!(written.is_ok(), *loads, "case {i}: writer {written:?}");
            if *loads {
                assert_eq!(read, Ok(s), "case {i}");
            } else {
                // Any refusal here must be the limit's.
                let why = read.expect_err("the reader refuses it");
                assert!(
                    why.contains("2^62") || why.contains("deeper") || why.contains("longer"),
                    "case {i}: the reader refused for another reason: {why}"
                );
            }
        }
    }
}
