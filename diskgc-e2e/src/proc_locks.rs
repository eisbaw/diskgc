//! `/proc/locks` sampling: the independent view of who holds or waits for
//! which file lock (docs/testing.md, Grounding, negative feedback). The Cargo
//! tests use it to prove that a helper paused a Cargo process where it
//! should, holding (or not holding) the target's lock files.
//!
//! A line of `/proc/locks` (fs/locks.c, `lock_get_status`):
//!
//! ```text
//! 1: FLOCK  ADVISORY  WRITE 1234 00:2a:5678 0 EOF
//! 1: -> FLOCK  ADVISORY  WRITE 1235 00:2a:5678 0 EOF
//! ```
//!
//! an id, `->` for a request blocked behind the lock on the line before,
//! the class (`FLOCK`, `POSIX`, `OFDLCK`, `LEASE`, ...), its type, the mode,
//! the pid (as seen from the reader's pid namespace; `-1` for OFD locks),
//! the file as `MAJOR:MINOR:INODE` (device numbers in hex, the inode in
//! decimal), and the byte range.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A file as `/proc/locks` names it: device major, minor and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId {
    pub major: u32,
    pub minor: u32,
    pub ino: u64,
}

impl FileId {
    /// The id of the file at `path` (not following a final symlink).
    pub fn of(path: &Path) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| format!("lstat {}: {e}", path.display()))?;
        let dev = meta.dev();
        Ok(Self {
            major: rustix::fs::major(dev),
            minor: rustix::fs::minor(dev),
            ino: meta.ino(),
        })
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02x}:{:02x}:{}", self.major, self.minor, self.ino)
    }
}

/// One line of `/proc/locks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    /// A request blocked behind a granted lock (`->`), not a granted lock.
    pub waiting: bool,
    /// `FLOCK`, `POSIX`, `OFDLCK`, `LEASE`, `DELEG`.
    pub class: String,
    /// `READ` (shared), `WRITE` (exclusive) or `NONE`.
    pub mode: String,
    /// The locker's pid in the reader's pid namespace; -1 for OFD locks.
    pub pid: i64,
    pub file: FileId,
}

/// Parses the text of `/proc/locks`. Fails on any line it cannot read, so
/// a format change is noticed rather than silently sampled as "no locks".
pub fn parse(text: &str) -> Result<Vec<Lock>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| parse_line(line).ok_or_else(|| format!("unreadable /proc/locks line {line:?}")))
        .collect()
}

fn parse_line(line: &str) -> Option<Lock> {
    let mut fields = line.split_whitespace();
    fields.next()?.strip_suffix(':')?.parse::<u64>().ok()?;
    let mut class = fields.next()?;
    let waiting = class == "->";
    if waiting {
        class = fields.next()?;
    }
    let _type = fields.next()?;
    let mode = fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    let mut id = fields.next()?.split(':');
    let major = u32::from_str_radix(id.next()?, 16).ok()?;
    let minor = u32::from_str_radix(id.next()?, 16).ok()?;
    let ino = id.next()?.parse().ok()?;
    if id.next().is_some() {
        return None;
    }
    // The byte range; both present on every class.
    fields.next()?;
    fields.next()?;
    Some(Lock {
        waiting,
        class: class.to_owned(),
        mode: mode.to_owned(),
        pid,
        file: FileId { major, minor, ino },
    })
}

/// How many times [`sample`] tries to read `/proc/locks` in one `read`.
const SAMPLE_ATTEMPTS: usize = 100;

/// Reads `/proc/locks` once, as one consistent snapshot (the harness runs as
/// container root, and a container sees only the locks of its own pid
/// namespace's processes).
///
/// Consistency needs a single `read(2)`: seq_file produces one buffer (a
/// page) of text per call under the lock list's lock, and the next call
/// restarts at an index into the host-wide list, which the processes of
/// every other container change all the time, so a text read in several
/// calls can repeat or skip lines (seen: one `flock` listed twice under a
/// parallel run). The whole text is read with one call into a large buffer;
/// if a second call still returns text, the sample is retried, and after
/// [`SAMPLE_ATTEMPTS`] it fails rather than return a torn sample.
pub fn sample() -> Result<Vec<Lock>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..SAMPLE_ATTEMPTS {
        let mut file =
            std::fs::File::open("/proc/locks").map_err(|e| format!("open /proc/locks: {e}"))?;
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read /proc/locks: {e}"))?;
        let more = file
            .read(&mut buf[n..])
            .map_err(|e| format!("read /proc/locks: {e}"))?;
        if more == 0 {
            let text = std::str::from_utf8(&buf[..n])
                .map_err(|e| format!("/proc/locks is not UTF-8: {e}"))?;
            return parse(text);
        }
    }
    Err(format!(
        "/proc/locks did not come in one read in {SAMPLE_ATTEMPTS} attempts (more than a \
         page of locks?): no consistent sample"
    ))
}

/// Named files whose locks a test watches: the path for messages, the id
/// for matching `/proc/locks` lines.
#[derive(Debug, Clone)]
pub struct Watched {
    files: Vec<(PathBuf, FileId)>,
}

