//! Strict loading (task-14 AC #3): unknown keys and dangling references are
//! load errors, and the committed JSON Schema matches the format.

mod common;

use common::tmp_yaml;
use diskgc_model::ModelHost;

fn load_error(yaml: &str) -> String {
    match ModelHost::from_yaml(yaml) {
        Ok(_) => panic!("loaded, but should not:\n{yaml}"),
        Err(e) => e.to_string(),
    }
}

fn assert_error(yaml: &str, wanted: &[&str]) {
    let message = load_error(yaml);
    for part in wanted {
        assert!(message.contains(part), "{part:?} not in: {message}");
    }
}

#[test]
fn the_base_model_loads() {
    ModelHost::from_yaml(&tmp_yaml("")).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn unknown_keys_are_load_errors_at_every_level() {
    let cases = [
        ("top level", tmp_yaml("colour: blue\n")),
        (
            "mount",
            tmp_yaml("").replace("size: 1G,", "size: 1G, quota: 1G,"),
        ),
        (
            "file",
            tmp_yaml("").replace("{size: 10K}", "{size: 10K, sise: 1}"),
        ),
        (
            "directory attributes",
            tmp_yaml("").replace("{.: {}}", "{.: {owner: 1000}}"),
        ),
        (
            "process",
            tmp_yaml("processes:\n  - {pid: 7, cmdline: x}\n"),
        ),
        (
            "process fd",
            tmp_yaml("processes:\n  - {pid: 7, fds: [{path: /tmp/a/f, flags: r}]}\n"),
        ),
        (
            "lock",
            tmp_yaml(
                "processes:\n  - {pid: 7}\nlocks:\n  - {path: /tmp/a/f, holder: 7, mode: shared, waiters: []}\n",
            ),
        ),
        (
            "event",
            tmp_yaml("events:\n  - {at: {elapsed: 1s}, create: /tmp/x, after: 1}\n"),
        ),
        (
            "event trigger",
            tmp_yaml("events:\n  - {at: {elapsed: 1s, every: 2s}, create: /tmp/x}\n"),
        ),
        ("expect", tmp_yaml("expect: {exit: 0, deleted: []}\n")),
    ];
    for (level, yaml) in cases {
        let message = load_error(&yaml);
        assert!(message.contains("unknown field"), "{level}: {message}");
    }
}

#[test]
fn dangling_pid_references_are_load_errors() {
    assert_error(
        &tmp_yaml("locks:\n  - {path: /tmp/a/f, holder: 4242, mode: exclusive}\n"),
        &["locks[0]", "holder 4242 is not one of the processes"],
    );
    for action in [
        "exit: 9",
        "open: {pid: 9, path: /tmp/a/f}",
        "close: {pid: 9, path: /tmp/a/f}",
    ] {
        assert_error(
            &tmp_yaml(&format!(
                "processes:\n  - {{pid: 7}}\nevents:\n  - {{at: {{elapsed: 1s}}, {action}}}\n"
            )),
            &["events[0]", "pid 9 is not one of the processes"],
        );
    }
}

#[test]
fn dangling_paths_and_mounts_are_load_errors() {
    assert_error(
        &tmp_yaml("processes:\n  - {pid: 7, cwd: /tmp/nowhere}\n"),
        &["processes[pid 7].cwd", "/tmp/nowhere is not in the tree"],
    );
    assert_error(
        &tmp_yaml("processes:\n  - {pid: 7, fds: [{path: /tmp/a/g}]}\n"),
        &["processes[pid 7].fds", "/tmp/a/g is not in the tree"],
    );
    assert_error(
        &tmp_yaml(
            "processes:\n  - {pid: 7}\nlocks:\n  - {path: /tmp/b, holder: 7, mode: shared}\n",
        ),
        &["locks[0].path", "/tmp/b is not in the tree"],
    );
    assert_error(
        &tmp_yaml("").replace("{size: 10K}", "{link: /tmp/a/missing}"),
        &["tree[/tmp/a]/f.link"],
    );
    assert_error(
        &tmp_yaml("").replace("parent: 1}", "parent: 7}"),
        &[
            "mounts[id 30]",
            "parent 7 is not a mount declared before it",
        ],
    );
}

#[test]
fn contradictions_are_load_errors() {
    assert_error(
        &tmp_yaml("processes:\n  - {pid: 7}\n  - {pid: 7}\n"),
        &["pid declared twice"],
    );
    assert_error(
        &tmp_yaml("  /tmp/a/f: {size: 1K}\n"),
        &["tree[/tmp/a/f]", "declared twice"],
    );
    assert_error(
        &tmp_yaml("").replace("{size: 10K}", "{size: 10K, symlink: /x}"),
        &["only regular files have a size"],
    );
    assert_error(
        &tmp_yaml("").replace(
            "    sub: {.: {}}",
            "    sub: {.: {}}\n    sub2: {x: {}, size: 1}",
        ),
        &["mixes file attributes"],
    );
    assert_error(
        &tmp_yaml(
            "processes:\n  - {pid: 7}\n  - {pid: 8}\nlocks:\n  - {path: /tmp/a/f, holder: 7, mode: exclusive}\n  - {path: /tmp/a/f, holder: 8, mode: shared}\n",
        ),
        &["locks[1]", "conflicts"],
    );
    assert_error(
        &tmp_yaml("").replace("dev: \"0:42\"", "dev: \"254:1\""),
        &["mounted twice without bind_of"],
    );
}

#[test]
fn root_bind_requires_an_earlier_matching_filesystem() {
    let bind = |extra: &str| {
        tmp_yaml("").replace(
        "tree:\n",
        &format!("  - {{id: 31, at: /tmp/a/sub/bind, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 30, bind_of: 30{extra}}}\ntree:\n"),
    )
    };
    ModelHost::from_yaml(&bind("")).expect("matching root bind loads");
    assert_error(
        &bind("").replace("bind_of: 30", "bind_of: 99"),
        &["bind_of 99", "not an earlier mount"],
    );
    assert_error(
        &bind("").replace(
            "size: 1G, parent: 30, bind_of",
            "size: 2G, parent: 30, bind_of",
        ),
        &["incompatible filesystem metadata"],
    );
    assert_error(
        &bind("").replace(
            "dev: \"0:42\", size: 1G, parent: 30, bind_of",
            "dev: \"0:43\", size: 1G, parent: 30, bind_of",
        ),
        &["no filesystem on device"],
    );
}

#[test]
fn events_need_exactly_one_trigger_and_one_action() {
    for (event, wanted) in [
        ("{at: {op: statx}, create: /tmp/x}", "needs 'when"),
        (
            "{at: {op: statx, when: after, elapsed: 1s}, create: /tmp/x}",
            "exactly one of",
        ),
        (
            "{at: {op: statx, nth: 0, when: after}, create: /tmp/x}",
            "nth counts from 1",
        ),
        ("{at: {elapsed: 1s}}", "no action"),
        (
            "{at: {elapsed: 1s}, create: /tmp/x, mkdir: /tmp/y}",
            "more than one action",
        ),
        ("{at: {elapsed: 1s}, create: tmp/x}", "not an absolute path"),
        // A failure is injected into one call that can fail that way.
        (
            "{at: {elapsed: 1s}, fail: EIO}",
            "'fail' needs an op trigger",
        ),
        (
            "{at: {call: 3, when: before}, fail: EIO}",
            "'fail' needs an op trigger",
        ),
        (
            "{at: {op: statx, when: after}, fail: EIO}",
            "'fail' needs an op trigger",
        ),
        (
            "{at: {op: now, when: before}, fail: EIO}",
            "'fail' needs an op trigger",
        ),
        (
            "{at: {op: statx, when: before}, fail: EWHAT}",
            "not an errno name",
        ),
    ] {
        assert_error(&tmp_yaml(&format!("events:\n  - {event}\n")), &[wanted]);
    }
}

#[test]
fn scalars_are_checked() {
    assert_error(
        &tmp_yaml("").replace("size: 1G,", "size: 1GB,"),
        &["not a size"],
    );
    assert_error(
        &tmp_yaml("").replace("{size: 10K}", "{mode: 755}"),
        &["mode"],
    );
    assert_error(
        &tmp_yaml("").replace("2026-10-15T03:00:00Z", "yesterday"),
        &["not an RFC 3339 time"],
    );
    assert_error(
        &tmp_yaml("").replace("host_model: 1", "host_model: 2"),
        &["version 2"],
    );
}

/// The committed `host-model.schema.json` is what the format types
/// generate. Regenerate it with `DISKGC_UPDATE_SCHEMA=1 cargo test -p
/// diskgc-model --test loading`.
#[test]
fn the_committed_json_schema_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("host-model.schema.json");
    let generated = serde_json::to_string_pretty(&diskgc_model::spec::json_schema())
        .expect("serialises")
        + "\n";
    if std::env::var_os("DISKGC_UPDATE_SCHEMA").is_some() {
        std::fs::write(&path, &generated).expect("writes the schema");
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale; regenerate it with DISKGC_UPDATE_SCHEMA=1 cargo test -p diskgc-model \
         --test loading",
        path.display()
    );
    // Strictness is part of the schema, so editors flag unknown keys too.
    let schema: serde_json::Value = serde_json::from_str(&committed).expect("JSON");
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}

#[test]
fn names_yaml_would_read_as_other_types_round_trip_as_names() {
    let yaml = tmp_yaml(
        "  /tmp/odd:\n    \"true\": {}\n    \"null\": {}\n    \"1\": {}\n    \"no\": {}\n    \"0755\": {}\n    \"~\": {}\n    \"a: b\": {}\n",
    );
    let model = diskgc_model::load::parse_yaml(&yaml).unwrap_or_else(|e| panic!("{e}"));
    let printed = model.to_yaml();
    let back =
        diskgc_model::load::parse_yaml(&printed).unwrap_or_else(|e| panic!("{e}\n{printed}"));
    assert_eq!(back, model, "{printed}");
    let host = ModelHost::new(&back).unwrap();
    for name in ["true", "null", "1", "no", "0755", "~", "a: b"] {
        let path = std::path::Path::new("/tmp/odd").join(name);
        assert!(host.stat_path(&path).is_some(), "{name}");
    }
}
