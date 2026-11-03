//! Read-only binding of the caller's local Podman store (kind
//! `podman-images`, docs/safety.md, Podman images). Podman has no daemon:
//! the "backend" is the store `podman --remote=false` opens for diskgc's
//! user, as one `podman info` reports it. diskgc never touches the store's
//! files; Podman prunes its own images, by its own rules, and the binding
//! only decides whether that prune is the one asked for (a local, rootless
//! store of an admitted Podman whose graph root is on the requested
//! filesystem).
//!
//! The binding also holds the storage configuration Podman reads (the
//! `store.configFile` it reports and the system `storage.conf` files, their
//! contents) and `store.graphOptions`, compared again before the prune; a
//! separate image store (`imagestore`, `additionalimagestores`), which
//! `podman info` does not report, is refused when configured.
//!
//! Limit (documented in docs/safety.md): the prune child reads the
//! configuration and opens the store by path again, so a change between the
//! recheck and the child's start is not caught.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::backend::{Skipped, skip};
use crate::engine::Reason;
use crate::host::{DevIno, Errno, Host, HostError, PodmanQuery};
use crate::resolve::{self, ResolveError, Want};

/// Podman versions whose `info` output and `image prune` behaviour were
/// checked (docs/reference.md, Backend version allowlist). Another version
/// skips the tier `unsupported_backend`.
pub const ADMITTED_VERSIONS: &[&str] = &["5.8.7"];

/// The system `storage.conf` files containers/storage 5.8.7 reads when no
/// per-user one exists (`/etc` overrides `/usr/share`), bound with the
/// reported `store.configFile`.
pub const SYSTEM_CONFIG_FILES: [&str; 2] = [
    "/etc/containers/storage.conf",
    "/usr/share/containers/storage.conf",
];

/// A `storage.conf` larger than this is not bound (`unsupported_backend`):
/// the shipped files are a few KiB.
pub const CONFIG_MAX_BYTES: usize = 64 * 1024;

/// What diskgc binds of a local store: compared in full again right before
/// the prune, so a changed configuration, store or client is `changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanInfo {
    pub version: String,
    /// `store.graphRoot`: where image layers live, the measured candidate.
    pub graph_root: PathBuf,
    /// `store.runRoot`: where Podman keeps the store's locks.
    pub run_root: PathBuf,
    /// `store.graphDriverName`.
    pub driver: String,
    /// `store.configFile`: the storage configuration Podman reports using
    /// (it need not exist; then the system files apply).
    pub config_file: PathBuf,
    /// `store.graphOptions`, as compact JSON.
    pub graph_options: String,
}

impl PodmanInfo {
    /// Parse one `podman --remote=false info --format json` response.
    /// `Ok(Err(_))`: a store diskgc does not prune (another version, a
    /// remote service, a rootful store), identified from a specific field.
    /// `Err`: output an admitted version must not produce (a run error).
    pub fn parse(json: &str) -> Result<Result<Self, Skipped>, String> {
        let info: Value = serde_json::from_str(json)
            .map_err(|error| format!("Podman info is not JSON: {error}"))?;
        let version = info["version"]["Version"]
            .as_str()
            .filter(|version| !version.is_empty())
            .ok_or("Podman info has no version.Version")?;
        if !ADMITTED_VERSIONS.contains(&version) {
            return Ok(Err(Skipped {
                reason: Reason::UnsupportedBackend,
                detail: format!(
                    "Podman {version} is not admitted (admitted: {})",
                    ADMITTED_VERSIONS.join(", ")
                ),
            }));
        }
        let remote = info["host"]["serviceIsRemote"]
            .as_bool()
            .ok_or("Podman info has no boolean host.serviceIsRemote")?;
        if remote {
            return Ok(Err(Skipped {
                reason: Reason::RemoteBackend,
                detail: "podman --remote=false still reports a remote service \
                         (podman-remote installed as podman?)"
                    .to_owned(),
            }));
        }
        let rootless = info["host"]["security"]["rootless"]
            .as_bool()
            .ok_or("Podman info has no boolean host.security.rootless")?;
        if !rootless {
            return Ok(Err(Skipped {
                reason: Reason::UnsupportedBackend,
                detail: "the Podman store is rootful; only the caller's rootless store is pruned"
                    .to_owned(),
            }));
        }
        let store = &info["store"];
        let path = |key: &str| -> Result<PathBuf, String> {
            let text = store[key]
                .as_str()
                .ok_or_else(|| format!("Podman info has no store.{key}"))?;
            absolute_clean_path(text, key)
        };
        let driver = store["graphDriverName"]
            .as_str()
            .filter(|driver| !driver.is_empty())
            .ok_or("Podman info has no store.graphDriverName")?;
        let graph_options = store["graphOptions"]
            .as_object()
            .ok_or("Podman info has no store.graphOptions object")?;
        if let Some(key) = graph_options
            .keys()
            .find(|key| key.to_ascii_lowercase().contains("imagestore"))
        {
            return Ok(Err(Skipped {
                reason: Reason::UnsupportedBackend,
                detail: format!(
                    "Podman graph option {key:?} names a separate image store, \
                     whose space and filesystem diskgc cannot bind"
                ),
            }));
        }
        Ok(Ok(Self {
            version: version.to_owned(),
            graph_root: path("graphRoot")?,
            run_root: path("runRoot")?,
            driver: driver.to_owned(),
            config_file: path("configFile")?,
            graph_options: Value::Object(graph_options.clone()).to_string(),
        }))
    }
}

