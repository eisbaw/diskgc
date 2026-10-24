//! Parsers for `/proc/<pid>/maps`, `/proc/<pid>/environ` and
//! `/proc/<pid>/status` (proc(5)). The readers that open these files are in
//! [`crate::host`].

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use diskgc_core::host::{Dev, Environ, Ids, MapEntry};

/// Parses `maps`:
///
/// ```text
/// 7f2c4a1d2000-7f2c4a1f4000 r--p 00000000 fd:01 1314792    /usr/lib/libc.so.6
/// address                   perms offset  dev   inode      pathname
/// ```
///
/// `dev` is `major:minor` in hex, `inode` decimal (0 for anonymous
/// mappings). The pathname (everything after the padding; it may hold
/// spaces and end in ` (deleted)`) is not kept ([`MapEntry`]). Any
/// malformed line fails the parse.
pub fn parse_maps(text: &[u8]) -> Result<Vec<MapEntry>, String> {
    text.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_maps_line)
        .collect()
}

fn parse_maps_line(line: &[u8]) -> Result<MapEntry, String> {
    let shown = || String::from_utf8_lossy(line).into_owned();
    let mut rest = line;
    let mut fields: [&[u8]; 5] = [&[]; 5];
    for field in &mut fields {
        rest = trim_spaces(rest);
        let end = rest.iter().position(|b| *b == b' ').unwrap_or(rest.len());
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let [_, _, _, dev, inode] = fields;
    if inode.is_empty() {
        return Err(format!("no inode column in maps line: {}", shown()));
    }
    let text = |f| utf8(f, line);
    let (major, minor) = text(dev)?
        .split_once(':')
        .ok_or_else(|| format!("bad dev column in maps line: {}", shown()))?;
    let hex = |s: &str| {
        u32::from_str_radix(s, 16).map_err(|_| format!("bad dev column in maps line: {}", shown()))
    };
    let ino = text(inode)?
        .parse()
        .map_err(|_| format!("bad inode column in maps line: {}", shown()))?;
    Ok(MapEntry {
        dev: Dev {
            major: hex(major)?,
            minor: hex(minor)?,
        },
        ino,
    })
}

fn utf8<'a>(field: &'a [u8], line: &[u8]) -> Result<&'a str, String> {
    std::str::from_utf8(field)
        .map_err(|_| format!("bad maps line: {}", String::from_utf8_lossy(line)))
}

fn trim_spaces(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| *b != b' ').unwrap_or(s.len());
    &s[start..]
}

/// Parses `environ`: NUL-terminated `NAME=value` entries.
pub fn parse_environ(bytes: &[u8]) -> Environ {
    let mut env = Environ::default();
    let body = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if body.is_empty() {
        return env;
    }
    for entry in body.split(|b| *b == 0) {
        match entry.iter().position(|b| *b == b'=') {
            Some(eq) => env.vars.push((
                OsString::from_vec(entry[..eq].to_vec()),
                OsString::from_vec(entry[eq + 1..].to_vec()),
            )),
            None => env.without_equals.push(OsString::from_vec(entry.to_vec())),
        }
    }
    env
}

/// Reads a file whose content must come from one source in one `read`
/// call into a buffer of `first` bytes (`environ`: `environ_read` takes one
/// reference to the memory map per call and copies the whole environment
/// within the call, so one call gives one complete environment or
/// nothing). A full buffer may hide more: it is discarded and the file
/// opened again with twice the buffer, up to `max`; `None` when even that
/// was filled. A prefix is never returned.
pub fn read_in_one_call<F, E>(
    first: usize,
    max: usize,
    mut open: impl FnMut() -> Result<F, E>,
    mut read: impl FnMut(&mut F, &mut [u8]) -> Result<usize, E>,
) -> Result<Option<Vec<u8>>, E> {
    let mut size = first.max(1);
    while size <= max {
        let mut file = open()?;
        let mut buf = vec![0; size];
        let n = read(&mut file, &mut buf)?;
        if n < size {
            buf.truncate(n);
            return Ok(Some(buf));
        }
        size *= 2;
    }
    Ok(None)
}

