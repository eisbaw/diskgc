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
        // Per-mount options only: the kernel prints `idmapped` there
        // (`show_vfsmnt_opts`), never among a superblock's options.
        idmapped: fields[5].split(|b| *b == b',').any(|o| o == b"idmapped"),
        upper_dir: if fields[sep + 1] == b"overlay" {
            overlay_upper_dir(fields[sep + 3])
                .map_err(|e| format!("{e} in mountinfo line: {}", shown()))?
        } else {
            None
        },
    })
}

/// The `upperdir=` value among an overlay's superblock options, unescaped.
/// overlayfs shows it with `seq_show_option`, which octal-escapes a comma,
/// `=`, whitespace and a backslash, so the options split at commas.
fn overlay_upper_dir(options: &[u8]) -> Result<Option<PathBuf>, String> {
    options
        .split(|b| *b == b',')
        .find_map(|option| option.strip_prefix(b"upperdir="))
        .map(|value| Ok(PathBuf::from(OsString::from_vec(unescape(value)?))))
        .transpose()
}

/// What the `hidepid` and `gid` superblock options of a procfs mount say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidepid {
    /// `hidepid` absent, `0` or `off`.
    Off,
    /// `noaccess` (`1`) or `invisible` (`2`): processes diskgc may not
    /// ptrace-read are hidden, except from members of `gid`.
    Hides { gid: Option<u32> },
    /// `ptraceable` (`4`): hidden from every reader that may not
    /// ptrace-read them; `gid` is not consulted.
    Ptraceable,
}

/// The `hidepid` of the procfs mount with id `mount_id` (its superblock
/// options). An unknown `hidepid` value counts as hiding. A table without
/// that mount, or a line for it that is not procfs, is an error: the
/// in-use check's and Owner PID's precondition is then undecided, never
/// assumed.
pub fn hidepid_of(text: &[u8], mount_id: u64) -> Result<Hidepid, String> {
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
        let options: Vec<&[u8]> = fields[sep + 3].split(|b| *b == b',').collect();
        let value = |key: &[u8]| {
            options
                .iter()
                .rev()
                .find_map(|option| option.strip_prefix(key))
        };
        return Ok(match value(b"hidepid=") {
            None | Some(b"0" | b"off") => Hidepid::Off,
            Some(b"4" | b"ptraceable") => Hidepid::Ptraceable,
            Some(b"1" | b"2" | b"noaccess" | b"invisible") => {
                let gid = match value(b"gid=") {
                    None => None,
                    Some(text) => Some(
                        std::str::from_utf8(text)
                            .ok()
                            .and_then(|t| t.parse::<u32>().ok())
                            .ok_or_else(|| {
                                format!(
                                    "mount {mount_id}: gid={} is not a number",
                                    String::from_utf8_lossy(text)
                                )
                            })?,
                    ),
                };
                Hidepid::Hides { gid }
            }
            Some(_) => Hidepid::Hides { gid: None },
        });
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
29 1 0:27 / /proc7 rw - proc proc rw,hidepid=4,gid=10
30 1 0:28 / /proc8 rw - proc proc rw,hidepid=1
31 1 0:29 / /proc9 rw - proc proc rw,hidepid=7
32 1 0:30 / /proc10 rw - proc proc rw,hidepid=2,gid=x
";
        for (id, hidden) in [
            (22, Hidepid::Off),
            (23, Hidepid::Hides { gid: None }),
            (24, Hidepid::Hides { gid: Some(10) }),
            (25, Hidepid::Off),
            (26, Hidepid::Off),
            (27, Hidepid::Ptraceable),
            (29, Hidepid::Ptraceable),
            (30, Hidepid::Hides { gid: None }),
            (31, Hidepid::Hides { gid: None }),
        ] {
            assert_eq!(hidepid_of(table, id), Ok(hidden), "mount {id}");
        }
        assert!(hidepid_of(table, 28).is_err(), "not proc");
        assert!(hidepid_of(table, 32).is_err(), "gid not a number");
        assert!(hidepid_of(table, 99).is_err(), "absent");
    }

    /// TASK-307 review: an overlay's `upperdir=` is read and unescaped
    /// (`seq_show_option` escapes the comma and the `=` in it); other types
    /// and an overlay without an upper layer have none.
    #[test]
    fn an_overlays_upper_dir_is_read_from_its_superblock_options() {
        let table = b"\
50 28 0:60 / /live rw - overlay overlay rw,lowerdir=/rofs,upperdir=/cow/up\\054per\\075x,workdir=/cow/work,uuid=on
51 28 0:61 / /ro rw - overlay overlay ro,lowerdir=/a:/b
52 28 254:1 / /e rw - ext4 /dev/x rw,upperdir=/nope
";
        let mounts = parse_mountinfo(table).expect("parses");
        assert_eq!(
            mounts[0].upper_dir.as_deref(),
            Some(Path::new("/cow/up,per=x"))
        );
        assert_eq!(mounts[1].upper_dir, None, "no upper layer");
        assert_eq!(mounts[2].upper_dir, None, "not an overlay");
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

    /// TASK-299: the kernel prints `idmapped` among the per-mount options
    /// (`show_vfsmnt_opts`, after `rw`/`ro`); a superblock option of that
    /// name (a filesystem's own text) is not the flag.
    #[test]
    fn the_idmapped_flag_is_read_from_the_per_mount_options() {
        let mounts = parse_mountinfo(
            b"\
50 28 254:1 /home/u /mnt/u rw,relatime,idmapped shared:9 - ext4 /dev/mapper/root rw
51 28 254:1 /home/u /mnt/v rw,relatime - ext4 /dev/mapper/root rw,idmapped
52 28 254:1 /home/u /mnt/w ro,nosuid,idmapped,nosymfollow - ext4 /dev/mapper/root rw
",
        )
        .expect("parses");
        let flags: Vec<bool> = mounts.iter().map(|m| m.idmapped).collect();
        assert_eq!(flags, [true, false, true]);
        assert!(
            parse_mountinfo(SAMPLE)
                .expect("parses")
                .iter()
                .all(|m| !m.idmapped)
        );
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