/// A local store checked against the requested filesystem. Re-run
/// [`inspect_backend`] and compare before the prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub info: PodmanInfo,
    /// The graph root directory resolved by its path: a store moved away
    /// and replaced at that path since the binding is `changed`.
    pub graph_root: DevIno,
    /// Each storage configuration file Podman may read and its contents
    /// (`None`: absent): an edited or newly created one is `changed`.
    pub config: Vec<(PathBuf, Option<Vec<u8>>)>,
}

pub type Binding = crate::backend::Binding<Bound>;

/// Bind the caller's local Podman store. One query, through the Host port,
/// so ModelHost reproduces every outcome without a real Podman. `target`
/// is the requested filesystem's space: the graph root must be on it, or
/// the kind is skipped `other_fs`.
pub fn inspect_backend<H: Host>(
    host: &H,
    target: crate::places::Space,
) -> Result<Binding, HostError> {
    let text = match host.podman_query(PodmanQuery::Info) {
        Ok(text) => text,
        Err(error) if error.is(Errno::ENOENT) => {
            return Ok(skip(Reason::Missing, "Podman client is not installed"));
        }
        Err(error) => return Err(error),
    };
    let info = match PodmanInfo::parse(&text) {
        Ok(Ok(info)) => info,
        Ok(Err(skipped)) => return Ok(Binding::Skipped(skipped)),
        Err(error) => return Err(HostError::invalid("query Podman", "info", error)),
    };
    let mut config = Vec::new();
    for path in std::iter::once(info.config_file.clone())
        .chain(SYSTEM_CONFIG_FILES.iter().map(PathBuf::from))
    {
        if config.iter().any(|(seen, _)| *seen == path) {
            continue;
        }
        let content = host.read_backend_config(&path, CONFIG_MAX_BYTES + 1)?;
        if let Some(bytes) = &content {
            if bytes.len() > CONFIG_MAX_BYTES {
                return Ok(skip(
                    Reason::UnsupportedBackend,
                    format!(
                        "Podman storage configuration {} is over {CONFIG_MAX_BYTES} bytes",
                        path.display()
                    ),
                ));
            }
            if configures_separate_image_store(bytes) {
                return Ok(skip(
                    Reason::UnsupportedBackend,
                    format!(
                        "Podman storage configuration {} sets imagestore or \
                         additionalimagestores: a separate image store's space and filesystem \
                         are not in podman info",
                        path.display()
                    ),
                ));
            }
        }
        config.push((path, content));
    }
    let root = info.graph_root.clone();
    let graph_root = match resolve::resolve(host, &root, Want::Directory) {
        Ok(resolved) => {
            let space = crate::places::space_at(host, &resolved.mount, &resolved.fd);
            if !space.same_as(&target) {
                return Ok(skip(
                    Reason::OtherFs,
                    format!(
                        "Podman graph root {} is on {space}, requested filesystem is {target}",
                        root.display()
                    ),
                ));
            }
            resolved.stat.dev_ino().ok_or_else(|| {
                HostError::invalid("resolve", root.display(), "statx reported no inode")
            })?
        }
        Err(ResolveError::Host(error)) => return Err(error),
        Err(error) => {
            return Ok(skip(
                error
                    .skip_reason()
                    .expect("a host error returned above; every other one is a skip"),
                format!(
                    "Podman graph root {} cannot be safely resolved: {error}",
                    root.display()
                ),
            ));
        }
    };
    Ok(Binding::Bound(Bound {
        info,
        graph_root,
        config,
    }))
}

/// The prune's arguments after the program: always the local store
/// (`--remote=false`), never a container or volume (`image prune` removes
/// neither, and `--external` is never passed, so images of Buildah's
/// working containers are kept too).
pub fn prune_arguments(all: bool) -> Vec<&'static str> {
    let mut arguments = vec!["--remote=false", "image", "prune", "--force"];
    if all {
        arguments.push("--all");
    }
    arguments
}

/// Whether a `storage.conf` sets a separate image store: an uncommented
/// `imagestore` with a non-empty value, or `additionalimagestores` with a
/// non-empty list, in any table. Deliberately coarse (a line scan, not a
/// TOML parse): a false positive only skips the tier.
pub fn configures_separate_image_store(text: &[u8]) -> bool {
    String::from_utf8_lossy(text).lines().any(|line| {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let value: String = value.chars().filter(|c| !c.is_whitespace()).collect();
        match key.trim().trim_matches('"').trim_matches('\'') {
            "imagestore" => !matches!(value.as_str(), "\"\"" | "''"),
            "additionalimagestores" => value != "[]",
            _ => false,
        }
    })
}

