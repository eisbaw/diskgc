//! TASK-78: the Electron application caches (`teams-for-linux-electron-cache`,
//! `figma-linux-electron-cache`) in `ensure --dry-run` on the model host.
//!
//! Each source matches only named Chromium cache directories in its app's
//! user-data directory below a supplied home root, and is gated on that
//! app's own Chromium `SingletonLock` (`app_not_running`), as the browser
//! caches are (docs/reference.md, Built-in profile library; docs/safety.md,
//! Producer liveness). Any present lock retains every cache of its app,
//! whether its pid is live here, gone, or names another host.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::Library;
use diskgc_core::report::Report;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

/// One Electron app: its profile, its user-data directory below
/// `.config`, and the cache directories (relative to that directory) the
/// profile must match.
struct App {
    profile: &'static str,
    config: &'static str,
    caches: &'static [&'static str],
}

const APPS: [App; 2] = [
    App {
        profile: "teams-for-linux-electron-cache",
        config: "teams-for-linux",
        // The default session and every persistent partition
        // (`persist:teams-4-linux`, multi-account `persist:teams-profile-*`).
        caches: &[
            "Cache",
            "Code Cache",
            "GPUCache",
            "Partitions/teams-4-linux/Cache",
            "Partitions/teams-4-linux/Code Cache",
            "Partitions/teams-4-linux/GPUCache",
            "Partitions/teams-profile-1/Cache",
            "Partitions/teams-profile-1/Code Cache",
            "Partitions/teams-profile-1/GPUCache",
        ],
    },
    App {
        profile: "figma-linux-electron-cache",
        config: "figma-linux",
        // figma-linux uses the default session only: no partitions.
        caches: &["Cache", "Code Cache", "GPUCache"],
    },
];

/// Electron user-data directories of apps that are not candidates
/// (Cursor, draw.io and Saleae Logic: no app-wide SingletonLock; see the
/// library notes in builtin.yaml).
const OTHER_APPS: [&str; 3] = ["Cursor", "draw.io", "Logic"];

fn yaml(extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
hostname: model-host
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, parent: 1}}
tree:
  /usr/bin/tool: {{}}
{extra}"#
    )
}

/// The user-data tree of every app: each cache to match, and around it
/// site data, profile data and cache-shaped names no profile may match.
fn tree() -> String {
    let mut tree = String::new();
    for app in &APPS {
        let base = format!("/home/mark/.config/{}", app.config);
        for cache in app.caches {
            tree.push_str(&format!(
                "  \"{base}/{cache}\": {{.: {{}}, data: {{size: 4K}}}}\n"
            ));
        }
        for (path, entry) in [
            // Site data and settings: never cache.
            ("Local Storage/leveldb", "{.: {}, log: {size: 9K}}"),
            (
                "IndexedDB/https_x.indexeddb.leveldb",
                "{.: {}, log: {size: 9K}}",
            ),
            ("Service Worker/CacheStorage", "{.: {}, site: {size: 9K}}"),
            ("Service Worker/ScriptCache", "{.: {}, js: {size: 9K}}"),
            ("Session Storage", "{.: {}, log: {size: 9K}}"),
            ("WebStorage", "{.: {}, site: {size: 9K}}"),
            ("blob_storage", "{.: {}}"),
            ("Cookies", "{size: 9K}"),
            ("Preferences", "{size: 1K}"),
            // Cache-shaped names outside the named set.
            ("Cache.backup", "{.: {}}"),
            ("DawnGraphiteCache", "{.: {}}"),
            ("CachedData", "{.: {}}"),
            ("Default/Cache", "{.: {}}"),
            ("Partitions/teams-4-linux/Local Storage", "{.: {}}"),
            (
                "Partitions/teams-4-linux/Service Worker/CacheStorage",
                "{.: {}}",
            ),
            ("Partitions/teams-4-linux/IndexedDB", "{.: {}}"),
            ("Partitions/teams-4-linux/Cookies", "{size: 1K}"),
            ("Partitions/nested/deeper/Cache", "{.: {}}"),
            // A regular file at a cache-shaped path (type rule).
            ("Partitions/file-shaped/GPUCache", "{size: 1K}"),
        ] {
            tree.push_str(&format!("  \"{base}/{path}\": {entry}\n"));
        }
        if app.caches.iter().all(|c| !c.starts_with("Partitions/")) {
            // An app without partitions matches none, even one level deep.
            tree.push_str(&format!("  \"{base}/Partitions/x/Cache\": {{.: {{}}}}\n"));
        }
        // The same names below .cache are not the app's user data.
        tree.push_str(&format!(
            "  \"/home/mark/.cache/{}/Cache\": {{.: {{}}}}\n",
            app.config
        ));
    }
    for other in OTHER_APPS {
        tree.push_str(&format!(
            "  \"/home/mark/.config/{other}/Cache\": {{.: {{}}, data: {{size: 4K}}}}\n  \"/home/mark/.config/{other}/Code Cache\": {{.: {{}}}}\n"
        ));
    }
    tree
}

