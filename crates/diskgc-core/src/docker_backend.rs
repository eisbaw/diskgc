//! Read-only Docker backend binding. The root is derived from daemon facts,
//! never from the client environment or a guessed default. A system
//! containerd socket is deliberately unsupported until its independent root
//! can be established from that daemon's configuration.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::engine::Reason;
use crate::host::{DockerEnvironment, DockerQuery, Errno, Host, HostError};
use crate::resolve::{self, ResolveError, Want};

/// The image store that a bound Docker daemon uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageStore {
    /// Classic graphdriver data is below DockerRootDir.
    Classic(PathBuf),
    /// A Docker-managed containerd stores image data at this distinct root.
    BundledContainerd(PathBuf),
}

impl ImageStore {
    pub fn root(&self) -> &Path {
        match self {
            Self::Classic(root) | Self::BundledContainerd(root) => root,
        }
    }
}

/// A Docker `/info` response parsed from one query to the bound daemon.
/// Parsing is strict because a wrong root can make filesystem matching lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerInfo {
    /// Daemon identity, compared again immediately before deletion.
    pub id: String,
    pub docker_root: PathBuf,
    pub image_store: ImageStore,
}

impl DockerInfo {
    pub fn parse(json: &str) -> Result<Self, String> {
        let info: Value = serde_json::from_str(json)
            .map_err(|error| format!("Docker info is not JSON: {error}"))?;
        let id = info["ID"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Docker info has no daemon ID")?
            .to_owned();
        let root = info["DockerRootDir"]
            .as_str()
            .ok_or("Docker info has no DockerRootDir")?;
        let root = absolute_clean_path(root, "DockerRootDir")?;

        let driver = info["Driver"].as_str().ok_or("Docker info has no Driver")?;
        let snapshotter = match info.get("DriverStatus") {
            Some(Value::Null) if driver == "vfs" => false,
            Some(Value::Null) => {
                return Err(format!(
                    "unsupported_backend: Docker driver {driver:?} has no store-type status"
                ));
            }
            Some(Value::Array(entries)) => {
                let mut store_type = None;
                for entry in entries {
                    let pair = entry
                        .as_array()
                        .filter(|pair| pair.len() == 2)
                        .ok_or("Docker DriverStatus has a malformed entry")?;
                    let key = pair[0]
                        .as_str()
                        .ok_or("Docker DriverStatus key is not a string")?;
                    let value = pair[1]
                        .as_str()
                        .ok_or("Docker DriverStatus value is not a string")?;
                    if key == "driver-type" && store_type.replace(value).is_some() {
                        return Err("Docker DriverStatus has duplicate driver-type rows".to_owned());
                    }
                }
                match store_type {
                    Some("io.containerd.snapshotter.v1") => true,
                    _ => {
                        return Err("unsupported_backend: Docker DriverStatus has no verified image-store type".to_owned());
                    }
                }
            }
            Some(_) => return Err("Docker DriverStatus is not a list or null".to_owned()),
            None => return Err("Docker info has no DriverStatus".to_owned()),
        };
        let image_store = if snapshotter {
            let address = info["Containerd"]["Address"].as_str().ok_or(
                "Docker uses a containerd image store but did not report Containerd.Address",
            )?;
            if address != "/run/docker/containerd/containerd.sock" {
                return Err(format!(
                    "unsupported_backend: containerd address {address:?} is not Docker's bundled socket; its independent storage root is unknown"
                ));
            }
            ImageStore::BundledContainerd(root.join("containerd/daemon"))
        } else {
            // Only vfs with null DriverStatus has been verified as classic.
            ImageStore::Classic(root.clone())
        };
        Ok(Self {
            id,
            docker_root: root,
            image_store,
        })
    }
}

/// A backend whose endpoint and storage roots were checked against
/// the requested filesystem. Re-run `bind` and compare before a prune: a
/// context or daemon can change between planning and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub info: DockerInfo,
    pub endpoint: String,
}

pub use crate::backend::Skipped;

/// A Docker binding: bound, or a skip of the backend.
pub type Binding = crate::backend::Binding<Bound>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Images,
    BuildCache,
}

use crate::backend::skip;

