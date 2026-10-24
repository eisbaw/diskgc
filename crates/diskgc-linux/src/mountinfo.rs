//! Parsing `/proc/self/mountinfo` (proc(5)):
//!
//! ```text
//! 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! (1)(2)(3)   (4)   (5)      (6)      (7)   (8) (9)   (10)         (11)
//! ```
//!
//! The kernel escapes space, tab, newline and backslash in paths as `\ooo`
//! (`\040` for a space); they are decoded here, so a mount point with a
//! space compares equal to the path a user writes.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use diskgc_core::host::{Dev, MountEntry};

/// Parses the whole table. Any malformed line fails the parse: a mount
/// table diskgc cannot read completely is not used.
pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountEntry>, String> {
    text.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_line)
        .collect()
}

fn parse_line(line: &[u8]) -> Result<MountEntry, String> {
    let shown = || String::from_utf8_lossy(line).into_owned();
    let fields: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
    // Optional fields (7) end at the first "-" from field 7 on.
    let sep = fields
        .iter()
        .skip(6)
        .position(|f| *f == b"-")
        .map(|p| p + 6)
        .ok_or_else(|| format!("mountinfo line without ' - ' separator: {}", shown()))?;
    if fields.len() < sep + 4 {
        return Err(format!("mountinfo line too short: {}", shown()));
    }
    let number = |f: &[u8], what: &str| -> Result<u64, String> {
        utf8(f, line)?
            .parse()
            .map_err(|_| format!("bad {what} in mountinfo line: {}", shown()))
    };
    let (major, minor) = utf8(fields[2], line)?
        .split_once(':')
        .ok_or_else(|| format!("bad major:minor in mountinfo line: {}", shown()))?;
    let dev_part = |s: &str| {
        s.parse::<u32>()
            .map_err(|_| format!("bad major:minor in mountinfo line: {}", shown()))
    };
    let path = |f: &[u8]| -> Result<PathBuf, String> {
        let bytes = unescape(f).map_err(|e| format!("{e} in mountinfo line: {}", shown()))?;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    };
    let has_ro = |options: &[u8]| options.split(|b| *b == b',').any(|o| o == b"ro");
    Ok(MountEntry {
        id: number(fields[0], "mount id")?,
        parent_id: number(fields[1], "parent id")?,
        dev: Dev {
            major: dev_part(major)?,
            minor: dev_part(minor)?,
        },
        fs_root: path(fields[3])?,
        mount_point: path(fields[4])?,
        fs_type: utf8(fields[sep + 1], line)?.to_owned(),
        source: String::from_utf8_lossy(
            &unescape(fields[sep + 2])
                .map_err(|e| format!("{e} in mountinfo line: {}", shown()))?,
        )
        .into_owned(),
        read_only: has_ro(fields[5]) || has_ro(fields[sep + 3]),
    })
}

/// Whether the procfs mount with id `mount_id` hides processes: its
/// superblock options name a `hidepid` other than `0` / `off`
/// (`noaccess`, `invisible`, `ptraceable` and their numbers 1, 2, 4).
/// A table without that mount, or a line for it that is not procfs, is an
/// error: Owner PID's precondition is then undecided, never assumed.
pub fn hidepid_of(text: &[u8], mount_id: u64) -> Result<bool, String> {
    let mounts = text.split(|b| *b == b'\n').filter(|line| !line.is_empty());
    for line in mounts {
        let entry = parse_line(line)?;
        if entry.id != mount_id {
            continue;
        }
        if entry.fs_type != "proc" {
            return Err(format!("mount {mount_id} is {}, not proc", entry.fs_type));
        }
        let fields: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        let sep = fields
            .iter()
            .skip(6)
            .position(|f| *f == b"-")
            .map(|p| p + 6)
            .expect("parse_line found the separator");
        let hidden = fields[sep + 3]
            .split(|b| *b == b',')
            .filter_map(|option| option.strip_prefix(b"hidepid="))
            .any(|value| value != b"0" && value != b"off");
        return Ok(hidden);
    }
    Err(format!("no mount {mount_id} in mountinfo"))
}

fn utf8<'a>(field: &'a [u8], line: &[u8]) -> Result<&'a str, String> {
    std::str::from_utf8(field).map_err(|_| {
        format!(
            "non-UTF-8 field in mountinfo line: {}",
            String::from_utf8_lossy(line)
        )
    })
}