fn lock(config: &str, target: &str) -> String {
    format!("  \"/home/mark/.config/{config}/SingletonLock\": {{symlink: {target}}}\n")
}

const LIVE_42: &str = "processes:\n  - {pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /usr/bin/tool}], environ: [PATH=/usr/bin]}\n";

fn request(profile: &str) -> Request {
    let library = Library::builtin().expect("built-in profiles");
    let roots = ["/home/mark".to_owned()];
    Request {
        path: PathBuf::from("/home/mark"),
        min_free: parse_size("1G").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(&format!("profile:{profile}"), &library, Some(&roots), 1000)
            .unwrap_or_else(|e| panic!("{e}")),
    }
}

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

/// The plan as sorted (candidate, reason name) pairs.
fn plan(report: &Report) -> Vec<(String, Option<&'static str>)> {
    let mut rows: Vec<_> = report
        .plan
        .iter()
        .map(|p| {
            (
                p.candidate
                    .as_deref()
                    .expect("a candidate")
                    .display()
                    .to_string(),
                p.reason.map(Reason::as_str),
            )
        })
        .collect();
    rows.sort();
    rows
}

fn expected(app: &App, reason: Option<Reason>) -> Vec<(String, Option<&'static str>)> {
    let mut rows: Vec<_> = app
        .caches
        .iter()
        .map(|cache| {
            (
                format!("/home/mark/.config/{}/{cache}", app.config),
                reason.map(Reason::as_str),
            )
        })
        .collect();
    rows.sort();
    rows
}

fn nothing_locked_or_deleted(host: &ModelHost) {
    for op in [Op::Flock, Op::Unlinkat, Op::Fchmod, Op::MayDelete] {
        assert!(host.trace().iter().all(|c| c.op != op), "{op} in a dry run");
    }
}

/// AC #1: with the app stopped (no SingletonLock), each profile's plan is
/// exactly its named cache directories, all eligible. Site data (Local
/// Storage, IndexedDB, Service Worker storage, Cookies, WebStorage),
/// profile settings, other cache-shaped names, a file at a cache path,
/// deeper partitions and other Electron apps' caches never match.
#[test]
fn electron_app_caches_match_only_named_cache_dirs_when_the_app_is_stopped() {
    let model = host(&yaml(&tree()));
    for app in &APPS {
        assert_eq!(
            plan(&dry_run(&model, &request(app.profile))),
            expected(app, None),
            "{} stopped match set",
            app.profile
        );
    }
    nothing_locked_or_deleted(&model);
}

/// AC #2: a running app (its SingletonLock present, naming this host and a
/// live pid) makes every cache of that app ineligible, and only that
/// app's: the other app's caches stay eligible.
#[test]
fn a_running_app_keeps_all_its_caches_and_only_its_own() {
    for (running, stopped) in [(&APPS[0], &APPS[1]), (&APPS[1], &APPS[0])] {
        let model = host(&yaml(&format!(
            "{}{}{LIVE_42}",
            tree(),
            lock(running.config, "model-host-42")
        )));
        assert_eq!(
            plan(&dry_run(&model, &request(running.profile))),
            expected(running, Some(Reason::InUse)),
            "{} running",
            running.profile
        );
        assert_eq!(
            plan(&dry_run(&model, &request(stopped.profile))),
            expected(stopped, None),
            "{} is not blocked by {}'s lock",
            stopped.profile,
            running.profile
        );
        nothing_locked_or_deleted(&model);
    }
}

