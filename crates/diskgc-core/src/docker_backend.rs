//! Read-only Docker backend binding. The root is derived from daemon facts,
//! never from the client environment or a guessed default. A system
//! containerd socket is deliberately unsupported until its independent root
//! can be established from that daemon's configuration.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

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
    pub docker_root: PathBuf,
    pub image_store: ImageStore,
}

impl DockerInfo {
    pub fn parse(json: &str) -> Result<Self, String> {
        let info: Value = serde_json::from_str(json)
            .map_err(|error| format!("Docker info is not JSON: {error}"))?;
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
            // Only vfs has been observed with a null DriverStatus in the
            // disposable fixture. Other classic drivers need their own
            // binding proof before they can become deletion backends.
            ImageStore::Classic(root.clone())
        };
        Ok(Self {
            docker_root: root,
            image_store,
        })
    }
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
        let json = "{\"DockerRootDir\":\"/private/docker\",\"Driver\":\"overlayfs\",\"DriverStatus\":[[\"driver-type\",\"io.containerd.snapshotter.v1\"]],\"Containerd\":{\"Address\":\"/run/docker/containerd/containerd.sock\"}}";
        let info = DockerInfo::parse(json).unwrap();
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
        let json =
            "{\"DockerRootDir\":\"/private/docker\",\"Driver\":\"vfs\",\"DriverStatus\":null}";
        assert_eq!(
            DockerInfo::parse(json).unwrap().image_store,
            ImageStore::Classic(PathBuf::from("/private/docker"))
        );
        assert!(DockerInfo::parse(&json.replace("/private/docker", "relative/../docker")).is_err());
        assert!(DockerInfo::parse(&json.replace("\"vfs\"", "\"unknown\"")).is_err());
        assert!(DockerInfo::parse(&json.replace(",\"DriverStatus\":null", "")).is_err());
        let malformed = "{\"DockerRootDir\":\"/private/docker\",\"Driver\":\"overlayfs\",\"DriverStatus\":[[\"driver-type\",\"io.containerd.snapshotter.v1\"],42],\"Containerd\":{\"Address\":\"/run/docker/containerd/containerd.sock\"}}";
        assert!(DockerInfo::parse(malformed).is_err());
        let duplicate = "{\"DockerRootDir\":\"/private/docker\",\"Driver\":\"overlayfs\",\"DriverStatus\":[[\"driver-type\",\"io.containerd.snapshotter.v1\"],[\"driver-type\",\"io.containerd.snapshotter.v1\"]],\"Containerd\":{\"Address\":\"/run/docker/containerd/containerd.sock\"}}";
        assert!(DockerInfo::parse(duplicate).is_err());
        let unknown = "{\"DockerRootDir\":\"/private/docker\",\"Driver\":\"overlayfs\",\"DriverStatus\":[[\"driver-type\",\"unknown\"]]}";
        assert!(
            DockerInfo::parse(unknown)
                .unwrap_err()
                .contains("unsupported_backend")
        );
    }
}