/// Reads a `seq_file` whose records come from a source captured at open
/// (`maps`: the memory map). It cannot be read in one call (each `read`
/// gathers at most one internal buffer, and `m_start` takes the source
/// again per call, ending the file early once the source is gone). So the
/// file is read to the end, then read from the start once more on the same
/// open file: if that finds nothing, the source ended during or after the
/// read, and the content (possibly a prefix) is discarded: the result is
/// empty. A source that is still there now was there for every earlier
/// call (a memory map's users never come back from zero).
pub fn read_confirmed<F, E>(
    mut open: impl FnMut() -> Result<F, E>,
    mut read_to_end: impl FnMut(&mut F) -> Result<Vec<u8>, E>,
    mut read_from_start: impl FnMut(&mut F) -> Result<usize, E>,
) -> Result<Vec<u8>, E> {
    let mut file = open()?;
    let content = read_to_end(&mut file)?;
    if content.is_empty() || read_from_start(&mut file)? == 0 {
        return Ok(Vec::new());
    }
    Ok(content)
}

/// Reads a record file twice, each pass complete on its own (see
/// [`read_confirmed`]), and returns the union of their lines (the first
/// pass's lines, then those of the second it lacked). A record moved
/// between two `read` calls of one pass from beyond the pass's position to
/// before it (`maps`: `mremap` to a lower address; the next call resumes at
/// the address it reached, task_mmu.c `m_start`) is missed by that pass but
/// shows up in the other one, unless it moves again in step with it. Empty
/// when either pass is (a pass found nothing).
pub fn read_two_passes<E>(mut pass: impl FnMut() -> Result<Vec<u8>, E>) -> Result<Vec<u8>, E> {
    let first = pass()?;
    if first.is_empty() {
        return Ok(first);
    }
    let second = pass()?;
    if second.is_empty() {
        return Ok(second);
    }
    // Borrowed lines, one output buffer: a desktop's maps are some 10^5
    // lines per scan, and a copy per line made the static build's
    // allocator lock the scan's bottleneck (TASK-115). Two equal passes
    // (nearly always) have the first's lines as their union: no set.
    fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
        text.split(|b| *b == b'\n').filter(|l| !l.is_empty())
    }
    let mut union = Vec::with_capacity(first.len() + 1);
    let mut add = |line: &[u8]| {
        union.extend_from_slice(line);
        union.push(b'\n');
    };
    lines(&first).for_each(&mut add);
    if first != second {
        let known: std::collections::HashSet<&[u8]> = lines(&first).collect();
        lines(&second)
            .filter(|l| !known.contains(l))
            .for_each(&mut add);
    }
    Ok(union)
}

/// Lists a directory whose entries must come from one source in one
/// `getdents` call (a task's fd table: `proc_readfd_common` finds the task
/// once per call). `list(size)` makes one call with a buffer of `size`
/// bytes and returns its entries and whether the next call found nothing
/// (end of directory). If it did not, the listing may span two calls (two
/// tasks, after a non-leader exec): it is discarded and the directory
/// opened again with twice the buffer, up to `max`; `None` when even that
/// was not enough.
pub fn list_in_one_call<T, E>(
    first: usize,
    max: usize,
    mut list: impl FnMut(usize) -> Result<(Vec<T>, bool), E>,
) -> Result<Option<Vec<T>>, E> {
    let mut size = first.max(1);
    while size <= max {
        let (entries, complete) = list(size)?;
        if complete {
            return Ok(Some(entries));
        }
        size *= 2;
    }
    Ok(None)
}

/// `State:` and `Threads:` from one read of `/proc/<pid>/status`.
///
/// One read means one `show` of the file: `status` is a `single_open`
/// seq_file (fs/proc/base.c `ONE("status", ...)`), whose whole text is made
/// by one `proc_pid_status` call on the first `read` (fs/seq_file.c
/// `seq_read_iter`; a text too large for the buffer is made again, whole,
/// into a larger one), and later `read` calls only copy it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFields {
    /// The state letter of the task the read reached (`R`, `S`, `Z`, ...).
    pub state: char,
    /// The kernel's thread count, `signal->nr_threads`: the tasks of the
    /// group not yet released. 0 when the task reached was already released
    /// (`task_sig` in fs/proc/array.c prints 0 without signal handlers).
    pub threads: usize,
}