fn absolute_clean_path(text: &str, label: &str) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "Podman store.{label} must be an absolute normalized path, got {text:?}"
        ));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str, remote: bool, rootless: bool, root: &str) -> String {
        serde_json::json!({
            "host": {"serviceIsRemote": remote, "security": {"rootless": rootless}},
            "store": {"graphRoot": root, "runRoot": "/run/user/1000/containers", "graphDriverName": "overlay",
                      "configFile": "/home/u/.config/containers/storage.conf", "graphOptions": {}},
            "version": {"Version": version},
        })
        .to_string()
    }

    #[test]
    fn an_admitted_local_rootless_store_parses_with_its_identity() {
        let parsed = PodmanInfo::parse(&info(
            "5.8.7",
            false,
            true,
            "/home/u/.local/share/containers/storage",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed,
            PodmanInfo {
                version: "5.8.7".to_owned(),
                graph_root: "/home/u/.local/share/containers/storage".into(),
                run_root: "/run/user/1000/containers".into(),
                driver: "overlay".to_owned(),
                config_file: "/home/u/.config/containers/storage.conf".into(),
                graph_options: "{}".to_owned(),
            }
        );
    }

    #[test]
    fn another_version_remote_or_rootful_store_is_a_skip_with_its_reason() {
        let root = "/home/u/.local/share/containers/storage";
        let reason = |json: String| PodmanInfo::parse(&json).unwrap().unwrap_err().reason;
        assert_eq!(
            reason(info("5.4.0", false, true, root)),
            Reason::UnsupportedBackend
        );
        assert_eq!(
            reason(info("5.8.7", true, true, root)),
            Reason::RemoteBackend
        );
        assert_eq!(
            reason(info("5.8.7", false, false, root)),
            Reason::UnsupportedBackend
        );
        // An unadmitted version is skipped whatever shape the rest has.
        assert_eq!(
            reason(serde_json::json!({"version": {"Version": "9.0.0"}}).to_string()),
            Reason::UnsupportedBackend
        );
    }

    #[test]
    fn output_an_admitted_version_must_not_produce_is_an_error() {
        assert!(PodmanInfo::parse("not json").is_err());
        assert!(PodmanInfo::parse(&serde_json::json!({"host": {}}).to_string()).is_err());
        assert!(PodmanInfo::parse(&info("5.8.7", false, true, "relative/storage")).is_err());
        assert!(PodmanInfo::parse(&info("5.8.7", false, true, "/home/u/../storage")).is_err());
        let without = |pointer: &str, key: &str| {
            let mut json: Value = serde_json::from_str(&info("5.8.7", false, true, "/s")).unwrap();
            json.pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .unwrap()
                .remove(key);
            json.to_string()
        };
        for (pointer, key) in [
            ("/host", "serviceIsRemote"),
            ("/host/security", "rootless"),
            ("/store", "runRoot"),
            ("/store", "graphDriverName"),
            ("/store", "configFile"),
            ("/store", "graphOptions"),
        ] {
            let json = without(pointer, key);
            assert!(PodmanInfo::parse(&json).is_err(), "{json}");
        }
        let no_driver = info("5.8.7", false, true, "/s").replace("\"overlay\"", "\"\"");
        assert!(PodmanInfo::parse(&no_driver).is_err());
    }

    #[test]
    fn a_separate_image_store_is_refused_from_graph_options_or_storage_conf() {
        let mut json: Value = serde_json::from_str(&info("5.8.7", false, true, "/s")).unwrap();
        json["store"]["graphOptions"] = serde_json::json!({"overlay.imagestore": "/other"});
        let skipped = PodmanInfo::parse(&json.to_string()).unwrap().unwrap_err();
        assert_eq!(skipped.reason, Reason::UnsupportedBackend);

        for set in [
            "[storage]\nimagestore = \"/mnt/images\"\n",
            "[storage.options]\nadditionalimagestores = [ \"/ro/store\" ]\n",
            "[storage.options]\nadditionalimagestores = [\n  \"/ro/store\",\n]\n",
            "  imagestore='/x' # moved\n",
        ] {
            assert!(configures_separate_image_store(set.as_bytes()), "{set}");
        }
        for unset in [
            "[storage]\ndriver = \"overlay\"\n# imagestore = \"/mnt/images\"\n",
            "[storage]\nimagestore = \"\"\n",
            "[storage.options]\nadditionalimagestores = []\n",
        ] {
            assert!(
                !configures_separate_image_store(unset.as_bytes()),
                "{unset}"
            );
        }
    }

    #[test]
    fn the_prune_pins_the_local_store_and_adds_all_only_for_unreferenced() {
        assert_eq!(
            prune_arguments(false),
            ["--remote=false", "image", "prune", "--force"]
        );
        assert_eq!(
            prune_arguments(true),
            ["--remote=false", "image", "prune", "--force", "--all"]
        );
    }
}
