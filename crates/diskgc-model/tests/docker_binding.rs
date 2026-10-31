use diskgc_core::docker_backend::{self, Binding, ImageStore, Storage};
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::Dev;
use diskgc_core::profile::Library;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use std::path::PathBuf;
use std::time::Duration;

const BUNDLED: &str = include_str!("models/docker-bundled.yaml");
const DOCKER_FS: Dev = Dev {
    major: 254,
    minor: 2,
};

fn bind(model: &str, target: Dev) -> (Binding, ModelHost) {
    let host = ModelHost::from_yaml(model).expect("valid fake host");
    let result =
        docker_backend::inspect_backend(&host, target, Storage::Images).expect("query succeeds");
    (result, host)
}

fn skipped(model: &str, target: Dev, reason: Reason) {
    let (binding, _) = bind(model, target);
    match binding {
        Binding::Skipped(skip) => assert_eq!(skip.reason, reason, "{skip:?}"),
        Binding::Bound(bound) => panic!("unsafe backend was bound: {bound:?}"),
    }
}

#[test]
fn binds_only_the_real_bundled_image_root_on_the_requested_filesystem() {
    let (binding, host) = bind(BUNDLED, DOCKER_FS);
    let Binding::Bound(bound) = binding else {
        panic!("expected bound backend: {binding:?}")
    };
    assert_eq!(
        bound.info.image_store,
        ImageStore::BundledContainerd("/var/lib/docker/containerd/daemon".into())
    );
    assert_eq!(host.trace_count(Op::DockerQuery), 4);
    skipped(
        BUNDLED,
        Dev {
            major: 254,
            minor: 1,
        },
        Reason::OtherFs,
    );
}

#[test]
fn build_cache_refuses_a_separate_buildkit_mount() {
    let model = BUNDLED.replace("  /var/lib/docker/buildkit: {.: {}}\n", "").replace(
        "tree:\n",
        "  - {id: 3, at: /var/lib/docker/buildkit, fs: ext4, dev: \"254:3\", size: 10G, parent: 2}\ntree:\n",
    );
    let host = ModelHost::from_yaml(&model).expect("valid separate BuildKit mount");
    let binding = docker_backend::inspect_backend(&host, DOCKER_FS, Storage::BuildCache)
        .expect("queries succeed");
    assert!(
        matches!(binding, Binding::Skipped(ref skip) if skip.reason == Reason::OtherFs),
        "{binding:?}"
    );
    assert_eq!(host.trace_count(Op::RunCommand), 0);
    assert!(matches!(
        docker_backend::inspect_backend(&host, DOCKER_FS, Storage::Images).unwrap(),
        Binding::Bound(_)
    ));
}

#[test]
fn build_cache_binding_does_not_require_the_image_store_mount() {
    let model = BUNDLED.replace(
        "  /var/lib/docker/containerd/daemon: {.: {}, io.containerd.content.v1.content: {.: {}}}\n",
        "",
    ).replace(
        "tree:\n",
        "  - {id: 3, at: /var/lib/docker/containerd/daemon, fs: ext4, dev: \"254:3\", size: 10G, parent: 2}\ntree:\n",
    );
    let host = ModelHost::from_yaml(&model).expect("valid separate image-store mount");
    assert!(matches!(
        docker_backend::inspect_backend(&host, DOCKER_FS, Storage::BuildCache).unwrap(),
        Binding::Bound(_)
    ));
    assert!(matches!(
        docker_backend::inspect_backend(&host, DOCKER_FS, Storage::Images).unwrap(),
        Binding::Skipped(ref skip) if skip.reason == Reason::OtherFs
    ));
}

#[test]
fn remote_rootless_and_external_store_fail_closed() {
    skipped(
        &BUNDLED.replace(
            "context_endpoint: '\"unix:///var/run/docker.sock\"'",
            "context_endpoint: '\"tcp://remote:2375\"'",
        ),
        DOCKER_FS,
        Reason::RemoteBackend,
    );
    skipped(
        &BUNDLED.replace(
            "context_endpoint: '\"unix:///var/run/docker.sock\"'",
            "context_endpoint: '\"unix:///run/user/1000/docker.sock\"'",
        ),
        DOCKER_FS,
        Reason::UnsupportedBackend,
    );
    skipped(
        &BUNDLED.replace(
            "/run/docker/containerd/containerd.sock",
            "/run/containerd/containerd.sock",
        ),
        DOCKER_FS,
        Reason::UnsupportedBackend,
    );
    let model = format!("{BUNDLED}\n  host: tcp://remote:2375\n");
    let host = ModelHost::from_yaml(&model).expect("valid fake host");
    let result = docker_backend::inspect_backend(&host, DOCKER_FS, Storage::Images).unwrap();
    assert!(matches!(result, Binding::Skipped(ref s) if s.reason == Reason::RemoteBackend));
    assert_eq!(host.trace_count(Op::DockerQuery), 0);

    let model = format!("{BUNDLED}\n  host: unix:///run/user/1000/docker.sock\n");
    let host = ModelHost::from_yaml(&model).expect("valid fake host");
    let result = docker_backend::inspect_backend(&host, DOCKER_FS, Storage::Images).unwrap();
    assert!(matches!(result, Binding::Skipped(ref s) if s.reason == Reason::UnsupportedBackend));
    assert_eq!(host.trace_count(Op::DockerQuery), 0);
}

