//! Binding of the system Nix daemon (kind `nix-store`, docs/safety.md,
//! nix-store). diskgc never names a store path to delete: Nix decides
//! liveness. This module decides only whether the one bounded garbage
//! collection a tier runs goes to the daemon at the fixed socket, whose root
//! database counts the system's generations, through an admitted CppNix
//! client and daemon, with the store on the requested filesystem.
//!
//! - [`inspect`] is the preflight (steps 1 to 6 of the binding protocol),
//!   read-only queries on [`Host`], also run by a dry run and again right
//!   before the GC (step 7: the walk compares the two [`Bound`]s). It
//!   returns the socket it pinned (`O_PATH`), which the deleting runs that
//!   follow go through.
//! - [`generations`] and [`check_roots`] are step 8, the root check: every
//!   generation link diskgc sees under `/nix/var/nix/profiles` must be a
//!   root of the daemon that will collect.
//! - The argv of every Nix child ([`query_argv`], [`roots_argv`],
//!   [`gc_argv`]) and the child environment ([`child_environment`]) are
//!   constants here; `LinuxHost` runs nothing else (the closed set of
//!   docs/safety.md, GC roots, profiles and generations are never removed).
//!
//! What the binding does not prove (a second root database over the same
//! physical store, the daemon's physical store directory, one daemon from
//! step 8 to step 9) is stated in docs/safety.md.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::backend::{Skipped, skip};
use crate::engine::Reason;
use crate::host::{
    Access, Base, DaemonProbe, DevIno, DirListing, Errno, FileType, Host, HostError, NixClient,
    NixEnvironment, NixQuery, OpenHow, ResolveFlags, Stat,
};
use crate::places::{Space, space_at};
use crate::resolve::{self, ResolveError, Resolved, Want};

/// Daemon versions whose GC and root semantics the container scenarios
/// checked (docs/reference.md, Backend version allowlist): the version
/// `nix store info --json` reports.
pub const ADMITTED_DAEMON_VERSIONS: &[&str] = &["2.34.8"];

/// Client versions whose command line and `--query --roots` output the
/// container scenarios checked: the exact `nix-store --version` line is
/// `nix-store (Nix) <version>`.
pub const ADMITTED_CLIENT_VERSIONS: &[&str] = &["2.34.8"];

/// The only store diskgc collects.
pub const STORE_DIR: &str = "/nix/store";
/// The daemon's state directory diskgc binds (its root database).
pub const STATE_DIR: &str = "/nix/var/nix";
/// The system daemon's socket.
pub const SOCKET: &str = "/nix/var/nix/daemon-socket/socket";
/// The directories above the socket: owned by root, writable by neither
/// group nor others, so only root can have bound or replaced the socket.
pub const SOCKET_ANCESTORS: [&str; 4] = [
    "/nix",
    "/nix/var",
    "/nix/var/nix",
    "/nix/var/nix/daemon-socket",
];
/// Where the system's generations live (step 8).
pub const PROFILES: &str = "/nix/var/nix/profiles";
/// The per-user profile directories below [`PROFILES`].
pub const PER_USER: &str = "per-user";

/// How long the daemon probe waits for the magic answer (step 4).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Entries listed per profiles directory before the root check refuses
/// (step 8).
pub const MAX_PROFILE_ENTRIES: usize = 4096;
/// Generation links checked at most, in total, before the root check
/// refuses rather than samples (step 8).
pub const MAX_GENERATIONS: usize = 1024;
/// Where the installation's own client is found, in order (step 1): the
/// NixOS system profile, then root's default profile (the multi-user
/// installer's). Both are root's choice; diskgc's `PATH` is never consulted,
/// so a user's own directory cannot pick the client.
pub const CLIENT_ANCHORS: [&str; 2] = [
    "/run/current-system/sw/bin/nix-store",
    "/nix/var/nix/profiles/default/bin/nix-store",
];

/// The `store` setting values that select the system daemon (step 5), as
/// `nix config show store` prints them.
pub const DAEMON_STORES: [&str; 3] = ["auto", "daemon", "unix:///nix/var/nix/daemon-socket/socket"];
/// Store URL schemes of remote stores (`remote_backend`, step 5).
pub const REMOTE_SCHEMES: [&str; 6] = ["ssh", "ssh-ng", "http", "https", "s3", "gs"];

/// Every Nix command's first arguments: no plugin is loaded, whatever a
/// configuration says (`plugin-files` from `NIX_CONFIG` or a user's
/// `nix.conf` would run its code in the query before the command does;
/// observed on 2.34.8: this flag first stops the load, for `nix` and the
/// legacy `nix-store`).
const PLUGINS_OFF: [&str; 3] = ["--option", "plugin-files", ""];

/// Every `nix` command's next arguments: a stock `/etc/nix/nix.conf` does
/// not enable `nix-command`, and the child reads no other configuration.
const NIX_COMMAND: [&str; 2] = ["--extra-experimental-features", "nix-command"];

/// The store URL of the pinned socket inherited as descriptor `fd`.
pub fn pinned_store(fd: impl std::fmt::Display) -> String {
    format!("unix:///proc/self/fd/{fd}")
}

/// The program (in the client directory) and arguments of `query`;
/// `store` is [`pinned_store`]'s URL, used only by a pinned query.
pub fn query_argv(query: NixQuery, store: &str) -> (&'static str, Vec<OsString>) {
    let (program, words): (&str, Vec<&str>) = match query {
        NixQuery::ClientVersion => ("nix-store", vec!["--version"]),
        NixQuery::ConfigShowStore => (
            "nix",
            NIX_COMMAND
                .into_iter()
                .chain(["config", "show", "store"])
                .collect(),
        ),
        NixQuery::StoreInfo => (
            "nix",
            NIX_COMMAND
                .into_iter()
                .chain(["store", "info", "--json", "--store", store])
                .collect(),
        ),
    };
    (
        program,
        PLUGINS_OFF
            .into_iter()
            .chain(words)
            .map(OsString::from)
            .collect(),
    )
}