/// Parses `State:` and `Threads:` of `/proc/<pid>/status`.
pub fn parse_status(text: &[u8]) -> Result<StatusFields, String> {
    let text = String::from_utf8_lossy(text);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
            .ok_or_else(|| format!("no {name} line in status"))
    };
    let state = field("State:")?;
    let mut letters = state.chars();
    let state = match (letters.next(), letters.next()) {
        (Some(letter), None | Some(' ')) => letter,
        _ => return Err(format!("bad State: line in status: {state:?}")),
    };
    let threads = field("Threads:")?
        .parse()
        .map_err(|_| "bad Threads: line in status".to_owned())?;
    Ok(StatusFields { state, threads })
}

/// What [`parse_status_ids`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIds {
    pub name: OsString,
    pub uid: Ids,
    pub gid: Ids,
    pub cap_permitted: u64,
    pub cap_effective: u64,
}

/// The `NSpid:` line of one read of a `status` file: the pid in every
/// namespace from the `/proc` mount's down to the process's own. Every
/// field is a pid (1 or more). A missing, empty or malformed line is an
/// error, never a shorter list: a level left out could be the one that
/// matches.
pub fn parse_nspid_levels(text: &[u8]) -> Result<Vec<u32>, String> {
    let line = text
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"NSpid:"))
        .ok_or("has no NSpid line")?;
    let line = std::str::from_utf8(line).map_err(|_| "non-UTF-8 NSpid line")?;
    let levels = line
        .split_ascii_whitespace()
        .map(|field| match field.parse::<u32>() {
            Ok(0) | Err(_) => Err(format!("bad NSpid field {field:?}")),
            Ok(level) => Ok(level),
        })
        .collect::<Result<Vec<u32>, String>>()?;
    if levels.is_empty() {
        return Err("empty NSpid line".to_owned());
    }
    Ok(levels)
}

/// [`parse_nspid_levels`] of `/proc/<pid>/status` read through the held
/// `/proc`, which is diskgc's own namespace's (`LinuxHost::new`), so the
/// first level must be `pid` itself.
pub fn parse_nspid(text: &[u8], pid: u32) -> Result<Vec<u32>, String> {
    let levels = parse_nspid_levels(text)?;
    if levels[0] != pid {
        return Err(format!(
            "NSpid starts with {}, not the process's pid {pid}",
            levels[0]
        ));
    }
    Ok(levels)
}

