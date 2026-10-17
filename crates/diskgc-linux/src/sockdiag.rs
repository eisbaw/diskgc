//! The `NETLINK_SOCK_DIAG` query for AF_UNIX sockets bound to a filesystem
//! name (`Host::bound_unix_sockets`): the request and the parsing of the
//! kernel's answer, without I/O so both are unit-tested; the socket calls
//! are in [`crate::host`].
//!
//! Wire format (linux/netlink.h, linux/sock_diag.h, linux/unix_diag.h, all
//! in host byte order, every message and attribute aligned to 4 bytes):
//!
//! - request: `nlmsghdr` (len, type `SOCK_DIAG_BY_FAMILY`, flags
//!   `NLM_F_REQUEST | NLM_F_DUMP`, seq, pid 0) then `unix_diag_req`
//!   (family `AF_UNIX`, protocol 0, pad, states: all, ino 0, show
//!   `UDIAG_SHOW_VFS`, cookie 0);
//! - answer: datagrams of messages; each `SOCK_DIAG_BY_FAMILY` message is a
//!   `unix_diag_msg` (16 bytes) then attributes, of which `UNIX_DIAG_VFS`
//!   holds `unix_diag_vfs` { `udiag_vfs_ino`: u32, `udiag_vfs_dev`: u32, the
//!   kernel's internal `dev_t`, major in the bits above 20 }, present only
//!   for a socket bound to a filesystem name (net/unix/diag.c
//!   `sk_diag_dump_vfs`: the bound dentry's inode and superblock device);
//!   `NLMSG_DONE` ends the dump, `NLMSG_ERROR` fails it.
//!
//! A dump that the kernel marks interrupted (`NLM_F_DUMP_INTR`: the socket
//! table changed in a way that may have made it skip or repeat entries),
//! a message of the wrong sequence number, or a message cut short is an
//! error, never a shorter list.

use diskgc_core::host::{BoundSocket, Dev};

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_DUMP_INTR: u16 = 0x10;
const AF_UNIX: u8 = 1;
const UDIAG_SHOW_VFS: u32 = 0x2;
const UNIX_DIAG_VFS: u16 = 1;
/// `struct unix_diag_req`.
const REQ_LEN: usize = 24;
/// `struct unix_diag_msg`.
const MSG_LEN: usize = 16;
/// `NLA_TYPE_MASK`: the attribute type without the nested/byte-order bits.
const NLA_TYPE_MASK: u16 = 0x3fff;

/// The dump request with sequence number `seq`.
pub fn request(seq: u32) -> Vec<u8> {
    let len = NLMSG_HDRLEN + REQ_LEN;
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&u32::try_from(len).expect("small").to_ne_bytes());
    buf.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    buf.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    buf.extend_from_slice(&seq.to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes()); // pid: the kernel
    buf.push(AF_UNIX);
    buf.push(0); // protocol
    buf.extend_from_slice(&0u16.to_ne_bytes()); // pad
    buf.extend_from_slice(&u32::MAX.to_ne_bytes()); // every state
    buf.extend_from_slice(&0u32.to_ne_bytes()); // ino: any
    buf.extend_from_slice(&UDIAG_SHOW_VFS.to_ne_bytes());
    buf.extend_from_slice(&[0; 8]); // cookie
    buf
}

/// What one datagram of the answer held.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Datagram {
    /// The bound sockets it reported.
    pub sockets: Vec<BoundSocket>,
    /// It ended the dump (`NLMSG_DONE`).
    pub done: bool,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// Parses one datagram of the answer to the request with `seq`.
