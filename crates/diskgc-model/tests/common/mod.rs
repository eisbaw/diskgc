//! Helpers shared by the model tests.

#![allow(dead_code)] // each test binary uses its own subset

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use diskgc_core::engine::Rules;
use diskgc_core::profile::Library;
use diskgc_core::resolve::{Resolved, Want, resolve};
use diskgc_model::{ModelFd, ModelHost};

/// `/` (ext4) and a 1 GiB tmpfs on `/tmp` (mount 30) holding `/tmp/a` with
/// a 10 KiB file `f` and an empty directory `sub`. `extra` is appended
/// (more top-level keys).
pub fn tmp_yaml(extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /tmp/a:
    f: {{size: 10K}}
    sub: {{.: {{}}}}
{extra}"#
    )
}

pub fn host(yaml: &str) -> ModelHost {
    ModelHost::from_yaml(yaml).unwrap_or_else(|e| panic!("{e}"))
}

/// Resolves a directory the way the core does.
pub fn dir(host: &ModelHost, path: &str) -> Resolved<ModelFd> {
    resolve(host, Path::new(path), Want::Directory)
        .unwrap_or_else(|e| panic!("resolve {path}: {e}"))
}

/// The space of the filesystem `path` is on, as the core reads it
/// ([`diskgc_core::places::space_at`]).
#[allow(dead_code)]
pub fn space(host: &ModelHost, path: &str) -> diskgc_core::places::Space {
    let at = dir(host, path);
    diskgc_core::places::space_at(host, &at.mount, &at.fd)
}

/// The walk rules the executor builds for a `profile:cargo-target` tier
/// run by `owners`: the defaults ([`Rules::new`]) and the built-in
/// profile's `checkouts` setting, read from the library as the executor
/// does ([`Library::checkouts`]), never stated here.
pub fn cargo_target_rules(owners: &[u32]) -> Rules<'_> {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    Rules {
        checkouts: library
            .checkouts("cargo-target")
            .expect("a built-in source profile `cargo-target`"),
        ..Rules::new(owners)
    }
}

/// Runs `f`, which must fail the test, and returns the failure message.
pub fn failure(f: impl FnOnce()) -> String {
    let err = catch_unwind(AssertUnwindSafe(f)).expect_err("expected the test to fail");
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}