/// What one process does with the watched files in one sample.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Watched files it holds a granted lock on, with the mode.
    pub holds: Vec<(PathBuf, String)>,
    /// Watched files it is blocked waiting to lock.
    pub waits: Vec<PathBuf>,
}

impl Usage {
    /// Whether it holds a granted lock on `path`.
    pub fn holds(&self, path: &Path) -> bool {
        self.holds.iter().any(|(p, _)| p == path)
    }

    /// Whether it waits for a lock on `path`.
    pub fn waits(&self, path: &Path) -> bool {
        self.waits.iter().any(|p| p == path)
    }

    /// Neither holds nor waits for any watched file.
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty() && self.waits.is_empty()
    }
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let holds: Vec<String> = self
            .holds
            .iter()
            .map(|(p, mode)| format!("{} ({mode})", p.display()))
            .collect();
        let waits: Vec<String> = self.waits.iter().map(|p| p.display().to_string()).collect();
        write!(
            f,
            "holds [{}], waits for [{}]",
            holds.join(", "),
            waits.join(", ")
        )
    }
}

impl Watched {
    /// Watches the existing files `paths`.
    pub fn files(paths: &[PathBuf]) -> Result<Self, String> {
        let files = paths
            .iter()
            .map(|p| Ok((p.clone(), FileId::of(p)?)))
            .collect::<Result<_, String>>()?;
        Ok(Self { files })
    }

    /// What `pid` does with the watched files according to `locks`.
    pub fn usage(&self, locks: &[Lock], pid: u32) -> Usage {
        let mut usage = Usage::default();
        for lock in locks.iter().filter(|l| l.pid == i64::from(pid)) {
            for (path, _) in self.files.iter().filter(|(_, id)| *id == lock.file) {
                if lock.waiting {
                    usage.waits.push(path.clone());
                } else {
                    usage.holds.push((path.clone(), lock.mode.clone()));
                }
            }
        }
        usage
    }

    /// Every lock line on a watched file, whoever holds it.
    pub fn locks_on<'a>(&self, locks: &'a [Lock]) -> Vec<&'a Lock> {
        locks
            .iter()
            .filter(|l| self.files.iter().any(|(_, id)| *id == l.file))
            .collect()
    }

    /// Samples `/proc/locks` until `pid`'s usage satisfies `done`, at most
    /// `timeout`; returns that usage, or fails with the last one seen.
    pub fn wait_for(
        &self,
        pid: u32,
        timeout: Duration,
        what: &str,
        done: impl Fn(&Usage) -> bool,
    ) -> Result<Usage, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let usage = self.usage(&sample()?, pid);
            if done(&usage) {
                return Ok(usage);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "pid {pid} not {what} within {timeout:?}; last /proc/locks sample: it {usage}"
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1: FLOCK  ADVISORY  WRITE 1234 00:2a:5678 0 EOF
1: -> FLOCK  ADVISORY  WRITE 1235 00:2a:5678 0 EOF
2: FLOCK  ADVISORY  READ 1235 00:2a:5679 0 EOF
3: POSIX  ADVISORY  WRITE 999 fd:01:123 0 EOF
4: OFDLCK ADVISORY  READ -1 00:06:9531 0 EOF
";

    fn id(major: u32, minor: u32, ino: u64) -> FileId {
        FileId { major, minor, ino }
    }

    #[test]
    fn parses_holders_waiters_and_device_numbers_in_hex() {
        let locks = parse(SAMPLE).unwrap();
        assert_eq!(locks.len(), 5);
        assert_eq!(
            locks[0],
            Lock {
                waiting: false,
                class: "FLOCK".into(),
                mode: "WRITE".into(),
                pid: 1234,
                file: id(0, 0x2a, 5678),
            }
        );
        assert!(locks[1].waiting && locks[1].pid == 1235);
        assert_eq!(locks[3].file, id(0xfd, 1, 123));
        assert_eq!(locks[4].pid, -1);
    }

    #[test]
    fn an_unreadable_line_fails_the_sample() {
        assert!(parse("1: FLOCK ADVISORY WRITE x 00:2a:5678 0 EOF\n").is_err());
        assert!(parse("garbage\n").is_err());
        assert!(parse("1: FLOCK ADVISORY WRITE 1 00:2a 0 EOF\n").is_err());
        assert_eq!(parse("").unwrap(), vec![]);
    }

    #[test]
    fn usage_splits_held_and_awaited_watched_files_per_pid() {
        let locks = parse(SAMPLE).unwrap();
        let lock = PathBuf::from("/t/debug/.cargo-lock");
        let build = PathBuf::from("/t/debug/.cargo-build-lock");
        let watched = Watched {
            files: vec![
                (lock.clone(), id(0, 0x2a, 5678)),
                (build.clone(), id(0, 0x2a, 5679)),
            ],
        };
        let waiter = watched.usage(&locks, 1235);
        assert!(waiter.waits(&lock) && !waiter.holds(&lock));
        assert!(waiter.holds(&build));
        assert_eq!(waiter.holds, vec![(build.clone(), "READ".to_owned())]);
        let holder = watched.usage(&locks, 1234);
        assert!(holder.holds(&lock) && holder.waits.is_empty());
        assert!(watched.usage(&locks, 999).is_empty());
        assert_eq!(watched.locks_on(&locks).len(), 3);
    }
}