pub fn parse(buf: &[u8], seq: u32) -> Result<Datagram, String> {
    let mut out = Datagram::default();
    let mut at = 0;
    while at < buf.len() {
        if buf.len() - at < NLMSG_HDRLEN {
            return Err(format!("a message header cut short at byte {at}"));
        }
        let len = u32_at(buf, at) as usize;
        let kind = u16_at(buf, at + 4);
        let flags = u16_at(buf, at + 6);
        let got_seq = u32_at(buf, at + 8);
        if len < NLMSG_HDRLEN || len > buf.len() - at {
            return Err(format!(
                "a message of length {len} at byte {at} is cut short"
            ));
        }
        if got_seq != seq {
            return Err(format!("a message of sequence {got_seq}, expected {seq}"));
        }
        if flags & NLM_F_DUMP_INTR != 0 {
            return Err("the kernel marked the dump interrupted (NLM_F_DUMP_INTR)".to_owned());
        }
        let body = &buf[at + NLMSG_HDRLEN..at + len];
        match kind {
            NLMSG_DONE => {
                if body.len() >= 4 {
                    let status = i32::from_ne_bytes(body[..4].try_into().expect("4 bytes"));
                    if status < 0 {
                        return Err(format!("the dump ended with error {}", -status));
                    }
                }
                out.done = true;
                return Ok(out);
            }
            NLMSG_ERROR => {
                let errno = if body.len() >= 4 {
                    -i32::from_ne_bytes(body[..4].try_into().expect("4 bytes"))
                } else {
                    0
                };
                return Err(format!("the kernel refused the request: errno {errno}"));
            }
            SOCK_DIAG_BY_FAMILY => {
                if let Some(socket) = parse_socket(body)? {
                    out.sockets.push(socket);
                }
            }
            other => return Err(format!("unexpected message type {other}")),
        }
        at += align4(len);
    }
    Ok(out)
}

/// One `unix_diag_msg` and its attributes: the bound file's identity, or
/// `None` for a socket without a filesystem name.
fn parse_socket(body: &[u8]) -> Result<Option<BoundSocket>, String> {
    if body.len() < MSG_LEN {
        return Err(format!(
            "a socket message of {} bytes is cut short",
            body.len()
        ));
    }
    if body[0] != AF_UNIX {
        return Err(format!("a socket of family {}, not AF_UNIX", body[0]));
    }
    let mut found = None;
    let mut at = MSG_LEN;
    while at < body.len() {
        if body.len() - at < 4 {
            return Err(format!("an attribute header cut short at byte {at}"));
        }
        let len = usize::from(u16_at(body, at));
        let kind = u16_at(body, at + 2) & NLA_TYPE_MASK;
        if len < 4 || len > body.len() - at {
            return Err(format!(
                "an attribute of length {len} at byte {at} is cut short"
            ));
        }
        if kind == UNIX_DIAG_VFS {
            if len != 4 + 8 {
                return Err(format!("a UNIX_DIAG_VFS attribute of length {len}"));
            }
            let ino = u32_at(body, at + 4);
            let dev = u32_at(body, at + 8);
            found = Some(BoundSocket {
                dev: Dev {
                    major: dev >> 20,
                    minor: dev & 0xf_ffff,
                },
                ino,
            });
        }
        at += align4(len);
    }
    Ok(found)
}

/// The union of two complete dumps (`dump` called twice): a dump is not a
/// snapshot (a socket closed during it can make it skip a stable one), so
/// a socket bound throughout is missed only if both dumps miss it.
pub fn union_of_two<E>(
    mut dump: impl FnMut() -> Result<Vec<BoundSocket>, E>,
) -> Result<Vec<BoundSocket>, E> {
    let mut sockets = dump()?;
    sockets.extend(dump()?);
    sockets.sort_unstable();
    sockets.dedup();
    Ok(sockets)
}

