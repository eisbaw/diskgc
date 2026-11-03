//! TASK-93: binding the caller's local Podman store (docs/safety.md, Podman
//! images) on ModelHost. The end-to-end argv, skips and measurement are the
//! `tests/vectors/backend/podman-*` vectors; these pin the binder alone:
//! one query, no prune, and which response is a skip and which an error.

use diskgc_core::backend::Binding;
use diskgc_core::engine::Reason;
use diskgc_core::host::Dev;
use diskgc_core::podman_backend::{self, PodmanInfo};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use std::path::PathBuf;

const ROOT: &str = "/home/tester/.local/share/containers/storage";
const CONFIG: &str = "/home/tester/.config/containers/storage.conf";
const HOME_FS: Dev = Dev {
    major: 254,
    minor: 2,
};

fn info(version: &str, remote: bool, rootless: bool) -> String {
    serde_json::json!({
        "host": {"serviceIsRemote": remote, "security": {"rootless": rootless}},
        "store": {"graphRoot": ROOT, "runRoot": "/run/user/1000/containers", "graphDriverName": "overlay",
                  "configFile": CONFIG, "graphOptions": {}},
        "version": {"Version": version},
    })
    .to_string()
}

fn model(info: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 10G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 10G, parent: 1}}
tree:
  /usr/bin/podman: {{mode: "0755"}}
  {ROOT}: {{storage.lock: {{}}, overlay-layers: {{.: {{}}}}}}
podman:
  info: '{info}'
"#
    )
}

fn space(dev: Dev) -> diskgc_core::places::Space {
    diskgc_core::places::space_of(&diskgc_core::host::MountEntry {
        id: 1,
        parent_id: 1,
        dev,
        fs_root: PathBuf::from("/"),
        mount_point: PathBuf::from("/"),
        fs_type: "ext4".to_owned(),
        source: "/dev/fixture".to_owned(),
        read_only: false,
        idmapped: false,
        noatime: false,
        upper_dir: None,
    })
}

fn bind(info: &str) -> (Result<podman_backend::Binding, String>, ModelHost) {
    let host = ModelHost::from_yaml(&model(info)).expect("valid fake host");
    let result = podman_backend::inspect_backend(&host, space(HOME_FS)).map_err(|e| e.to_string());
    (result, host)
}

#[test]
fn binds_the_rootless_graph_root_on_the_requested_filesystem_with_one_query() {
    let (binding, host) = bind(&info("5.8.7", false, true));
    let Ok(Binding::Bound(bound)) = binding else {
        panic!("expected a bound store: {binding:?}")
    };
    assert_eq!(bound.info.graph_root, PathBuf::from(ROOT));
    assert_eq!(bound.graph_root.dev, HOME_FS);
    assert_eq!(host.trace_count(Op::PodmanQuery), 1);
    assert_eq!(host.trace_count(Op::RunCommand), 0);
    // Nothing below the graph root is listed: the store is Podman's.
    assert_eq!(host.trace_count(Op::ReadDir), 0);
}

#[test]
fn unadmitted_remote_and_rootful_stores_are_skipped_without_a_prune() {
    for (json, reason) in [
        (info("5.4.2", false, true), Reason::UnsupportedBackend),
        (info("5.8.7", true, true), Reason::RemoteBackend),
        (info("5.8.7", false, false), Reason::UnsupportedBackend),
    ] {
        let (binding, host) = bind(&json);
        match binding {
            Ok(Binding::Skipped(skip)) => assert_eq!(skip.reason, reason, "{json}"),
            other => panic!("{json}: expected a {reason} skip, got {other:?}"),
        }
        assert_eq!(host.trace_count(Op::RunCommand), 0);
    }
}

#[test]
fn a_response_an_admitted_version_must_not_give_is_a_run_error() {
    let missing_store = info("5.8.7", false, true).replace("\"graphRoot\"", "\"graphroot\"");
    let (binding, _) = bind(&missing_store);
    let error = binding.expect_err("no store.graphRoot is an error, not a skip");
    assert!(error.contains("store.graphRoot"), "{error}");
    assert!(PodmanInfo::parse("{").is_err());
}

#[test]
fn an_absent_graph_root_is_a_skip_not_a_fresh_store() {
    // Podman reports a graph root that is not there (never used, or on an
    // unmounted disk): nothing to prune on this filesystem, and diskgc does
    // not create it.
    let yaml = model(&info("5.8.7", false, true)).replace(
        &format!("  {ROOT}: {{storage.lock: {{}}, overlay-layers: {{.: {{}}}}}}\n"),
        "",
    );
    let host = ModelHost::from_yaml(&yaml).expect("valid fake host");
    let binding = podman_backend::inspect_backend(&host, space(HOME_FS)).expect("a skip");
    assert!(
        matches!(binding, Binding::Skipped(ref skip) if skip.reason == Reason::Missing),
        "{binding:?}"
    );
}
