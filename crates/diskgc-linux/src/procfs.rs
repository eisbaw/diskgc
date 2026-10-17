//! Parsers for `/proc/<pid>/maps` and `/proc/<pid>/environ` (proc(5)). The
//! readers that open these files are in [`crate::host`].

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use diskgc_core::host::{Dev, Environ, MapEntry};

/// Parses `maps`:
///
/// ```text
/// 7f2c4a1d2000-7f2c4a1f4000 r--p 00000000 fd:01 1314792    /usr/lib/libc.so.6
/// address                   perms offset  dev   inode      pathname
/// ```
///
/// `dev` is `major:minor` in hex, `inode` decimal (0 for anonymous
/// mappings). The pathname is everything after the padding (it may hold
/// spaces and end in ` (deleted)`). Any malformed line fails the parse.
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
    let path = trim_spaces(rest);
    Ok(MapEntry {
        dev: Dev {
            major: hex(major)?,
            minor: hex(minor)?,
        },
        ino,
        path: (!path.is_empty()).then(|| OsString::from_vec(path.to_vec())),
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
    let lines = |text: &[u8]| -> Vec<Vec<u8>> {
        text.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    };
    let mut union = lines(&first);
    let known: std::collections::HashSet<Vec<u8>> = union.iter().cloned().collect();
    union.extend(lines(&second).into_iter().filter(|l| !known.contains(l)));
    Ok(union
        .into_iter()
        .flat_map(|mut l| {
            l.push(b'\n');
            l
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(maps[0].path.as_deref(), Some("/usr/bin/cat".as_ref()));
        assert_eq!((maps[1].ino, maps[1].path.clone()), (0, None));
        assert_eq!(
            maps[2].dev,
            Dev {
                major: 0x103,
                minor: 0xa2
            }
        );
        assert_eq!(
            maps[2].path.as_deref(),
            Some("/tmp/a dir/lib.so (deleted)".as_ref())
        );
        assert_eq!(maps[3].path.as_deref(), Some("[stack]".as_ref()));
    }

    #[test]
    fn malformed_maps_lines_fail() {
        for bad in [
            &b"55d0-55d1 r--p 00000000 fd01 1 /x\n"[..],
            b"55d0-55d1 r--p 00000000 fd:01 x /x\n",
            b"55d0-55d1 r--p 00000000 zz:01 1 /x\n",
            b"55d0-55d1 r--p\n",
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
        let exe = std::fs::read_link("/proc/self/exe").expect("exe");
        assert!(
            maps.iter()
                .any(|m| m.ino != 0 && m.path.as_deref() == Some(exe.as_os_str())),
            "{exe:?} not mapped"
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