/// `nix-store`'s arguments for the root check (step 8). `targets` are
/// store paths directly in `/nix/store` ([`is_store_path`]); anything else
/// is refused, so no option can be smuggled in as a target.
pub fn roots_argv(store: &str, targets: &[PathBuf]) -> Result<Vec<OsString>, String> {
    if targets.is_empty() {
        return Err("the root check needs at least one target".to_owned());
    }
    if let Some(bad) = targets.iter().find(|target| !is_store_path(target)) {
        return Err(format!(
            "{} is not a store path directly in {STORE_DIR}",
            bad.display()
        ));
    }
    Ok(PLUGINS_OFF
        .into_iter()
        .chain(["--store", store, "--query", "--roots"])
        .map(OsString::from)
        .chain(targets.iter().map(|target| target.as_os_str().to_owned()))
        .collect())
}

/// `nix-store`'s arguments for the GC (step 9). `max_freed` 0 is refused:
/// Nix deletes nothing with `--max-freed 0`.
pub fn gc_argv(store: &str, max_freed: u64) -> Result<Vec<OsString>, String> {
    if max_freed == 0 {
        return Err("--max-freed 0 deletes nothing; the need is at least 1".to_owned());
    }
    Ok(PLUGINS_OFF
        .into_iter()
        .map(str::to_owned)
        .chain([
            "--store".to_owned(),
            store.to_owned(),
            "--gc".to_owned(),
            "--max-freed".to_owned(),
            max_freed.to_string(),
        ])
        .map(OsString::from)
        .collect())
}

/// The child environment of every pinned query, the root check and the GC:
/// exactly `HOME` (diskgc's, when set) and `NIX_USER_CONF_FILES=` (no
/// per-user configuration). Nothing else: `NIX_REMOTE`, `NIX_CONFIG`,
/// `NIX_STORE_DIR`, `NIX_STATE_DIR`, `NIX_DAEMON_SOCKET_PATH` and
/// `NIX_CONF_DIR` are gone, so only the root-owned `/etc/nix/nix.conf` and
/// the constant command line apply.
pub fn child_environment(home: Option<&OsStr>) -> Vec<(OsString, OsString)> {
    let mut environment = Vec::with_capacity(2);
    if let Some(home) = home {
        environment.push(("HOME".into(), home.to_owned()));
    }
    environment.push(("NIX_USER_CONF_FILES".into(), OsString::new()));
    environment
}

/// Whether `path` is `/nix/store/<name>`: one normal component below the
/// store directory, not starting with `.`.
pub fn is_store_path(path: &Path) -> bool {
    let mut components = path.components();
    let prefix: PathBuf = components.by_ref().take(3).collect();
    let name = components.next();
    prefix == Path::new(STORE_DIR)
        && components.next().is_none()
        && matches!(name, Some(Component::Normal(name)) if !name.as_encoded_bytes().starts_with(b"."))
}

/// What diskgc binds of the daemon: compared in full again right before the
/// GC (step 7), so another client, daemon version, socket or store is
/// `changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// `/nix/store/<hash>-<name>/bin`: where `nix` and `nix-store` run
    /// from, never a `PATH` lookup again.
    pub client_dir: PathBuf,
    /// The version in the client's `nix-store --version` line.
    pub client_version: String,
    /// The daemon's version, as `nix store info --json` reports it.
    pub daemon_version: String,
    /// Whether the daemon trusts diskgc's user (`None`: not reported).
    pub trusted: Option<bool>,
    /// The store directory's identity.
    pub store: DevIno,
    /// The socket inode the preflight pinned.
    pub socket: DevIno,
    /// The client's `nix` file (what every child executes).
    pub client_nix: DevIno,
}

/// A binding with the socket it pinned: the root check and the GC go
/// through this descriptor, so a socket replaced after the recheck is not
/// reached.
#[derive(Debug)]
pub struct Pinned<F> {
    pub bound: Bound,
    pub socket: F,
    /// The client checked in step 1, executed through its descriptor.
    pub client: NixClient<F>,
}

pub type Binding<F> = crate::backend::Binding<Pinned<F>>;

/// The preflight, steps 1 to 6 (docs/safety.md, nix-store: Binding
/// protocol): the first failing step decides the skip; an error is what
/// no step identifies. `target` is the requested filesystem's space.
///
/// One deviation in order from the protocol's numbering: the client's
/// `nix-store --version` (step 6's client half) runs right after the
/// environment check of step 5 and before `nix config show store`, so a
/// client of another variant (Lix, whose command line differs) is refused
/// by its version line before any other query of it runs.
pub fn inspect<H: Host>(host: &H, target: Space) -> Result<Binding<H::Fd>, HostError> {
    binding(host, target, None)
}

/// Step 7, right before the root check and the GC: the preflight again,
/// each fact compared with `bound` as soon as it is read, before any
/// admission rule judges it, so a different client, store, socket inode,
/// client or daemon version is `changed` (a daemon upgraded to a version
/// that is not admitted too), and nothing is run. The returned socket is
/// the one the root check and the GC go through.
pub fn recheck<H: Host>(
    host: &H,
    target: Space,
    bound: &Bound,
) -> Result<Binding<H::Fd>, HostError> {
    binding(host, target, Some(bound))
}

/// The `changed` skip of a recheck that read `what` other than bound.
fn changed(what: &str, bound: impl std::fmt::Display, now: impl std::fmt::Display) -> Skipped {
    Skipped {
        reason: Reason::Changed,
        detail: format!("the Nix {what} changed before the GC: bound {bound}, now {now}"),
    }
}