/// Decodes `\ooo` octal escapes. A backslash not followed by three octal
/// digits is an error rather than a guess.
pub fn unescape(field: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' {
            let digits = field
                .get(i + 1..i + 4)
                .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)))
                .ok_or_else(|| {
                    format!("bad octal escape in {:?}", String::from_utf8_lossy(field))
                })?;
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value).map_err(|_| {
                format!(
                    "octal escape above \\377 in {:?}",
                    String::from_utf8_lossy(field)
                )
            })?);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn hidepid_is_read_from_the_proc_mounts_superblock_options() {
        let table = b"\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
23 1 0:22 / /proc2 rw - proc proc rw,hidepid=invisible
24 1 0:23 / /proc3 rw - proc proc rw,hidepid=2,gid=10
25 1 0:24 / /proc4 rw - proc proc rw,hidepid=off,subset=pid
26 1 0:25 / /proc5 rw - proc proc rw,hidepid=0
27 1 0:26 / /proc6 rw - proc proc rw,hidepid=ptraceable
28 1 254:1 / / rw - ext4 /dev/root rw
";
        for (id, hidden) in [
            (22, false),
            (23, true),
            (24, true),
            (25, false),
            (26, false),
            (27, true),
        ] {
            assert_eq!(hidepid_of(table, id), Ok(hidden), "mount {id}");
        }
        assert!(hidepid_of(table, 28).is_err(), "not proc");
        assert!(hidepid_of(table, 99).is_err(), "absent");
    }

    const SAMPLE: &[u8] = b"\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
28 1 254:1 / / rw,relatime shared:1 - ext4 /dev/mapper/root rw
40 28 0:35 / /tmp rw,nosuid,nodev shared:17 - tmpfs tmpfs rw,size=8g
41 40 0:35 /src /tmp/with\\040space\\011tab\\134back rw master:3 propagate_from:2 - tmpfs tmp\\040fs ro
";

    #[test]
    fn fields_are_parsed_and_unescaped() {
        let mounts = parse_mountinfo(SAMPLE).expect("parses");
        assert_eq!(mounts.len(), 4);
        let proc = &mounts[0];
        assert_eq!((proc.id, proc.parent_id), (22, 1));
        assert_eq!(
            proc.dev,
            Dev {
                major: 0,
                minor: 21
            }
        );
        assert_eq!(proc.fs_type, "proc");
        assert!(!proc.read_only);

        let bind = &mounts[3];
        assert_eq!(bind.id, 41);
        assert_eq!(bind.fs_root, Path::new("/src"));
        assert_eq!(bind.mount_point, Path::new("/tmp/with space\ttab\\back"));
        assert_eq!(bind.source, "tmp fs");
        assert_eq!(bind.fs_type, "tmpfs");
        assert!(bind.read_only, "superblock option ro");
    }

    #[test]
    fn non_utf8_mount_points_survive() {
        let mounts = parse_mountinfo(b"7 1 0:5 / /tmp/\xff\\040x rw - tmpfs t rw\n").unwrap();
        assert_eq!(
            mounts[0].mount_point.as_os_str().as_encoded_bytes(),
            b"/tmp/\xff x"
        );
    }

    #[test]
    fn malformed_lines_fail_the_whole_table() {
        for bad in [
            &b"1 2 0:1 / /x rw overlay\n"[..],
            b"1 2 0:1 / /x\\04 rw - tmpfs t rw\n",
            b"1 2 0:1 / /x\\08a rw - tmpfs t rw\n",
            b"1 2 0:1 / /x\\777 rw - tmpfs t rw\n",
            b"1 2 0:1 / /x rw - tmpfs t\n",
            b"1 x 0:1 / /x rw - tmpfs t rw\n",
            b"1 2 01 / /x rw - tmpfs t rw\n",
        ] {
            let mut table = SAMPLE.to_vec();
            table.extend_from_slice(bad);
            assert!(
                parse_mountinfo(&table).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn this_process_mount_table_parses() {
        let text = std::fs::read("/proc/self/mountinfo").expect("read mountinfo");
        let mounts = parse_mountinfo(&text).expect("parses");
        assert!(mounts.iter().any(|m| m.mount_point == Path::new("/")));
    }
}
