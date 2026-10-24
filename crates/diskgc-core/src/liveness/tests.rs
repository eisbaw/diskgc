//! The pid grammar and the Owner PID answer (docs/testing.md, Producer
//! liveness vectors, Properties).

use proptest::prelude::*;

use super::*;

/// Every pid Linux can hand out reads back from its decimal form.
#[test]
fn every_valid_pid_parses_back() {
    for pid in 1..=PID_MAX_LIMIT {
        assert_eq!(parse_pid(pid.to_string().as_bytes()), Some(pid));
    }
}

#[test]
fn leading_zeros_signs_overflow_and_non_digits_never_parse() {
    for text in [
        "", "0", "00", "042", "+42", "-42", " 42", "42 ", "4194305", "99999999", "4x", "x4", "٤٢",
        "4\n", "１",
    ] {
        assert_eq!(parse_pid(text.as_bytes()), None, "{text:?}");
    }
}

#[test]
fn a_marker_is_a_pid_and_at_most_one_newline_in_sixteen_bytes() {
    assert_eq!(parse_pid_marker(b"42"), Some(42));
    assert_eq!(parse_pid_marker(b"42\n"), Some(42));
    for content in [
        &b""[..],
        b"\n",
        b"42\n\n",
        b"42 43",
        b"0042",
        b"42\r\n",
        b"abc",
        b"00000000000000042",
        b"42\n              ",
    ] {
        assert_eq!(
            parse_pid_marker(content),
            None,
            "{:?}",
            String::from_utf8_lossy(content)
        );
    }
}

proptest! {
    /// The grammar is exactly canonical decimal in range.
    #[test]
    fn parse_pid_accepts_only_canonical_decimal(text in proptest::collection::vec(any::<u8>(), 0..10)) {
        let canonical = std::str::from_utf8(&text)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|v| (1..=PID_MAX_LIMIT).contains(v) && v.to_string().as_bytes() == text.as_slice());
        prop_assert_eq!(parse_pid(&text), canonical);
    }

    #[test]
    fn parse_pid_digit_strings(digits in "[0-9]{1,9}") {
        let want = digits.parse::<u64>().ok()
            .filter(|v| (1..=u64::from(PID_MAX_LIMIT)).contains(v) && !digits.starts_with('0'))
            .map(|v| u32::try_from(v).unwrap());
        prop_assert_eq!(parse_pid(digits.as_bytes()), want);
    }
}

const TMPFS: u64 = 0x0102_1994;
const OVERLAY: u64 = 0x794C_7630;

/// One generated process: its levels, or unreadable.
#[derive(Debug, Clone)]
enum Generated {
    Levels(Vec<u32>),
    Unreadable(u32),
}

fn generated() -> impl Strategy<Value = Generated> {
    let small = 1u32..40;
    prop_oneof![
        6 => (small.clone(), proptest::collection::vec(small.clone(), 0..3))
            .prop_map(|(pid, inner)| Generated::Levels(std::iter::once(pid).chain(inner).collect())),
        1 => small.prop_map(Generated::Unreadable),
    ]
}