/// [`inspect`], or with `expected` [`recheck`].
fn binding<H: Host>(
    host: &H,
    target: Space,
    expected: Option<&Bound>,
) -> Result<Binding<H::Fd>, HostError> {
    macro_rules! same {
        ($field:ident, $what:expr, $value:expr) => {
            if let Some(expected) = expected
                && expected.$field != $value
            {
                return Ok(crate::backend::Binding::Skipped(changed(
                    $what,
                    format!("{:?}", expected.$field),
                    format!("{:?}", $value),
                )));
            }
        };
    }
    // Step 1: the client.
    let Client {
        client,
        nix: client_nix,
    } = match find_client(host)? {
        Ok(client) => client,
        Err(skipped) => return Ok(crate::backend::Binding::Skipped(skipped)),
    };
    same!(client_dir, "client directory", client.dir);
    same!(client_nix, "client binary", client_nix);
    let environment = host.nix_environment();
    let child = child_environment(environment.home.as_deref());
    // Step 2: the layout, and the socket pinned.
    let store = match resolve_layout_dir(host, Path::new(STORE_DIR), LayoutRole::Store)? {
        Ok(store) => store,
        Err(skipped) => return Ok(crate::backend::Binding::Skipped(skipped)),
    };
    // Identity first (step 7 compares device, inode, owner and type):
    // a store or socket replaced since the preflight is `changed`,
    // whatever the new one is.
    let store_id = dev_ino(&store.stat, STORE_DIR)?;
    same!(store, "store directory", store_id);
    if store.stat.uid != Some(0) || store.stat.perm.is_none_or(|perm| perm & 0o1000 == 0) {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!(
                "{STORE_DIR} is owned by uid {} with mode {:o}: not the multi-user layout \
                 (root, sticky bit); single-user Nix is not collected",
                shown_id(store.stat.uid),
                store.stat.perm.unwrap_or_default()
            ),
        ));
    }
    for ancestor in SOCKET_ANCESTORS {
        let dir = match resolve_layout_dir(host, Path::new(ancestor), LayoutRole::SocketAncestor)? {
            Ok(dir) => dir,
            Err(skipped) => return Ok(crate::backend::Binding::Skipped(skipped)),
        };
        if dir.stat.uid != Some(0) || dir.stat.perm.is_none_or(|perm| perm & 0o022 != 0) {
            return Ok(skip(
                Reason::UnsupportedBackend,
                format!(
                    "{ancestor} is owned by uid {} with mode {:o}: only a directory of root's \
                     that group and others cannot write keeps the daemon socket root's",
                    shown_id(dir.stat.uid),
                    dir.stat.perm.unwrap_or_default()
                ),
            ));
        }
    }
    let socket = match resolve_layout_dir(host, Path::new(SOCKET), LayoutRole::Socket)? {
        Ok(socket) => socket,
        Err(skipped) => return Ok(crate::backend::Binding::Skipped(skipped)),
    };
    let socket_id = dev_ino(&socket.stat, SOCKET)?;
    same!(socket, "daemon socket inode", socket_id);
    if socket.stat.file_type != Some(FileType::Socket) || socket.stat.uid != Some(0) {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!(
                "{SOCKET} is a {} owned by uid {}: not a socket of root's",
                socket
                    .stat
                    .file_type
                    .map_or_else(|| "file of unknown type".to_owned(), |t| t.to_string()),
                shown_id(socket.stat.uid)
            ),
        ));
    }
    // Step 3: the filesystem.
    let space = space_at(host, &store.mount, &store.fd);
    if !space.same_as(&target) {
        return Ok(skip(
            Reason::OtherFs,
            format!("{STORE_DIR} is on {space}, requested filesystem is {target}"),
        ));
    }
    // Step 4: a daemon serving diskgc's user at the pinned socket.
    match host.nix_daemon_probe(&socket.fd, PROBE_TIMEOUT) {
        Ok(DaemonProbe::Serving) => {}
        Ok(DaemonProbe::Eof) => {
            return Ok(skip(
                Reason::BackendUnavailable,
                format!(
                    "the Nix daemon at {SOCKET} closed the connection before its answer \
                     (allowed-users refuses diskgc's user, or a socket-activated daemon \
                     failed to start)"
                ),
            ));
        }
        Ok(DaemonProbe::Timeout) => {
            return Ok(skip(
                Reason::BackendUnavailable,
                format!(
                    "the Nix daemon at {SOCKET} did not answer within {}s",
                    PROBE_TIMEOUT.as_secs()
                ),
            ));
        }
        Ok(DaemonProbe::WrongMagic(magic)) => {
            return Err(HostError::invalid(
                "probe Nix daemon",
                SOCKET,
                format!("answered {magic:#x}, not the worker protocol's magic number"),
            ));
        }
        Err(error) if error.is(Errno::ECONNREFUSED) || error.is(Errno::EACCES) => {
            return Ok(skip(
                Reason::BackendUnavailable,
                format!("the Nix daemon at {SOCKET} is not serving diskgc's user: {error}"),
            ));
        }
        Err(error) => return Err(error),
    }
    // Step 5: variables that redirect `daemon` and `auto` unseen.
    if let Some(skipped) = redirecting_variable(&environment) {
        return Ok(crate::backend::Binding::Skipped(skipped));
    }
    // Step 6, client half (see the deviation in order above).
    let version_line = host.nix_query(&client, NixQuery::ClientVersion, None, Some(&child))?;
    if let Some(expected) = expected
        && version_line != format!("nix-store (Nix) {}\n", expected.client_version)
    {
        return Ok(crate::backend::Binding::Skipped(changed(
            "client version",
            &expected.client_version,
            version_line.trim_end(),
        )));
    }
    let client_version = match client_version(&version_line) {
        Ok(version) => version,
        Err(skipped) => return Ok(crate::backend::Binding::Skipped(skipped)),
    };
    // Step 5: the caller's store selection, then the pinned daemon.
    let selected = host.nix_query(&client, NixQuery::ConfigShowStore, None, None)?;
    if let Some(skipped) = store_selection(&selected)
        .map_err(|why| HostError::invalid("query Nix", "config show store", why))?
    {
        return Ok(crate::backend::Binding::Skipped(skipped));
    }
    let info = host.nix_query(&client, NixQuery::StoreInfo, Some(&socket.fd), Some(&child))?;
    let info = StoreInfo::parse(&info)
        .map_err(|why| HostError::invalid("query Nix", "store info", why))?;
    same!(daemon_version, "daemon version", info.version);
    same!(trusted, "daemon's trust in diskgc's user", info.trusted);
    // Step 6, daemon half.
    if !ADMITTED_DAEMON_VERSIONS.contains(&info.version.as_str()) {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!(
                "Nix daemon {} with client {client_version} is not admitted (daemons: {}; \
                 clients: {})",
                info.version,
                ADMITTED_DAEMON_VERSIONS.join(", "),
                ADMITTED_CLIENT_VERSIONS.join(", ")
            ),
        ));
    }
    Ok(crate::backend::Binding::Bound(Pinned {
        bound: Bound {
            client_dir: client.dir.clone(),
            client_nix,
            client_version,
            daemon_version: info.version,
            trusted: info.trusted,
            store: store_id,
            socket: socket_id,
        },
        socket: socket.fd,
        client,
    }))
}