/// A stale lock retains as the browser caches' does: diskgc does not
/// interpret the lock text, so a dead pid, another host's name, a
/// malformed target and a regular file all keep the caches (a pid gone
/// here may be live in another PID namespace sharing this home).
#[test]
fn a_stale_foreign_or_malformed_lock_still_keeps_the_caches() {
    for app in &APPS {
        for target in ["model-host-42", "other-host-42", "garbage"] {
            // No process 42: the pid is dead in this namespace.
            let model = host(&yaml(&format!("{}{}", tree(), lock(app.config, target))));
            assert_eq!(
                plan(&dry_run(&model, &request(app.profile))),
                expected(app, Some(Reason::InUse)),
                "{} lock -> {target}",
                app.profile
            );
            nothing_locked_or_deleted(&model);
        }
        let regular = format!(
            "{}  \"/home/mark/.config/{}/SingletonLock\": {{size: 0}}\n",
            tree(),
            app.config
        );
        let model = host(&yaml(&regular));
        assert_eq!(
            plan(&dry_run(&model, &request(app.profile))),
            expected(app, Some(Reason::InUse)),
            "{} regular-file lock",
            app.profile
        );
    }
}

/// A cache directory that is a symlink is never a candidate (find follows
/// no symlink), and a user-data directory reached through a symlink yields
/// no candidate at all: the link target's data is untouched.
#[test]
fn a_symlinked_cache_or_user_data_dir_is_refused() {
    for app in &APPS {
        let base = format!("/home/mark/.config/{}", app.config);
        let linked_cache = format!(
            "  \"{base}/Cache\": {{symlink: /home/mark/elsewhere/cache}}\n  /home/mark/elsewhere/cache: {{.: {{}}, data: {{size: 4K}}}}\n  \"{base}/GPUCache\": {{.: {{}}}}\n"
        );
        let model = host(&yaml(&linked_cache));
        assert_eq!(
            plan(&dry_run(&model, &request(app.profile))),
            vec![(format!("{base}/GPUCache"), None)],
            "{}: a symlinked Cache is not a candidate",
            app.profile
        );
        nothing_locked_or_deleted(&model);

        let linked_dir = format!(
            "  \"{base}\": {{symlink: /home/mark/dotfiles/{0}}}\n  \"/home/mark/dotfiles/{0}/Cache\": {{.: {{}}, data: {{size: 4K}}}}\n",
            app.config
        );
        let model = host(&yaml(&linked_dir));
        assert_eq!(
            plan(&dry_run(&model, &request(app.profile))),
            vec![],
            "{}: a symlinked user-data directory yields no candidate",
            app.profile
        );
        assert!(
            model
                .stat_path(Path::new(&format!(
                    "/home/mark/dotfiles/{}/Cache",
                    app.config
                )))
                .is_some()
        );
    }
}

/// A regular file at a cache path the profile names is not a candidate
/// (the sources match directories only); the other caches still are.
#[test]
fn a_regular_file_at_a_cache_path_is_not_a_candidate() {
    for app in &APPS {
        let base = format!("/home/mark/.config/{}", app.config);
        let mut tree = format!("  \"{base}/GPUCache\": {{size: 1K}}\n");
        for cache in app.caches.iter().filter(|c| **c != "GPUCache") {
            tree.push_str(&format!("  \"{base}/{cache}\": {{.: {{}}}}\n"));
        }
        let model = host(&yaml(&tree));
        let want: Vec<_> = expected(app, None)
            .into_iter()
            .filter(|(path, _)| *path != format!("{base}/GPUCache"))
            .collect();
        assert_eq!(
            plan(&dry_run(&model, &request(app.profile))),
            want,
            "{}: a file named GPUCache",
            app.profile
        );
        nothing_locked_or_deleted(&model);
    }
}
