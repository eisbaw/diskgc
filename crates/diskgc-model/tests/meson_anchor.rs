//! Meson source evidence through ModelHost. The checked-in profile vector
//! covers the full plan and reclaim surface; these cases pin parse failures
//! and races at the anchor boundary.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::Library;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const ROOT: &str = "/home/tester/project";
const SOURCE: &str = "/home/tester/project/app";
const CANDIDATE: &str = "/home/tester/project/app/odd-output";
const OLD: &str = "2026-10-01T00:00:00Z";

fn request() -> Request {
    let library = Library::builtin().expect("built-ins");
    let roots = [ROOT.to_owned()];
    Request {
        path: PathBuf::from(ROOT),
        min_free: parse_size("100%").expect("size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:meson-build-anchored", &library, Some(&roots), 1000)
            .expect("Meson source"),
    }
}

fn model(build: &str, source_file: bool, duplicate_build: bool, events: &str) -> ModelHost {
    let source_entry = if source_file {
        format!("  {SOURCE}/meson.build: {{content: \"project('app')\\n\"}}\n")
    } else {
        String::new()
    };
    let duplicate = if duplicate_build {
        format!(",\"build\":\"{CANDIDATE}\"")
    } else {
        String::new()
    };
    let json = format!(
        "{{\"directories\":{{\"source\":\"{SOURCE}\",\"build\":\"{build}\"{duplicate},\"info\":\"{CANDIDATE}/meson-info\"}},\"error\":false}}"
    );
    host(&format!(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - {{id: 1, at: /, fs: ext4, dev: \"254:1\", size: 10G}}\n  - {{id: 2, at: /home, fs: ext4, dev: \"254:2\", size: 10G, parent: 1}}\ntree:\n  /run/diskgc/lock: {{uid: 0, gid: 0}}\n{source_entry}  {CANDIDATE}:\n    .: {{mtime: {OLD}}}\n    build.ninja: {{mtime: {OLD}}}\n    meson-private: {{.: {{mtime: {OLD}}}, coredata.dat: {{content: coredata, mtime: {OLD}}}}}\n    meson-info: {{.: {{mtime: {OLD}}}, meson-info.json: {{content: {json:?}, mtime: {OLD}}}}}\n    object.o: {{size: 8K, mtime: {OLD}}}\n{events}"
    ))
}

fn ensure(host: &ModelHost) -> diskgc_core::report::Report {
    let uid = executor::privileges(host, false).expect("privileges");
    executor::ensure(host, &request(), uid, &mut |_| {})
}

fn untouched(host: &ModelHost) {
    assert!(
        host.stat_path(Path::new(&format!("{CANDIDATE}/object.o")))
            .is_some()
    );
    assert!(
        host.trace()
            .iter()
            .filter(|call| call.op == Op::Unlinkat)
            .all(|call| !call
                .path
                .as_deref()
                .is_some_and(|path| path.starts_with(CANDIDATE)))
    );
}

#[test]
fn meson_build_path_must_name_the_candidate_exactly() {
    let host = model("/home/tester/project/app/other-output", true, false, "");
    let report = ensure(&host);
    assert!(report.actions.is_empty(), "{report:#?}");
    untouched(&host);
}

#[test]
fn meson_requires_a_surviving_build_file_and_unambiguous_json() {
    for (name, source_file, duplicate_build) in [
        ("missing source", false, false),
        ("duplicate build", true, true),
    ] {
        let host = model(CANDIDATE, source_file, duplicate_build, "");
        let report = ensure(&host);
        assert!(report.actions.is_empty(), "{name}: {report:#?}");
        untouched(&host);
    }
}

#[test]
fn meson_source_file_identity_is_rechecked_under_candidate_lock() {
    let events = format!(
        "events:\n  - {{at: {{op: flock, path: {CANDIDATE}, when: after}}, remove: {SOURCE}/meson.build}}\n"
    );
    let host = model(CANDIDATE, true, false, &events);
    let report = ensure(&host);
    assert!(host.unfired_events().is_empty(), "{report:#?}");
    assert!(report.actions.is_empty(), "{report:#?}");
    untouched(&host);
}