/// The detail's uid: a number, or `unknown` when `statx` did not say.
fn shown_id(uid: Option<u32>) -> String {
    uid.map_or_else(|| "unknown".to_owned(), |uid| uid.to_string())
}

fn dev_ino(stat: &Stat, path: &str) -> Result<DevIno, HostError> {
    stat.dev_ino()
        .ok_or_else(|| HostError::invalid("resolve", path, "statx reported no inode"))
}

/// Which part of the layout a path is: what its absence means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutRole {
    /// `/nix/store`: absent is `missing`.
    Store,
    /// A directory above the socket: absent means the socket is absent.
    SocketAncestor,
    /// The socket: absent is `backend_unavailable`.
    Socket,
}

/// `path` reached as in docs/safety.md, Path traversal (no symlink on the
/// way); a path that cannot be reached safely is a skip, a host error an
/// error.
fn resolve_layout_dir<H: Host>(
    host: &H,
    path: &Path,
    role: LayoutRole,
) -> Result<Result<Resolved<H::Fd>, Skipped>, HostError> {
    let want = if role == LayoutRole::Socket {
        Want::Any
    } else {
        Want::Directory
    };
    match resolve::resolve(host, path, want) {
        Ok(resolved) => Ok(Ok(resolved)),
        Err(ResolveError::Host(error)) => Err(error),
        Err(ResolveError::Missing { .. }) => Ok(Err(match role {
            LayoutRole::Store => Skipped {
                reason: Reason::Missing,
                detail: format!("{STORE_DIR} does not exist: Nix is not installed"),
            },
            LayoutRole::SocketAncestor | LayoutRole::Socket => Skipped {
                reason: Reason::BackendUnavailable,
                detail: format!(
                    "{SOCKET} does not exist: no Nix daemon is listening (multi-user store at \
                     {STORE_DIR})"
                ),
            },
        })),
        Err(error) => Ok(Err(Skipped {
            reason: Reason::UnsupportedBackend,
            detail: format!(
                "{} cannot be reached as the multi-user layout's: {error}",
                path.display()
            ),
        })),
    }
}

/// Step 5's variables: unset, empty or their default, otherwise the skip.
fn redirecting_variable(environment: &NixEnvironment) -> Option<Skipped> {
    [
        ("NIX_STATE_DIR", &environment.state_dir, STATE_DIR),
        ("NIX_STORE_DIR", &environment.store_dir, STORE_DIR),
        ("NIX_STORE", &environment.store, STORE_DIR),
        (
            "NIX_DAEMON_SOCKET_PATH",
            &environment.daemon_socket_path,
            SOCKET,
        ),
    ]
    .into_iter()
    .find_map(|(name, value, default)| {
        let value = value.as_ref()?;
        (!value.is_empty() && value.as_os_str() != default).then(|| Skipped {
            reason: Reason::UnsupportedBackend,
            detail: format!(
                "{name}={} redirects Nix to another store or daemon than {SOCKET}",
                value.to_string_lossy()
            ),
        })
    })
}

/// The version of a `nix-store --version` line that must be exactly
/// `nix-store (Nix) <version>` and one newline, the version admitted;
/// anything else (Lix, Determinate Nix, another version) is the skip.
pub fn client_version(line: &str) -> Result<String, Skipped> {
    let version = line
        .strip_suffix('\n')
        .and_then(|line| line.strip_prefix("nix-store (Nix) "))
        .filter(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        });
    match version {
        Some(version) if ADMITTED_CLIENT_VERSIONS.contains(&version) => Ok(version.to_owned()),
        Some(version) => Err(Skipped {
            reason: Reason::UnsupportedBackend,
            detail: format!(
                "Nix client {version} is not admitted (clients: {})",
                ADMITTED_CLIENT_VERSIONS.join(", ")
            ),
        }),
        None => Err(Skipped {
            reason: Reason::UnsupportedBackend,
            detail: format!(
                "the client's version line {:?} is not CppNix's `nix-store (Nix) <version>` \
                 (another variant, such as Lix or Determinate Nix, is not admitted)",
                line.trim_end()
            ),
        }),
    }
}

/// What `nix config show store` printed: `Ok(None)` when it selects the
/// system daemon, the skip otherwise; `Err` for output that is not one
/// value on one line.
pub fn store_selection(output: &str) -> Result<Option<Skipped>, String> {
    let value = output
        .strip_suffix('\n')
        .filter(|value| !value.is_empty() && !value.contains('\n'))
        .ok_or_else(|| format!("expected one value on one line, got {output:?}"))?;
    if DAEMON_STORES.contains(&value) {
        return Ok(None);
    }
    let scheme = value.split_once("://").map(|(scheme, _)| scheme);
    let reason = if scheme.is_some_and(|scheme| REMOTE_SCHEMES.contains(&scheme)) {
        Reason::RemoteBackend
    } else {
        Reason::UnsupportedBackend
    };
    Ok(Some(Skipped {
        reason,
        detail: format!(
            "the caller's Nix selects store {value:?}, not the system daemon at {SOCKET}: \
             diskgc does not collect behind it"
        ),
    }))
}