/// The length of a datagram received with `MSG_TRUNC` into a buffer of
/// `capacity` bytes: the kernel returns the real length, so one longer
/// than the buffer was cut short.
pub fn whole_datagram(len: usize, capacity: usize) -> Result<usize, String> {
    if len > capacity {
        Err(format!(
            "a datagram of {len} bytes did not fit the {capacity}-byte buffer"
        ))
    } else {
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(len: usize, kind: u16, flags: u16, seq: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&u32::try_from(len).unwrap().to_ne_bytes());
        b.extend_from_slice(&kind.to_ne_bytes());
        b.extend_from_slice(&flags.to_ne_bytes());
        b.extend_from_slice(&seq.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        b
    }

    /// A socket message, with a VFS attribute if `vfs` is given, and an
    /// unrelated attribute of odd length before it (padding is skipped).
    fn socket(seq: u32, vfs: Option<(u32, u32)>, flags: u16) -> Vec<u8> {
        let mut body = vec![AF_UNIX, 1, 10, 0];
        body.extend_from_slice(&4242u32.to_ne_bytes()); // sockfs inode
        body.extend_from_slice(&[0; 8]);
        // UNIX_DIAG_NAME (0), 5 bytes of payload, padded to 8.
        body.extend_from_slice(&9u16.to_ne_bytes());
        body.extend_from_slice(&0u16.to_ne_bytes());
        body.extend_from_slice(b"/x/s\0\0\0\0");
        if let Some((ino, dev)) = vfs {
            body.extend_from_slice(&12u16.to_ne_bytes());
            body.extend_from_slice(&UNIX_DIAG_VFS.to_ne_bytes());
            body.extend_from_slice(&ino.to_ne_bytes());
            body.extend_from_slice(&dev.to_ne_bytes());
        }
        let mut m = header(NLMSG_HDRLEN + body.len(), SOCK_DIAG_BY_FAMILY, flags, seq);
        m.extend_from_slice(&body);
        m
    }

    fn done(seq: u32, status: i32) -> Vec<u8> {
        let mut m = header(NLMSG_HDRLEN + 4, NLMSG_DONE, 0x2, seq);
        m.extend_from_slice(&status.to_ne_bytes());
        m
    }

    #[test]
    fn the_request_asks_for_every_unix_socket_with_its_file() {
        let r = request(7);
        assert_eq!(r.len(), 40);
        assert_eq!(u32_at(&r, 0), 40);
        assert_eq!(u16_at(&r, 4), SOCK_DIAG_BY_FAMILY);
        assert_eq!(u16_at(&r, 6), NLM_F_REQUEST | NLM_F_DUMP);
        assert_eq!(u32_at(&r, 8), 7);
        assert_eq!(r[16], AF_UNIX);
        assert_eq!(u32_at(&r, 20), u32::MAX, "every state");
        assert_eq!(u32_at(&r, 28), UDIAG_SHOW_VFS);
    }

    #[test]
    fn bound_sockets_are_read_and_unbound_ones_skipped() {
        // dev 0:42 (tmpfs) and 259:3 (a disk) in the kernel's encoding.
        let mut buf = socket(7, Some((100, 42)), 0x2);
        buf.extend(socket(7, None, 0x2));
        buf.extend(socket(7, Some((u32::MAX, (259 << 20) | 3)), 0x2));
        let got = parse(&buf, 7).expect("parses");
        assert!(!got.done);
        assert_eq!(
            got.sockets,
            vec![
                BoundSocket {
                    dev: Dev {
                        major: 0,
                        minor: 42
                    },
                    ino: 100
                },
                BoundSocket {
                    dev: Dev {
                        major: 259,
                        minor: 3
                    },
                    ino: u32::MAX
                },
            ]
        );
        let end = parse(&done(7, 0), 7).expect("done");
        assert!(end.done && end.sockets.is_empty());
    }

    #[test]
    fn anything_doubtful_fails_the_dump() {
        let ok = socket(7, Some((100, 42)), 0x2);
        // Interrupted, another sequence, cut short, an error, a failed end.
        let mut cases = vec![
            (
                "interrupted",
                socket(7, Some((100, 42)), 0x2 | NLM_F_DUMP_INTR),
            ),
            ("sequence", socket(8, Some((100, 42)), 0x2)),
            ("cut", ok[..ok.len() - 3].to_vec()),
            ("done error", done(7, -16)),
        ];
        let mut error = header(NLMSG_HDRLEN + 4, NLMSG_ERROR, 0, 7);
        error.extend_from_slice(&(-13i32).to_ne_bytes());
        cases.push(("error", error));
        let mut long = ok.clone();
        long[0] = 200;
        cases.push(("length", long));
        for (what, buf) in cases {
            assert!(parse(&buf, 7).is_err(), "{what}");
        }
    }

    /// A socket one dump skipped (a close shifted the bucket under it) is
    /// in the union as long as the other dump saw it.
    #[test]
    fn the_answer_is_the_union_of_two_dumps() {
        let socket = |ino| BoundSocket {
            dev: Dev {
                major: 0,
                minor: 42,
            },
            ino,
        };
        let mut dumps = vec![vec![socket(1), socket(3)], vec![socket(2), socket(3)]].into_iter();
        let got = union_of_two::<()>(|| Ok(dumps.next().expect("two dumps")));
        assert_eq!(got, Ok(vec![socket(1), socket(2), socket(3)]));
        // Either dump failing fails the answer.
        let mut n = 0;
        let got = union_of_two(|| {
            n += 1;
            if n == 2 {
                Err("cut short")
            } else {
                Ok(vec![socket(1)])
            }
        });
        assert_eq!(got, Err("cut short"));
    }

    #[test]
    fn a_datagram_longer_than_the_buffer_is_an_error() {
        assert_eq!(whole_datagram(100, 100), Ok(100));
        assert!(whole_datagram(101, 100).is_err());
    }
}