fn query<H: Host>(host: &H, what: DockerQuery) -> Result<Result<String, Binding>, HostError> {
    match host.docker_query(what) {
        Ok(output) => Ok(Ok(output)),
        Err(error) if error.is(Errno::ENOENT) => {
            Ok(Err(skip(Reason::Missing, "Docker client is not installed")))
        }
        Err(error) => Err(error),
    }
}

/// Bind the local system Docker backend. Every decision is from one Host
/// port, so ModelHost can reproduce remote, rootless, and mount
/// failures without touching a developer's daemon. `target` is the space
/// of the requested filesystem ([`crate::places::space_of`]): the storage
/// must be on it, or the kind is skipped `other_fs`.
pub fn inspect_backend<H: Host>(
    host: &H,
    target: crate::places::Space,
    storage: Storage,
) -> Result<Binding, HostError> {
    let DockerEnvironment {
        host: override_host,
        context,
    } = host.docker_environment();
    if let Some(value) = override_host {
        let host = value.to_str().unwrap_or("");
        if !matches!(
            host,
            "unix:///var/run/docker.sock" | "unix:///run/docker.sock"
        ) {
            let reason = if host.starts_with("unix://") {
                Reason::UnsupportedBackend
            } else {
                Reason::RemoteBackend
            };
            return Ok(skip(
                reason,
                format!("DOCKER_HOST is {value:?}, not the local system socket"),
            ));
        }
    }
    if let Some(value) = context
        && value != "default"
    {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!("DOCKER_CONTEXT selects {value:?}, not default"),
        ));
    }
    let context = match query(host, DockerQuery::ContextShow)? {
        Ok(value) => value,
        Err(skipped) => return Ok(skipped),
    };
    if context.trim() != "default" {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!("active Docker context is {:?}, not default", context.trim()),
        ));
    }
    let endpoint = match query(host, DockerQuery::ContextEndpoint)? {
        Ok(value) => value,
        Err(skipped) => return Ok(skipped),
    };
    let endpoint: String = match serde_json::from_str(endpoint.trim()) {
        Ok(value) => value,
        Err(error) => {
            return Ok(skip(
                Reason::UnsupportedBackend,
                format!("Docker context endpoint is not JSON: {error}"),
            ));
        }
    };
    if !matches!(
        endpoint.as_str(),
        "unix:///var/run/docker.sock" | "unix:///run/docker.sock"
    ) {
        let reason = if endpoint.starts_with("unix://") {
            Reason::UnsupportedBackend
        } else {
            Reason::RemoteBackend
        };
        return Ok(skip(
            reason,
            format!("Docker context endpoint {endpoint:?} is not the local system socket"),
        ));
    }
    let version = match query(host, DockerQuery::Version)? {
        Ok(value) => value,
        Err(skipped) => return Ok(skipped),
    };
    let server: Value = match serde_json::from_str(&version) {
        Ok(value) => value,
        Err(error) => {
            return Ok(skip(
                Reason::UnsupportedBackend,
                format!("Docker server identity is not JSON: {error}"),
            ));
        }
    };
    let name = server["Platform"]["Name"].as_str().unwrap_or("");
    if !name.starts_with("Docker Engine") {
        return Ok(skip(
            Reason::UnsupportedBackend,
            format!("server is {name:?}, not Docker Engine"),
        ));
    }
    if storage == Storage::BuildCache {
        // Docker CLI 29.8.2 forwards `docker builder` to Buildx unless the
        // prune child sets DOCKER_BUILDKIT=0. This dispatch was checked in
        // the pinned native fixture. Other client versions may dispatch
        // differently and must not act on an unbound Buildx store.
        let client_text = match query(host, DockerQuery::ClientVersion)? {
            Ok(value) => value,
            Err(skipped) => return Ok(skipped),
        };
        let client: Value = match serde_json::from_str(&client_text) {
            Ok(value) => value,
            Err(error) => {
                return Ok(skip(
                    Reason::UnsupportedBackend,
                    format!("Docker client identity is not JSON: {error}"),
                ));
            }
        };
        if client["Version"].as_str() != Some("29.8.2") {
            return Ok(skip(
                Reason::UnsupportedBackend,
                format!(
                    "Docker client version {:?} has unverified builder dispatch; 29.8.2 required",
                    client["Version"]
                ),
            ));
        }
    }
    let info_text = match query(host, DockerQuery::Info)? {
        Ok(value) => value,
        Err(skipped) => return Ok(skipped),
    };
    let info = match DockerInfo::parse(&info_text) {
        Ok(info) => info,
        Err(error) => return Ok(skip(Reason::UnsupportedBackend, error)),
    };
    let build_cache = info.docker_root.join("buildkit");
    let roots = match storage {
        Storage::BuildCache => [info.docker_root.as_path(), build_cache.as_path()],
        Storage::Images => [info.docker_root.as_path(), info.image_store.root()],
    };
    for path in roots {
        match resolve::resolve(host, path, Want::Directory) {
            Ok(resolved)
                if crate::places::space_at(host, &resolved.mount, &resolved.fd)
                    .same_as(&target) => {}
            Ok(resolved) => {
                return Ok(skip(
                    Reason::OtherFs,
                    format!(
                        "Docker storage {} is on {}, requested filesystem is {target}",
                        path.display(),
                        crate::places::space_at(host, &resolved.mount, &resolved.fd)
                    ),
                ));
            }
            Err(ResolveError::Host(error)) => return Err(error),
            Err(error) => {
                return Ok(skip(
                    error
                        .skip_reason()
                        .expect("a host error returned above; every other one is a skip"),
                    format!(
                        "Docker storage {} cannot be safely resolved: {error}",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(Binding::Bound(Bound { info, endpoint }))
}

fn absolute_clean_path(text: &str, label: &str) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "{label} must be an absolute normalized path, got {text:?}"
        ));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_store_uses_docker_root_and_refuses_external_containerd() {
        let json = serde_json::json!({"ID":"daemon-1","DockerRootDir":"/private/docker","Driver":"overlayfs","DriverStatus":[["driver-type","io.containerd.snapshotter.v1"]],"Containerd":{"Address":"/run/docker/containerd/containerd.sock"}}).to_string();
        let info = DockerInfo::parse(&json).unwrap();
        assert_eq!(
            info.image_store.root(),
            Path::new("/private/docker/containerd/daemon")
        );
        let external = json.replace(
            "/run/docker/containerd/containerd.sock",
            "/run/containerd/containerd.sock",
        );
        assert!(
            DockerInfo::parse(&external)
                .unwrap_err()
                .contains("unsupported_backend")
        );
    }

    #[test]
    fn classic_store_uses_docker_root_and_rejects_ambiguous_input() {
        let json = serde_json::json!({"ID":"daemon-1","DockerRootDir":"/private/docker","Driver":"vfs","DriverStatus":null}).to_string();
        assert_eq!(
            DockerInfo::parse(&json).unwrap().image_store,
            ImageStore::Classic(PathBuf::from("/private/docker"))
        );
        assert!(DockerInfo::parse(&json.replace("/private/docker", "relative/../docker")).is_err());
        assert!(DockerInfo::parse(&json.replace("\"vfs\"", "\"unknown\"")).is_err());
        assert!(DockerInfo::parse(&json.replace(",\"DriverStatus\":null", "")).is_err());
        let malformed = serde_json::json!({"ID":"daemon-1","DockerRootDir":"/private/docker","Driver":"overlayfs","DriverStatus":[["driver-type","io.containerd.snapshotter.v1"],42],"Containerd":{"Address":"/run/docker/containerd/containerd.sock"}}).to_string();
        assert!(DockerInfo::parse(&malformed).is_err());
        let duplicate = serde_json::json!({"ID":"daemon-1","DockerRootDir":"/private/docker","Driver":"overlayfs","DriverStatus":[["driver-type","io.containerd.snapshotter.v1"],["driver-type","io.containerd.snapshotter.v1"]],"Containerd":{"Address":"/run/docker/containerd/containerd.sock"}}).to_string();
        assert!(DockerInfo::parse(&duplicate).is_err());
        let unknown = serde_json::json!({"ID":"daemon-1","DockerRootDir":"/private/docker","Driver":"overlayfs","DriverStatus":[["driver-type","unknown"]]}).to_string();
        assert!(
            DockerInfo::parse(&unknown)
                .unwrap_err()
                .contains("unsupported_backend")
        );
    }
}