/// What diskgc reads of `nix store info --json` through the pinned socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    pub version: String,
    pub trusted: Option<bool>,
}

impl StoreInfo {
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| format!("store info is not JSON: {error}"))?;
        let version = value["version"]
            .as_str()
            .filter(|version| !version.is_empty())
            .ok_or("store info has no version (the daemon's)")?
            .to_owned();
        let trusted = match &value["trusted"] {
            serde_json::Value::Null => None,
            serde_json::Value::Bool(trusted) => Some(*trusted),
            other => return Err(format!("store info's trusted is {other}, not a boolean")),
        };
        Ok(Self { version, trusted })
    }
}

/// Step 1: the client of the installation root chose, one of
/// [`CLIENT_ANCHORS`] (the first that exists), its links followed to the
/// multi-call `nix` in a store path's `bin/`, checked and opened.
fn find_client<H: Host>(host: &H) -> Result<Result<Client<H::Fd>, Skipped>, HostError> {
    for anchor in CLIENT_ANCHORS {
        let start = Path::new(anchor);
        let followed = resolve::follow_links(host, start)?;
        let (end, stat, hops) = match followed {
            resolve::Followed::Missing => continue,
            resolve::Followed::TooManyLinks => {
                return Ok(Err(unsupported(format!(
                    "{anchor} is more than {} symlinks from a file",
                    resolve::MAX_LINK_HOPS
                ))));
            }
            resolve::Followed::End { path, stat, hops } => (path, *stat, hops),
        };
        if let Some(why) = untrusted_hop(&hops) {
            return Ok(Err(unsupported(format!(
                "{anchor} is not the installation's own client: {why}"
            ))));
        }
        return client_at(host, start, end, &stat);
    }
    Ok(Err(Skipped {
        reason: Reason::Missing,
        detail: format!(
            "no Nix client installed for the system ({}): neither leads to a file",
            CLIENT_ANCHORS.join(", ")
        ),
    }))
}

/// Why a link on the way from an anchor could have been placed by someone
/// other than root: the link is not root's, or it lies outside the store in
/// a directory that is not root's or that group or others can write. (Inside
/// a store path, content is immutable and the link is whatever root's chain
/// chose.)
fn untrusted_hop(hops: &[resolve::Hop]) -> Option<String> {
    hops.iter().find_map(|hop| {
        if hop.owner != Some(0) {
            return Some(format!(
                "the link {} is owned by uid {}, not root",
                hop.link.display(),
                shown_id(hop.owner)
            ));
        }
        let in_store = hop.dir.starts_with(STORE_DIR) && hop.dir != Path::new(STORE_DIR);
        let safe_dir =
            hop.dir_owner == Some(0) && hop.dir_perm.is_some_and(|perm| perm & 0o022 == 0);
        (!in_store && !safe_dir).then(|| {
            format!(
                "the link {} is in {}, owned by uid {} with mode {:o}: not a directory only \
                 root can write",
                hop.link.display(),
                hop.dir.display(),
                shown_id(hop.dir_owner),
                hop.dir_perm.unwrap_or_default()
            )
        })
    })
}

/// The bound client and the identity of its `nix`.
struct Client<F> {
    client: NixClient<F>,
    nix: DevIno,
}

/// The client whose `nix` the anchor `start` leads to, at `end`:
/// - `end` is `/nix/store/<hash>-nix-<admitted version>/bin/nix`;
/// - that store path and its `bin/` are root's and nobody can write them;
/// - its `nix-store` is a symlink whose text is exactly `nix` (CppNix's one
///   multi-call binary dispatching on `argv[0]`);
/// - `nix` is a regular executable of root's, and an ELF file (read on the
///   descriptor diskgc keeps and later executes): never a script, which
///   could pass the version query and then run something else.
fn client_at<H: Host>(
    host: &H,
    start: &Path,
    end: PathBuf,
    stat: &Stat,
) -> Result<Result<Client<H::Fd>, Skipped>, HostError> {
    let refuse = |why: String| {
        Ok(Err(unsupported(format!(
            "{} leads to {}, {why}: not an admitted CppNix multi-call client in a store path",
            start.display(),
            end.display()
        ))))
    };
    let dir = end.parent().unwrap_or(Path::new("/")).to_owned();
    let store_path = dir.parent().unwrap_or(Path::new("/")).to_owned();
    let in_store_bin = end.file_name() == Some(OsStr::new("nix"))
        && dir.file_name() == Some(OsStr::new("bin"))
        && is_store_path(&store_path);
    if !in_store_bin {
        return refuse("which is not /nix/store/<name>/bin/nix".to_owned());
    }
    let named = store_path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            ADMITTED_CLIENT_VERSIONS
                .iter()
                .any(|version| name.ends_with(&format!("-nix-{version}")))
        });
    if !named {
        return refuse(format!(
            "whose store path is not named `<hash>-nix-<version>` for an admitted version ({})",
            ADMITTED_CLIENT_VERSIONS.join(", ")
        ));
    }
    let store_path_fd = host.openat2(Base::Root, &store_path, store_dir_how())?;
    let dir_fd = host.openat2(Base::Root, &dir, store_dir_how())?;
    for (what, at) in [
        (
            store_path.as_path(),
            host.statx(Base::Dir(&store_path_fd), Path::new(""))?,
        ),
        (
            dir.as_path(),
            host.statx(Base::Dir(&dir_fd), Path::new(""))?,
        ),
    ] {
        if at.uid != Some(0) || at.perm.is_none_or(|perm| perm & 0o222 != 0) {
            return refuse(format!(
                "and {} is owned by uid {} with mode {:o}, not root's and read-only",
                what.display(),
                shown_id(at.uid),
                at.perm.unwrap_or_default()
            ));
        }
    }
    let multicall = match host.statx(Base::Dir(&dir_fd), Path::new("nix-store")) {
        Ok(link) if link.file_type == Some(FileType::Symlink) => {
            host.read_link(&dir_fd, OsStr::new("nix-store"))? == Path::new("nix")
        }
        Ok(_) => false,
        Err(error) if error.is(Errno::ENOENT) => false,
        Err(error) => return Err(error),
    };
    if !multicall {
        return refuse("whose directory's nix-store is not a symlink to `nix`".to_owned());
    }
    let nix = host.openat2(
        Base::Dir(&dir_fd),
        Path::new("nix"),
        resolve::beneath_how(Access::Read, Want::Any),
    )?;
    let opened = host.statx(Base::Dir(&nix), Path::new(""))?;
    if opened.dev_ino() != stat.dev_ino() {
        return Ok(Err(Skipped {
            reason: Reason::Changed,
            detail: format!("{} was replaced while it was checked", end.display()),
        }));
    }
    if opened.file_type != Some(FileType::Regular)
        || opened.uid != Some(0)
        || opened.perm.is_none_or(|perm| perm & 0o111 == 0)
    {
        return refuse(format!(
            "a {} of uid {} with mode {:o}, not a regular executable of root's",
            opened
                .file_type
                .map_or_else(|| "file of unknown type".to_owned(), |t| t.to_string()),
            shown_id(opened.uid),
            opened.perm.unwrap_or_default()
        ));
    }
    if host.read_head(&nix, ELF_MAGIC.len())? != ELF_MAGIC {
        return refuse("which is not an ELF executable (a script or wrapper)".to_owned());
    }
    let id = dev_ino(&opened, "nix")?;
    Ok(Ok(Client {
        client: NixClient { dir, nix },
        nix: id,
    }))
}

