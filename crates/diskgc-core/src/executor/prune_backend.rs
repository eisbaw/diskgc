//! The backend kinds (Docker's two, `podman-images`) seen alike by the
//! plan, the preflight under the host lock and the reclaim walk: each binds
//! one backend, shows one candidate (the storage the prune frees), and
//! runs one prune through its client, rebound right before it starts
//! (docs/reference.md, Backend skips and errors). What differs per backend
//! is here; the walk is [`super::Reclaim::walk_backend`].

use std::ffi::OsString;
use std::path::PathBuf;

use crate::backend::Binding;
use crate::docker_backend::{self, DockerInfo, Storage};
use crate::host::{Host, HostError};
use crate::places::Space;
use crate::podman_backend;
use crate::profile::Action;

use super::{SourceKind, Tier};

/// Which backend a tier prunes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Which {
    Docker(Storage),
    Podman,
}

impl Which {
    /// `None`: the tier is not of a backend kind.
    pub(super) fn of(tier: &Tier) -> Option<Self> {
        match tier.source {
            SourceKind::DockerBuildCache => Some(Self::Docker(Storage::BuildCache)),
            SourceKind::DockerImages => Some(Self::Docker(Storage::Images)),
            SourceKind::PodmanImages => Some(Self::Podman),
            SourceKind::CargoTarget | SourceKind::Fs(_) => None,
        }
    }

    /// The backend's name in log lines and errors.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Docker(_) => "Docker",
            Self::Podman => "Podman",
        }
    }

    /// The client program, looked up on `PATH` in the preflight.
    pub(super) fn client(self) -> &'static str {
        match self {
            Self::Docker(_) => "docker",
            Self::Podman => "podman",
        }
    }

    /// The operation a failed prune child reports.
    pub(super) fn prune_op(self) -> &'static str {
        match self {
            Self::Docker(_) => "Docker prune",
            Self::Podman => "Podman prune",
        }
    }

    /// The detail of a `changed` skip at the recheck.
    pub(super) fn changed(self) -> &'static str {
        match self {
            Self::Docker(_) => "Docker daemon, context, builder or storage changed before prune",
            Self::Podman => "Podman version, store or its configuration changed before prune",
        }
    }
}

/// A bound backend of either kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Bound {
    Docker(docker_backend::Bound),
    Podman(podman_backend::Bound),
}

/// What the prune child runs after its program, with which environment,
/// and (Docker only) the daemon identity its socket pin rechecks.
pub(super) struct Prune {
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub docker_info: Option<DockerInfo>,
}

/// Bind `which` against the requested filesystem's `space`.
pub(super) fn inspect<H: Host>(
    host: &H,
    space: Space,
    which: Which,
) -> Result<Binding<Bound>, HostError> {
    Ok(match which {
        Which::Docker(storage) => match docker_backend::inspect_backend(host, space, storage)? {
            Binding::Bound(bound) => Binding::Bound(Bound::Docker(bound)),
            Binding::Skipped(skipped) => Binding::Skipped(skipped),
        },
        Which::Podman => match podman_backend::inspect_backend(host, space)? {
            Binding::Bound(bound) => Binding::Bound(Bound::Podman(bound)),
            Binding::Skipped(skipped) => Binding::Skipped(skipped),
        },
    })
}

impl Bound {
    /// The storage the prune frees: the plan's and the action's candidate,
    /// judged by `protect` only (nothing below it is walked).
    pub(super) fn candidate(&self, tier: &Tier) -> PathBuf {
        match (self, &tier.source) {
            (Self::Docker(bound), SourceKind::DockerBuildCache) => {
                bound.info.docker_root.join("buildkit")
            }
            (Self::Docker(bound), SourceKind::DockerImages) => {
                bound.info.image_store.root().to_owned()
            }
            (Self::Podman(bound), SourceKind::PodmanImages) => bound.info.graph_root.clone(),
            _ => unreachable!("a backend tier is bound to its own backend"),
        }
    }

    /// The prune of `tier`'s action through this backend.
    pub(super) fn prune(&self, tier: &Tier) -> Prune {
        match (self, &tier.action) {
            (Self::Docker(bound), action) => {
                let mut arguments: Vec<OsString> =
                    vec!["--host=unix:///var/run/docker.sock".into()];
                match action {
                    Action::DockerBuilderPrune => {
                        arguments
                            .extend(["builder", "prune", "--all", "--force"].map(OsString::from));
                        if let Some(idle) = tier.idle_for {
                            arguments.push("--filter".into());
                            arguments.push(format!("until={}s", idle.0.as_secs()).into());
                        }
                    }
                    Action::DockerImagePrune { all } => {
                        arguments.extend(["image", "prune", "--force"].map(OsString::from));
                        if *all {
                            arguments.push("--all".into());
                        }
                    }
                    _ => unreachable!("Docker tier with non-Docker action admitted by ensure"),
                }
                Prune {
                    arguments,
                    environment: vec![
                        ("DOCKER_HOST".into(), "unix:///var/run/docker.sock".into()),
                        ("DOCKER_CONTEXT".into(), "default".into()),
                        // Docker CLI 29 forwards `docker builder` to Buildx
                        // unless DOCKER_BUILDKIT=0 selects the Engine
                        // command. The latter calls BuildCachePrune on the
                        // pinned daemon directly.
                        ("DOCKER_BUILDKIT".into(), "0".into()),
                    ],
                    docker_info: Some(bound.info.clone()),
                }
            }
            (Self::Podman(_), Action::PodmanImagePrune { all }) => Prune {
                arguments: podman_backend::prune_arguments(*all)
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                environment: Vec::new(),
                docker_info: None,
            },
            (Self::Podman(_), _) => {
                unreachable!("Podman tier with non-Podman action admitted by ensure")
            }
        }
    }
}