/// `Name:`, `Uid:`, `Gid:`, `CapPrm:` and `CapEff:` of one read of
/// `/proc/<pid>/status` (the owner of its `fd` directory comes from a
/// `statx`, not from here).
pub fn parse_status_ids(text: &[u8]) -> Result<StatusIds, String> {
    let line = |name: &[u8]| {
        text.split(|b| *b == b'\n')
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("no {} line in status", String::from_utf8_lossy(name)))
    };
    let ids = |name: &[u8]| -> Result<Ids, String> {
        let shown = String::from_utf8_lossy(name).into_owned();
        let text = std::str::from_utf8(line(name)?)
            .map_err(|_| format!("non-UTF-8 {shown} line in status"))?;
        let values: Vec<u32> = text
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| format!("bad {shown} line in status: {text:?}"))?;
        match values[..] {
            [real, effective, saved, fs] => Ok(Ids {
                real,
                effective,
                saved,
                fs,
            }),
            _ => Err(format!("bad {shown} line in status: {text:?}")),
        }
    };
    let name = line(b"Name:")?;
    let name = name.strip_prefix(b"\t").unwrap_or(name);
    let caps = |name: &[u8]| -> Result<u64, String> {
        std::str::from_utf8(line(name)?)
            .ok()
            .and_then(|t| u64::from_str_radix(t.trim(), 16).ok())
            .ok_or_else(|| format!("bad {} line in status", String::from_utf8_lossy(name)))
    };
    Ok(StatusIds {
        name: OsString::from_vec(name.to_vec()),
        uid: ids(b"Uid:")?,
        gid: ids(b"Gid:")?,
        cap_permitted: caps(b"CapPrm:")?,
        cap_effective: caps(b"CapEff:")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nspid_lists_every_level_from_the_reader_down() {
        let status = b"Name:\tsh\nNSpid:\t3025375\t9\nNSgid:\t1\n";
        assert_eq!(parse_nspid(status, 3_025_375), Ok(vec![3_025_375, 9]));
        assert_eq!(parse_nspid(b"NSpid:\t42\n", 42), Ok(vec![42]));
    }

    /// A level left out could be the one that matches: every malformed
    /// line is an error.
    #[test]
    fn a_malformed_nspid_line_is_an_error() {
        for status in [
            &b"Name:\tsh\n"[..],
            b"NSpid:\n",
            b"NSpid:\t42\tx\n",
            b"NSpid:\t42\t0\n",
            b"NSpid:\t42\t-9\n",
            b"NSpid:\t42\t4294967296\n",
            b"NSpid:\t\xff\n",
            b"NSpid:\t43\t9\n",
        ] {
            assert!(
                parse_nspid(status, 42).is_err(),
                "{:?}",
                String::from_utf8_lossy(status)
            );
        }
    }

    #[test]
    fn maps_columns_are_parsed() {
        let text = b"\
55d0c0a00000-55d0c0a02000 r--p 00000000 fd:01 1314792                    /usr/bin/cat
7f2c4a000000-7f2c4a021000 rw-p 00000000 00:00 0
7f2c4a1d2000-7f2c4a1f4000 r-xp 00002000 103:a2 99    /tmp/a dir/lib.so (deleted)
7ffd5e9d4000-7ffd5e9f5000 rw-p 00000000 00:00 0                          [stack]
";
        let maps = parse_maps(text).expect("parses");
        assert_eq!(maps.len(), 4);
        assert_eq!(
            maps[0].dev,
            Dev {
                major: 0xfd,
                minor: 1
            }
        );
        assert_eq!(maps[0].ino, 1_314_792);
        assert_eq!(maps[1].ino, 0);
        assert_eq!(
            maps[2].dev,
            Dev {
                major: 0x103,
                minor: 0xa2
            }
        );
        // A path with spaces and ` (deleted)` does not disturb the columns.
        assert_eq!(maps[2].ino, 99);
        assert_eq!(maps[3].ino, 0);
    }

    #[test]
    fn malformed_maps_lines_fail() {
        for bad in [
            &b"55d0-55d1 r--p 00000000 fd01 1 /x\n"[..],
            b"55d0-55d1 r--p 00000000 fd:01 x /x\n",
            b"55d0-55d1 r--p 00000000 zz:01 1 /x\n",
            b"55d0-55d1 r--p\n",
            b"55d0-55d1 r--p 00000000 fd:01\n",
        ] {
            assert!(
                parse_maps(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn own_maps_parse_and_include_the_executable() {
        let text = std::fs::read("/proc/self/maps").expect("read maps");
        let maps = parse_maps(&text).expect("parses");
        // By inode alone: on btrfs and overlayfs `maps` can show another
        // device than `stat` (docs/safety.md, In use: Overlay and btrfs).
        let exe = std::os::unix::fs::MetadataExt::ino(
            &std::fs::metadata("/proc/self/exe").expect("stat exe"),
        );
        assert!(
            maps.iter().any(|m| m.ino == exe),
            "the executable (inode {exe}) is not mapped"
        );
    }

    #[test]
    fn environ_entries_split_at_the_first_equals() {
        let env = parse_environ(b"TMPDIR=/tmp/x\0EQ=a=b\0EMPTY=\0NOEQUALS\0");
        assert_eq!(env.get("TMPDIR"), Some("/tmp/x".as_ref()));
        assert_eq!(env.get("EQ"), Some("a=b".as_ref()));
        assert_eq!(env.get("EMPTY"), Some("".as_ref()));
        assert_eq!(env.get("NOEQUALS"), None);
        assert_eq!(env.without_equals, vec![OsString::from("NOEQUALS")]);
        assert_eq!(parse_environ(b""), Environ::default());
        // Non-UTF-8 values are kept byte for byte.
        let raw = parse_environ(b"P=/tmp/\xff\0");
        assert_eq!(raw.vars[0].1.as_encoded_bytes(), b"/tmp/\xff");
    }

    #[test]
    fn state_and_thread_count_are_read_from_status() {
        let text = b"Name:\tx\nState:\tZ (zombie)\nTgid:\t7\nThreads:\t12\nSigQ:\t0/1\n";
        assert_eq!(
            parse_status(text),
            Ok(StatusFields {
                state: 'Z',
                threads: 12
            })
        );
        assert!(parse_status(b"Name:\tx\nThreads:\t1\n").is_err());
        assert!(parse_status(b"State:\tS (sleeping)\n").is_err());
        assert!(parse_status(b"State:\tS\nThreads:\tmany\n").is_err());
        assert!(parse_status(b"State:\tSS\nThreads:\t1\n").is_err());
        // The reading thread itself is running.
        let own = std::fs::read("/proc/thread-self/status").expect("read own status");
        let own = parse_status(&own).expect("own status");
        assert!(own.threads >= 1 && own.state == 'R', "{own:?}");
    }

    #[test]
    fn name_and_ids_are_read_from_status() {
        let text = b"Name:\tsudo -i x\nUmask:\t0022\nState:\tS (sleeping)\n\
Uid:\t1000\t0\t0\t0\nGid:\t100\t100\t100\t100\nThreads:\t1\n\
CapInh:\t0000000000000000\nCapPrm:\t0000000800000000\nCapEff:\t0000000000000020\n";
        let StatusIds {
            name,
            uid,
            gid,
            cap_permitted,
            cap_effective,
        } = parse_status_ids(text).expect("parses");
        assert_eq!(cap_effective, 1 << 5);
        assert_eq!(name, OsString::from("sudo -i x"));
        assert_eq!(cap_permitted, 1 << 35);
        assert_eq!(
            uid,
            Ids {
                real: 1000,
                effective: 0,
                saved: 0,
                fs: 0
            }
        );
        assert_eq!(gid, Ids::same(100));
        for bad in [
            &b"Name:\tx\nGid:\t1\t1\t1\t1\nCapPrm:\t0\nCapEff:\t0\n"[..],
            b"Name:\tx\nUid:\t1\t1\t1\nGid:\t1\t1\t1\t1\nCapPrm:\t0\nCapEff:\t0\n",
            b"Name:\tx\nUid:\t1\t1\t1\tz\nGid:\t1\t1\t1\t1\nCapPrm:\t0\nCapEff:\t0\n",
            b"Uid:\t1\t1\t1\t1\nGid:\t1\t1\t1\t1\nCapPrm:\t0\nCapEff:\t0\n",
            b"Name:\tx\nUid:\t1\t1\t1\t1\nGid:\t1\t1\t1\t1\n",
            b"Name:\tx\nUid:\t1\t1\t1\t1\nGid:\t1\t1\t1\t1\nCapPrm:\tzz\nCapEff:\t0\n",
            b"Name:\tx\nUid:\t1\t1\t1\t1\nGid:\t1\t1\t1\t1\nCapPrm:\t0\n",
        ] {
            assert!(
                parse_status_ids(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        // This process: its real uid is the one it runs as.
        let own = std::fs::read("/proc/self/status").expect("read own status");
        let own = parse_status_ids(&own).expect("own status");
        assert_eq!(own.uid.real, rustix::process::getuid().as_raw());
    }

    #[test]
    fn a_full_buffer_is_never_returned_as_the_content() {
        // 4 bytes of "AAAAAAAA" fit a 4-byte buffer exactly: that may be a
        // prefix, so the file is read again with 8, then 16 bytes.
        let mut opens = 0;
        let got = read_in_one_call::<_, ()>(
            4,
            64,
            || {
                opens += 1;
                Ok(())
            },
            |_, buf: &mut [u8]| {
                let content = b"AAAAAAAA";
                let n = content.len().min(buf.len());
                buf[..n].copy_from_slice(&content[..n]);
                Ok(n)
            },
        );
        assert_eq!(got, Ok(Some(b"AAAAAAAA".to_vec())));
        assert_eq!(opens, 3, "4 and 8 bytes were full, 16 was not");
        // Never more than `max`: then None, not a prefix.
        let got = read_in_one_call::<_, ()>(4, 8, || Ok(()), |_, buf: &mut [u8]| Ok(buf.len()));
        assert_eq!(got, Ok(None));
    }

    #[test]
    fn a_prefix_ended_by_its_source_going_away_is_discarded() {
        // A prefix, then EOF (the memory map was released between reads);
        // reading from the start again finds nothing.
        let prefix = b"55d0-55d1 r--p 00000000 fd:01 7 /usr/bin/x\n".to_vec();
        let got = read_confirmed::<_, ()>(|| Ok(()), |_| Ok(prefix.clone()), |_| Ok(0));
        assert_eq!(got, Ok(Vec::new()), "a prefix must not be the content");
        // A source still there afterwards: the content stands.
        let got = read_confirmed::<_, ()>(|| Ok(()), |_| Ok(b"whole".to_vec()), |_| Ok(5));
        assert_eq!(got, Ok(b"whole".to_vec()));
        // Nothing read: nothing to confirm.
        let got = read_confirmed::<_, ()>(|| Ok(()), |_| Ok(Vec::new()), |_| panic!("no recheck"));
        assert_eq!(got, Ok(Vec::new()));
    }

    #[test]
    fn a_listing_that_needed_two_calls_is_never_returned() {
        // A 2-entry buffer is too small for 3 fds (the next call still finds
        // one); a 4-entry buffer holds them all.
        let fds = [0, 1, 2];
        let got = list_in_one_call::<_, ()>(2, 64, |size| {
            let n = size.min(fds.len());
            Ok((fds[..n].to_vec(), n == fds.len()))
        });
        assert_eq!(got, Ok(Some(vec![0, 1, 2])));
        let got = list_in_one_call::<u32, ()>(2, 4, |_| Ok((vec![0], false)));
        assert_eq!(got, Ok(None));
    }

    /// Two equal passes (the common case, taken without a set of lines)
    /// give their lines once each, every one ended by a newline, as two
    /// different passes would.
    #[test]
    fn equal_passes_give_their_lines_once() {
        let pass = b"1000-2000 r--p 0 fd:01 7 /a\n\n3000-4000 r--p 0 fd:01 8 /b".to_vec();
        let got = read_two_passes::<()>(|| Ok(pass.clone()));
        assert_eq!(
            got,
            Ok(b"1000-2000 r--p 0 fd:01 7 /a\n3000-4000 r--p 0 fd:01 8 /b\n".to_vec())
        );
    }

    #[test]
    fn a_record_moved_during_one_pass_is_found_by_the_other() {
        // Pass 1 missed the mapping of /x (moved below the pass's position
        // between two reads); pass 2 lists it at its new address.
        let passes = [
            b"1000-2000 r--p 0 fd:01 7 /a\n".to_vec(),
            b"0800-0900 r--p 0 fd:01 9 /x\n1000-2000 r--p 0 fd:01 7 /a\n".to_vec(),
        ];
        let mut n = 0;
        let got = read_two_passes::<()>(|| {
            n += 1;
            Ok(passes[n - 1].clone())
        });
        let maps = parse_maps(&got.unwrap()).unwrap();
        assert!(
            maps.iter().any(|m| m.ino == 9),
            "the moved mapping is missing: {maps:?}"
        );
        assert_eq!(maps.len(), 2, "no duplicates: {maps:?}");
        // A pass that found nothing: nothing.
        let mut n = 0;
        let got = read_two_passes::<()>(|| {
            n += 1;
            Ok(if n == 1 {
                b"1000-2000 r--p 0 fd:01 7 /a\n".to_vec()
            } else {
                Vec::new()
            })
        });
        assert_eq!(got, Ok(Vec::new()));
    }
}