/// `openat2` arguments for the client's `bin/` directory: reached with no
/// symlink left (it is the resolved path).
fn store_dir_how() -> OpenHow {
    OpenHow {
        access: Access::Path,
        directory: true,
        nofollow: true,
        resolve: ResolveFlags {
            beneath: false,
            no_symlinks: true,
            no_xdev: false,
        },
    }
}

/// The first bytes of every ELF file.
pub const ELF_MAGIC: &[u8] = b"\x7fELF";

/// One generation link: `<profile>-<N>-link` below the profiles directory,
/// whose text is an existing store path directly in `/nix/store`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation {
    pub link: PathBuf,
    pub target: PathBuf,
}

/// Whether `name` is shaped `<profile>-<N>-link` (a profile generation).
pub fn is_generation_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(rest) = name.strip_suffix("-link") else {
        return false;
    };
    let Some((profile, number)) = rest.rsplit_once('-') else {
        return false;
    };
    !profile.is_empty() && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

/// Step 8's listing: every generation link in [`PROFILES`] and each
/// `per-user/*` directory (one level each, at most
/// [`MAX_PROFILE_ENTRIES`] names per directory, no symlink followed), in
/// order. `Err(Skipped)`: none to check, or more than a bound.
pub fn generations<H: Host>(host: &H) -> Result<Result<Vec<Generation>, Skipped>, HostError> {
    let store = match resolve::resolve(host, Path::new(STORE_DIR), Want::Directory) {
        Ok(store) => store,
        Err(ResolveError::Host(error)) => return Err(error),
        Err(error) => {
            return Ok(Err(unsupported(format!(
                "{STORE_DIR} cannot be reached for the root check: {error}"
            ))));
        }
    };
    let profiles = match resolve::resolve(host, Path::new(PROFILES), Want::Directory) {
        Ok(profiles) => profiles,
        Err(ResolveError::Host(error)) => return Err(error),
        Err(error) => {
            return Ok(Err(unsupported(format!(
                "no generation to check the daemon's roots against: {error}"
            ))));
        }
    };
    let mut found = Vec::new();
    let names = match list(host, &profiles.fd, Path::new(PROFILES))? {
        Ok(names) => names,
        Err(skipped) => return Ok(Err(skipped)),
    };
    for name in names {
        if found.len() > MAX_GENERATIONS {
            break;
        }
        if name == PER_USER {
            let per_user = Path::new(PROFILES).join(PER_USER);
            let stat = host.statx(Base::Dir(&profiles.fd), Path::new(PER_USER))?;
            if stat.file_type != Some(FileType::Directory) {
                continue;
            }
            let (dir, _) = match resolve::open_beneath(
                host,
                &profiles.fd,
                profiles.mount.id,
                Path::new(PER_USER),
                Access::Path,
                Want::Directory,
            ) {
                Ok(opened) => opened,
                Err(ResolveError::Host(error)) => return Err(error),
                Err(error) => {
                    return Ok(Err(unsupported(format!(
                        "{} cannot be listed safely: {error}",
                        per_user.display()
                    ))));
                }
            };
            let users = match list(host, &dir, &per_user)? {
                Ok(users) => users,
                Err(skipped) => return Ok(Err(skipped)),
            };
            for user in users {
                if found.len() > MAX_GENERATIONS {
                    break;
                }
                let stat = host.statx(Base::Dir(&dir), Path::new(&user))?;
                if stat.file_type != Some(FileType::Directory) {
                    continue;
                }
                let user_path = per_user.join(&user);
                let (user_dir, _) = match resolve::open_beneath(
                    host,
                    &dir,
                    profiles.mount.id,
                    Path::new(&user),
                    Access::Path,
                    Want::Directory,
                ) {
                    Ok(opened) => opened,
                    Err(ResolveError::Host(error)) => return Err(error),
                    Err(error) => {
                        return Ok(Err(unsupported(format!(
                            "{} cannot be listed safely: {error}",
                            user_path.display()
                        ))));
                    }
                };
                let links = match list(host, &user_dir, &user_path)? {
                    Ok(links) => links,
                    Err(skipped) => return Ok(Err(skipped)),
                };
                for link in links {
                    if let Some(generation) =
                        generation(host, &user_dir, &user_path, &link, &store.fd)?
                    {
                        found.push(generation);
                    }
                    if found.len() > MAX_GENERATIONS {
                        break;
                    }
                }
            }
            continue;
        }
        if let Some(generation) =
            generation(host, &profiles.fd, Path::new(PROFILES), &name, &store.fd)?
        {
            found.push(generation);
        }
        if found.len() > MAX_GENERATIONS {
            break;
        }
    }
    if found.is_empty() {
        return Ok(Err(unsupported(format!(
            "no generation link (`<profile>-<N>-link` into {STORE_DIR}) below {PROFILES} to \
             check the daemon's roots against"
        ))));
    }
    if found.len() > MAX_GENERATIONS {
        return Ok(Err(unsupported(format!(
            "more than {MAX_GENERATIONS} generation links below {PROFILES}: the root check \
             does not sample; delete old generations (diskgc never does)"
        ))));
    }
    found.sort();
    Ok(Ok(found))
}