#[test]
fn unverified_docker_client_dispatch_refuses_build_cache_only() {
    let model = BUNDLED.replace(
        "client_version: '{\"Version\":\"29.8.2\"}'",
        "client_version: '{\"Version\":\"30.0.0\"}'",
    );
    let host = ModelHost::from_yaml(&model).unwrap();
    assert!(matches!(
        docker_backend::inspect_backend(&host, DOCKER_FS, Storage::BuildCache).unwrap(),
        Binding::Skipped(ref skip) if skip.reason == Reason::UnsupportedBackend
    ));
    assert_eq!(host.trace_count(Op::RunCommand), 0);
    assert!(matches!(
        docker_backend::inspect_backend(&host, DOCKER_FS, Storage::Images).unwrap(),
        Binding::Bound(_)
    ));
}

fn request(profile: &str) -> Request {
    request_select(&format!("profile:{profile}"))
}

fn request_select(selection: &str) -> Request {
    Request {
        path: PathBuf::from("/var/lib/docker"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(60)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(selection, &Library::builtin().unwrap(), None, 1000).unwrap(),
    }
}

#[test]
fn build_cache_idle_filter_reaches_engine_prune() {
    let host = ModelHost::from_yaml(BUNDLED).unwrap();
    let uid = executor::privileges(&host, false).unwrap();
    let request = request_select("profile:docker-build-cache && filter:idle-1h");
    let report = executor::ensure(&host, &request, &[uid], &mut |_| {});
    assert_eq!(report.actions.len(), 1, "{report:?}");
    let command = host
        .trace()
        .into_iter()
        .find_map(|call| call.command)
        .expect("prune ran");
    let arguments: Vec<_> = command
        .arguments
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(
        arguments.ends_with(&["--filter".into(), "until=3600s".into()]),
        "{arguments:?}"
    );
}

#[test]
fn each_docker_profile_uses_one_fixed_whole_backend_action() {
    for (profile, candidate, argv) in [
        (
            "docker-build-cache",
            "/var/lib/docker/buildkit",
            vec![
                "--host=unix:///var/run/docker.sock",
                "builder",
                "prune",
                "--all",
                "--force",
            ],
        ),
        (
            "docker-dangling",
            "/var/lib/docker/containerd/daemon",
            vec![
                "--host=unix:///var/run/docker.sock",
                "image",
                "prune",
                "--force",
            ],
        ),
        (
            "docker-unreferenced",
            "/var/lib/docker/containerd/daemon",
            vec![
                "--host=unix:///var/run/docker.sock",
                "image",
                "prune",
                "--force",
                "--all",
            ],
        ),
    ] {
        let host = ModelHost::from_yaml(BUNDLED).unwrap();
        let uid = executor::privileges(&host, false).unwrap();
        let plan = executor::dry_run(&host, &request(profile), &[uid], &mut |_| {});
        assert_eq!(plan.plan.len(), 1, "{profile}: {plan:?}");
        assert_eq!(plan.plan[0].candidate, Some(PathBuf::from(candidate)));
        assert_eq!(host.trace_count(Op::RunCommand), 0);

        let report = executor::ensure(&host, &request(profile), &[uid], &mut |_| {});
        assert_eq!(report.actions.len(), 1, "{profile}: {report:?}");
        let command = host
            .trace()
            .into_iter()
            .find_map(|call| call.command)
            .expect("prune ran");
        let actual: Vec<_> = command
            .arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(actual, argv, "{profile}");
        assert_eq!(host.trace_count(Op::Unlinkat), 0);
    }
}

/// podman-docker links Podman's compatibility socket at the Docker system
/// socket path, so endpoint checks alone pass. Only the server identity says
/// it is not Docker Engine; it must be refused before any storage query.
#[test]
fn podman_compatibility_server_at_the_system_socket_is_unsupported() {
    let model = BUNDLED.replace(
        r#"version: '{"Platform":{"Name":"Docker Engine - Community"},"Version":"29.8.2"}'"#,
        r#"version: '{"Platform":{"Name":"Podman Engine"},"Version":"5.8.7"}'"#,
    );
    assert_ne!(model, BUNDLED, "fixture server identity line moved");
    for storage in [Storage::Images, Storage::BuildCache] {
        let host = ModelHost::from_yaml(&model).expect("valid fake host");
        let binding = docker_backend::inspect_backend(&host, DOCKER_FS, storage).unwrap();
        assert!(
            matches!(binding, Binding::Skipped(ref s) if s.reason == Reason::UnsupportedBackend
                && s.detail.contains("not Docker Engine")),
            "{binding:?}"
        );
        // Context show, context inspect and version; no `docker info`.
        assert_eq!(host.trace_count(Op::DockerQuery), 3);
    }
}

/// A Docker client that is not installed (no `docker` on the command
/// path): the first query fails ENOENT, as LinuxHost's spawn does, and the
/// backend is `missing` without a further query.
#[test]
fn a_missing_docker_client_is_missing_after_one_query() {
    let model = BUNDLED.replace("  /usr/bin/docker: {mode: \"0755\"}\n", "");
    assert_ne!(model, BUNDLED, "fixture client line moved");
    let (binding, host) = bind(&model, DOCKER_FS);
    assert!(
        matches!(binding, Binding::Skipped(ref s) if s.reason == Reason::Missing),
        "{binding:?}"
    );
    assert_eq!(host.trace_count(Op::DockerQuery), 1);
}