proptest! {
    /// Owner PID's answer over generated process tables and host facts:
    /// "ended" exactly when every precondition holds, the scan settled, no
    /// listed process carries the number at any level, none stayed
    /// unreadable, and the probe said ESRCH. A "may be in use" pid always
    /// carries the number (or is the number, from the probe).
    #[test]
    fn owner_pid_ends_only_on_a_complete_list(
        processes in proptest::collection::vec(generated(), 0..6),
        number in 1u32..40,
        initial in any::<bool>(),
        hidepid in any::<bool>(),
        namespace_readable in any::<bool>(),
        local in any::<bool>(),
        settled in any::<bool>(),
        probe in prop_oneof![Just(Ok(false)), Just(Ok(true)), Just(Err("EPERM".to_owned()))],
    ) {
        let namespace = if namespace_readable {
            Ok(PidNamespace { initial, hidepid })
        } else {
            Err("EACCES".to_owned())
        };
        let mut table = PidTable::new(namespace, settled);
        let mut carried = false;
        let mut unreadable = false;
        for p in &processes {
            match p {
                Generated::Levels(levels) => {
                    carried |= levels.contains(&number);
                    table.add_levels(levels);
                }
                Generated::Unreadable(pid) => {
                    unreadable = true;
                    table.add_unreadable(Pid(*pid));
                }
            }
        }
        let magic = if local { TMPFS } else { OVERLAY };
        let mut probed = false;
        let answer = owner_pid(&table, Ok(magic), number, |pid| {
            probed = true;
            assert_eq!(pid, Pid(number));
            probe.clone()
        });
        let preconditions = local && namespace_readable && initial && !hidepid;
        let ended = preconditions && settled && !carried && !unreadable && probe == Ok(false);
        prop_assert_eq!(answer == OwnerPid::Ended, ended, "{:?}", answer);
        if let OwnerPid::MayBeInUse(pid) = &answer {
            prop_assert!(preconditions);
            let carries = processes.iter().any(|p| matches!(p,
                Generated::Levels(levels) if levels[0] == pid.0 && levels.contains(&number)));
            prop_assert!(carries || (pid.0 == number && probe == Ok(true)));
        }
        if !preconditions {
            prop_assert!(matches!(answer, OwnerPid::Unknown(_)));
        }
        if carried && preconditions {
            prop_assert!(matches!(answer, OwnerPid::MayBeInUse(_)));
        }
        // The probe is the last word, never asked when another rule decided.
        prop_assert_eq!(probed, preconditions && !carried && settled && !unreadable);
    }
}

/// The TASK-39 counterexample's shape: a shell in a private PID namespace
/// records its inner pid 9; the host lists it as 3025375 with `NSpid`
/// `[3025375, 9]`. The inner level matches; the report names the outer pid.
#[test]
fn an_inner_namespace_level_matches_and_reports_the_outer_pid() {
    let mut table = PidTable::new(
        Ok(PidNamespace {
            initial: true,
            hidepid: false,
        }),
        true,
    );
    table.add_levels(&[3_025_375, 9]);
    let answer = owner_pid(&table, Ok(TMPFS), 9, |_| panic!("not probed"));
    assert_eq!(answer, OwnerPid::MayBeInUse(Pid(3_025_375)));
}

#[test]
fn unreadable_processes_are_every_blocker_sorted() {
    let mut table = PidTable::new(
        Ok(PidNamespace {
            initial: true,
            hidepid: false,
        }),
        true,
    );
    table.add_unreadable(Pid(91));
    table.add_unreadable(Pid(90));
    table.add_unreadable(Pid(91));
    let OwnerPid::Unknown(unknown) = owner_pid(&table, Ok(TMPFS), 4242, |_| panic!("not probed"))
    else {
        panic!("an unreadable process makes the answer unknown");
    };
    assert_eq!(unknown.blockers, [Pid(90), Pid(91)]);
}

#[test]
fn every_listed_local_filesystem_has_a_distinct_magic() {
    let mut magics: Vec<u64> = LOCAL_FILESYSTEMS.iter().map(|(m, _)| *m).collect();
    magics.sort_unstable();
    magics.dedup();
    assert_eq!(magics.len(), LOCAL_FILESYSTEMS.len());
    for name in ["tmpfs", "ext4", "xfs", "btrfs", "bcachefs", "f2fs"] {
        assert!(LOCAL_FILESYSTEMS.iter().any(|(_, n)| *n == name), "{name}");
    }
}

/// Facts read again after the scan must equal the first read: a `/proc`
/// remounted mid-scan, or a second read that fails, is unknown.
#[test]
fn namespace_facts_must_hold_over_the_whole_scan() {
    let clear = PidNamespace {
        initial: true,
        hidepid: false,
    };
    let hidden = PidNamespace {
        initial: true,
        hidepid: true,
    };
    for (before, after, trusted) in [
        (Ok(hidden), Ok(clear), false),
        (Ok(clear), Ok(hidden), false),
        (Ok(clear), Err("EIO".to_owned()), false),
        (Err("EIO".to_owned()), Ok(clear), false),
        (Ok(clear), Ok(clear), true),
    ] {
        let mut table = PidTable::new(before.clone(), true);
        table.confirm_namespace(after.clone());
        let answer = owner_pid(&table, Ok(TMPFS), 4242, |_| Ok(false));
        assert_eq!(answer == OwnerPid::Ended, trusted, "{before:?} {after:?}");
    }
}