fn unsupported(detail: String) -> Skipped {
    Skipped {
        reason: Reason::UnsupportedBackend,
        detail,
    }
}

/// The names of the directory `dir` (at `path`), at most
/// [`MAX_PROFILE_ENTRIES`].
fn list<H: Host>(
    host: &H,
    dir: &H::Fd,
    path: &Path,
) -> Result<Result<Vec<OsString>, Skipped>, HostError> {
    match host.read_dir(dir, MAX_PROFILE_ENTRIES)? {
        DirListing::Names(mut names) => {
            names.sort();
            Ok(Ok(names))
        }
        DirListing::TooMany => Ok(Err(unsupported(format!(
            "{} holds more than {MAX_PROFILE_ENTRIES} entries: the root check does not sample; \
             delete old generations (diskgc never does)",
            path.display()
        )))),
    }
}

/// `name` in `dir` (at `path`) as a generation, when it is one: a symlink
/// shaped `<profile>-<N>-link` whose text is a store path directly in
/// `/nix/store` that exists (looked up in the open store directory, not
/// followed).
fn generation<H: Host>(
    host: &H,
    dir: &H::Fd,
    path: &Path,
    name: &OsStr,
    store: &H::Fd,
) -> Result<Option<Generation>, HostError> {
    if !is_generation_name(name) {
        return Ok(None);
    }
    let stat = host.statx(Base::Dir(dir), Path::new(name))?;
    if stat.file_type != Some(FileType::Symlink) {
        return Ok(None);
    }
    let target = host.read_link(dir, name)?;
    if !is_store_path(&target) {
        return Ok(None);
    }
    let store_name = target.file_name().expect("a store path has a name");
    match host.statx(Base::Dir(store), Path::new(store_name)) {
        Ok(_) => Ok(Some(Generation {
            link: path.join(name),
            target,
        })),
        Err(error) if error.is(Errno::ENOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The distinct targets of `generations`, for `--query --roots`.
pub fn targets(generations: &[Generation]) -> Vec<PathBuf> {
    generations
        .iter()
        .map(|generation| generation.target.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Step 8's verdict on `nix-store --query --roots`'s output: every
/// generation must appear exactly as the line `<link> -> <target>`.
pub fn check_roots(generations: &[Generation], output: &str) -> Result<(), Skipped> {
    let lines: BTreeSet<&str> = output.lines().collect();
    match generations.iter().find(|generation| {
        let line = format!(
            "{} -> {}",
            generation.link.display(),
            generation.target.display()
        );
        !lines.contains(line.as_str())
    }) {
        None => Ok(()),
        Some(missing) => Err(unsupported(format!(
            "the bound Nix daemon does not count generation {} -> {} as a root: its root \
             database is not the system's ({STATE_DIR})",
            missing.link.display(),
            missing.target.display()
        ))),
    }
}

/// Step 9's bound: what is still needed to reach `goal` with `avail`
/// available, at least 1 (Nix deletes nothing with `--max-freed 0`).
pub fn need(goal: u64, avail: u64) -> u64 {
    goal.saturating_sub(avail).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_set_is_the_closed_one() {
        let store = pinned_store(3);
        assert_eq!(store, "unix:///proc/self/fd/3");
        let shown = |(program, args): (&str, Vec<OsString>)| {
            std::iter::once(program.to_owned())
                .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            shown(query_argv(NixQuery::ClientVersion, &store)),
            "nix-store --option plugin-files  --version"
        );
        assert_eq!(
            shown(query_argv(NixQuery::ConfigShowStore, &store)),
            "nix --option plugin-files  --extra-experimental-features nix-command config show \
             store"
        );
        assert_eq!(
            shown(query_argv(NixQuery::StoreInfo, &store)),
            "nix --option plugin-files  --extra-experimental-features nix-command store info \
             --json --store unix:///proc/self/fd/3"
        );
        let target = PathBuf::from("/nix/store/aaaa-system");
        assert_eq!(
            roots_argv(&store, std::slice::from_ref(&target)).unwrap(),
            [
                "--option",
                "plugin-files",
                "",
                "--store",
                "unix:///proc/self/fd/3",
                "--query",
                "--roots",
                "/nix/store/aaaa-system"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            gc_argv(&store, 1048576).unwrap(),
            [
                "--option",
                "plugin-files",
                "",
                "--store",
                "unix:///proc/self/fd/3",
                "--gc",
                "--max-freed",
                "1048576"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn the_runs_refuse_what_would_change_their_meaning() {
        assert!(gc_argv("s", 0).is_err(), "--max-freed 0 deletes nothing");
        assert!(roots_argv("s", &[]).is_err());
        for bad in [
            "/nix/store",
            "/nix/store/a/b",
            "/nix/store/.links",
            "/nix/store/../etc",
            "/nix/var/nix/profiles/system",
            "--delete",
            "nix/store/a",
        ] {
            assert!(
                roots_argv("s", &[PathBuf::from(bad)]).is_err(),
                "{bad} is no target"
            );
        }
    }

    #[test]
    fn the_child_environment_is_home_and_no_user_configuration() {
        assert_eq!(
            child_environment(Some(OsStr::new("/home/u"))),
            vec![
                (OsString::from("HOME"), OsString::from("/home/u")),
                (OsString::from("NIX_USER_CONF_FILES"), OsString::new()),
            ]
        );
        assert_eq!(
            child_environment(None),
            vec![(OsString::from("NIX_USER_CONF_FILES"), OsString::new())]
        );
    }

    #[test]
    fn only_the_cppnix_version_line_of_an_admitted_client_binds() {
        assert_eq!(
            client_version("nix-store (Nix) 2.34.8\n"),
            Ok("2.34.8".to_owned())
        );
        for line in [
            "nix-store (Nix) 2.34.9\n",
            "nix-store (Nix) 2.34.8",
            "nix-store (Nix) 2.34.8\n\n",
            "nix-store (Lix, like Nix) 2.95.4\n",
            "nix-store (Determinate Nix 3.11.2) 2.31.2\n",
            "nix (Nix) 2.34.8\n",
            "",
        ] {
            let skipped = client_version(line).expect_err(line);
            assert_eq!(skipped.reason, Reason::UnsupportedBackend, "{line:?}");
        }
    }

    #[test]
    fn the_store_selection_binds_only_the_system_daemon() {
        for value in DAEMON_STORES {
            assert_eq!(store_selection(&format!("{value}\n")), Ok(None), "{value}");
        }
        for (value, reason) in [
            ("ssh-ng://builder", Reason::RemoteBackend),
            ("ssh://builder", Reason::RemoteBackend),
            ("https://cache.example", Reason::RemoteBackend),
            ("s3://bucket", Reason::RemoteBackend),
            ("gs://bucket", Reason::RemoteBackend),
            ("http://cache", Reason::RemoteBackend),
            ("local", Reason::UnsupportedBackend),
            ("local://?state=/tmp/b", Reason::UnsupportedBackend),
            (
                "local://?root=/home/u/.local/share/nix/root",
                Reason::UnsupportedBackend,
            ),
            ("unix:///tmp/other.sock", Reason::UnsupportedBackend),
            ("file:///tmp/cache", Reason::UnsupportedBackend),
            ("dummy://", Reason::UnsupportedBackend),
            ("/home/u/chroot", Reason::UnsupportedBackend),
        ] {
            let skipped = store_selection(&format!("{value}\n"))
                .expect("one line")
                .expect(value);
            assert_eq!(skipped.reason, reason, "{value}");
        }
        for bad in ["", "auto", "auto\nlocal\n", "\n"] {
            assert!(store_selection(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn store_info_needs_the_daemon_version() {
        assert_eq!(
            StoreInfo::parse(
                "{\"trusted\":false,\"url\":\"unix:///proc/self/fd/3\",\"version\":\"2.34.8\"}"
            ),
            Ok(StoreInfo {
                version: "2.34.8".into(),
                trusted: Some(false)
            })
        );
        assert_eq!(
            StoreInfo::parse("{\"url\":\"daemon\",\"version\":\"2.34.8\"}").map(|i| i.trusted),
            Ok(None)
        );
        assert!(StoreInfo::parse("{\"url\":\"daemon\"}").is_err());
        assert!(StoreInfo::parse("{\"version\":\"2.34.8\",\"trusted\":\"yes\"}").is_err());
        assert!(StoreInfo::parse("not json").is_err());
    }

    #[test]
    fn generation_names() {
        for name in [
            "system-1-link",
            "system-177-link",
            "profile-2-link",
            "a-b-3-link",
        ] {
            assert!(is_generation_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "system",
            "default",
            "per-user",
            "-1-link",
            "system--link",
            "system-1a-link",
            "system-1-lnk",
        ] {
            assert!(!is_generation_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn the_root_check_needs_each_generation_line_exactly() {
        let generation = |link: &str, target: &str| Generation {
            link: link.into(),
            target: target.into(),
        };
        let gens = [
            generation("/nix/var/nix/profiles/system-1-link", "/nix/store/a-sys"),
            generation("/nix/var/nix/profiles/system-2-link", "/nix/store/b-sys"),
        ];
        let all = "/nix/var/nix/profiles/system-1-link -> /nix/store/a-sys\n\
                   /proc/12/fd/3 -> /nix/store/a-sys\n\
                   /nix/var/nix/profiles/system-2-link -> /nix/store/b-sys\n";
        assert_eq!(check_roots(&gens, all), Ok(()));
        for output in [
            // Another state directory reports other paths.
            "/nix/var/nix-b/profiles/system-1-link -> /nix/store/a-sys\n\
             /nix/var/nix-b/profiles/system-2-link -> /nix/store/b-sys\n",
            // An older generation not counted.
            "/nix/var/nix/profiles/system-2-link -> /nix/store/b-sys\n",
            // Not exactly the line.
            "/nix/var/nix/profiles/system-1-link -> /nix/store/a-sys \n\
             /nix/var/nix/profiles/system-2-link -> /nix/store/b-sys\n",
            "",
        ] {
            assert_eq!(
                check_roots(&gens, output).map_err(|s| s.reason),
                Err(Reason::UnsupportedBackend),
                "{output:?}"
            );
        }
        assert_eq!(
            targets(&[gens[1].clone(), gens[0].clone(), gens[0].clone()]),
            vec![
                PathBuf::from("/nix/store/a-sys"),
                PathBuf::from("/nix/store/b-sys")
            ]
        );
    }

    #[test]
    fn the_need_is_at_least_one_byte() {
        assert_eq!(need(100, 40), 60);
        assert_eq!(need(100, 100), 1);
        assert_eq!(need(100, 200), 1);
    }
}
