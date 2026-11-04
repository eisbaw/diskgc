//! What diskgc says about its profiles (docs/reference.md, `profiles`
//! command): the Profiles section of `diskgc --help`, and `diskgc profiles
//! list` / `show NAME` in human text and JSON.
//!
//! Every text here is rendered from a [`Library`] at run time, never
//! written by hand, so a new built-in appears in `--help`, the listing and
//! the JSON without editing any of them. All pure: the binary picks the
//! library (built-ins, plus a config's user profiles) and prints the
//! result.
//!
//! One JSON object describes a profile ([`profile_json`]); `list` and
//! `show` carry the same object, so its fields have one definition. Values
//! that are YAML identifiers keep their YAML spelling (kind names,
//! `delete`, `command`, `dir`); values only the JSON has are snake_case,
//! like the report's enum values (`status`, skip `reason`): `built_in`,
//! `cargo_lock_protocol`. (The report's `plan[].operation` is free text,
//! `cargo lock protocol`, not an enum.) The list table uses the same
//! spelling, so it can be grepped for them. `show`'s
//! human form is the profile as YAML in the schema of docs/reference.md
//! (Profiles), every default spelled out, so it loads back to the same
//! profile (a test proves it on every built-in and on the test fixtures).

use std::path::Path;

use super::{Action, Body, Finds, Library, Origin, Profile, Roots, Source, Where};
use crate::report::json::Json;
use crate::suggest::{MAX_NAME_CHARS, edit_distance};
use crate::units::format_duration;

/// The closing lines of `--help`'s Profiles section: where the rest is.
/// A release catalog runs no user profiles, so its help does not offer
/// them.
fn more(library: &Library) -> String {
    let first = match library.release_name() {
        Some(release) => format!(
            "diskgc {release} runs these built-ins only, no user profiles: \
             `diskgc profiles list`;"
        ),
        None => "User profiles from your config are listed by `diskgc profiles list`;".to_owned(),
    };
    format!("{first}\ndetails: `diskgc profiles show NAME`; machine-readable: `--json`.")
}

/// Every profile in the order every listing uses: sources by name, then
/// filters by their `idle_for` (shortest first, those without one last)
/// and name, so `idle-1h` comes before `idle-1d`.
pub fn listing_order(library: &Library) -> Vec<&Profile> {
    let mut profiles: Vec<&Profile> = library.profiles().collect();
    profiles.sort_by_key(|p| match &p.body {
        Body::Source(_) => (0, false, None, p.name.as_str()),
        Body::Filter(w) => (1, w.idle_for.is_none(), w.idle_for, p.name.as_str()),
    });
    profiles
}

/// A free text on one line: every run of whitespace one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rows as columns two spaces apart, each padded to its widest cell but
/// the last, which runs free (a description).
fn aligned(rows: &[Vec<String>]) -> String {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == columns {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&" ".repeat(widths[c] - cell.chars().count() + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---- `diskgc --help`

/// The Profiles section that ends `diskgc --help`: one line per built-in
/// source (name, category, description, and `(roots required)` where it
/// applies), the built-in filters on one line, and where to learn more
/// ([`more`]).
pub fn help_section(library: &Library) -> String {
    let builtin: Vec<&Profile> = listing_order(library)
        .into_iter()
        .filter(|p| p.origin == Origin::BuiltIn)
        .collect();
    let rows: Vec<Vec<String>> = builtin
        .iter()
        .filter_map(|p| match &p.body {
            Body::Source(source) => {
                let mut description = one_line(p.description.as_deref().unwrap_or(""));
                if source.roots == Roots::Required {
                    description.push_str(" (roots required: --roots DIR)");
                }
                Some(vec![
                    format!("  {}", p.name),
                    p.category.clone().unwrap_or_default(),
                    description,
                ])
            }
            Body::Filter(_) => None,
        })
        .collect();
    let filters: Vec<&str> = builtin
        .iter()
        .filter(|p| matches!(p.body, Body::Filter(_)))
        .map(|p| p.name.as_str())
        .collect();
    format!(
        "\nProfiles built in (select with --select 'profile:NAME && filter:NAME'):\n{}  \
         filters: {}\n{}\n",
        aligned(&rows),
        filters.join(", "),
        more(library)
    )
}

// ---- `diskgc profiles list`

/// The action's `type` in the JSON: `delete` and `command` as the YAML
/// spells them, `cargo_lock_protocol` for the kind's fixed protocol.
fn action_type(action: &Action) -> &'static str {
    match action {
        Action::Delete { .. } => "delete",
        Action::Command { .. } => "command",
        Action::CargoLockProtocol => "cargo_lock_protocol",
        Action::DockerBuilderPrune => "docker_builder_prune",
        Action::DockerImagePrune { all: false } => "docker_image_prune",
        Action::DockerImagePrune { all: true } => "docker_image_prune_all",
        Action::PodmanImagePrune { all: false } => "podman_image_prune",
        Action::PodmanImagePrune { all: true } => "podman_image_prune_all",
        Action::NixStoreGc => "nix_store_gc",
    }
}

/// The program a command action runs: its first word, or, for `env`, the
/// first word after `env`'s options and `NAME=value` assignments (`[env,
/// "GOCACHE={path}", go, clean, -cache]` runs `go`).
/// Limit: an option that takes a value (`env -u NAME`, `-C DIR`) is
/// skipped without it, so its value is named; `show` has the argv.
fn program(argv: &[String]) -> Option<&str> {
    let mut words = argv.iter().map(String::as_str);
    let first = words.next()?;
    if first.rsplit('/').next() != Some("env") {
        return Some(first);
    }
    words
        .find(|w| !w.starts_with('-') && !w.contains('='))
        .or(Some(first))
}

/// The short form of an action for the listing's column, in the JSON's
/// spelling: `delete`, `delete make_writable`, `command PROGRAM`,
/// `cargo_lock_protocol`; `show` has the whole action.
fn action_summary(action: &Action) -> String {
    match action {
        Action::Delete {
            make_writable: true,
        } => "delete make_writable".to_owned(),
        Action::Command { argv, .. } => match program(argv) {
            Some(program) => format!("command {program}"),
            None => "command".to_owned(),
        },
        other => action_type(other).to_owned(),
    }
}

/// `profiles list` for a human: one aligned table, one profile per row,
/// `-` where a column does not apply (a filter has no kind, action, roots
/// or checkouts).
pub fn list_text(library: &Library) -> String {
    let dash = || "-".to_owned();
    let mut rows = vec![
        [
            "NAME",
            "ORIGIN",
            "ROLE",
            "CATEGORY",
            "KIND",
            "ACTION",
            "ROOTS",
            "CHECKOUTS",
            "DESCRIPTION",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];
    for p in listing_order(library) {
        let (kind, action, roots, checkouts) = match &p.body {
            Body::Source(s) => (
                s.kind().name().to_owned(),
                action_summary(&s.action),
                match &s.roots {
                    Roots::Fixed(list) => list.join(","),
                    Roots::Required => "required".to_owned(),
                    Roots::Backend => "daemon".to_owned(),
                },
                if matches!(s.finds, Finds::NixStore) {
                    dash()
                } else {
                    s.checkouts.as_str().to_owned()
                },
            ),
            Body::Filter(_) => (dash(), dash(), dash(), dash()),
        };
        rows.push(vec![
            p.name.clone(),
            p.origin.name().to_owned(),
            p.role().to_owned(),
            p.category.clone().unwrap_or_else(dash),
            kind,
            action,
            roots,
            checkouts,
            p.description.as_deref().map_or_else(dash, one_line),
        ]);
    }
    aligned(&rows)
}

// ---- JSON

fn opt_str(value: Option<&str>) -> Json {
    Json::opt(value, Json::str)
}

fn strings(list: &[String]) -> Json {
    Json::Arr(list.iter().map(Json::str).collect())
}

fn where_json(where_: &Where) -> Json {
    Json::Obj(vec![
        (
            "idle_for",
            Json::opt(where_.idle_for, |d| Json::str(format_duration(d.0))),
        ),
        ("has", opt_str(where_.has.as_deref())),
        ("has_dir", opt_str(where_.has_dir.as_deref())),
        ("has_not", opt_str(where_.has_not.as_deref())),
        (
            "app_not_running",
            opt_str(where_.app_not_running.as_deref()),
        ),
        ("lacks_entry", opt_str(where_.lacks_entry.as_deref())),
        (
            "source_anchor",
            Json::opt(where_.source_anchor, |anchor| Json::str(anchor.name())),
        ),
        (
            "liveness_contract",
            Json::opt(where_.liveness_contract, |contract| {
                Json::str(contract.name())
            }),
        ),
        (
            "keep_newest",
            Json::opt(where_.keep_newest, |n| Json::U64(n as u64)),
        ),
        (
            "newest_group",
            Json::opt(where_.newest_group, |g| Json::str(g.name())),
        ),
        ("protect_links", strings(&where_.protect_links)),
        ("protect_link_dirs", strings(&where_.protect_link_dirs)),
        (
            "protect_parent_link",
            opt_str(where_.protect_parent_link.as_deref()),
        ),
        (
            "protect_cursor_extensions",
            opt_str(where_.protect_cursor_extensions.as_deref()),
        ),
        (
            "protect_toolchains",
            Json::opt(where_.protect_toolchains, |manager| {
                Json::str(manager.name())
            }),
        ),
    ])
}

/// An action, every field present (`null` where it does not apply), so a
/// reader branches on `type` alone.
fn action_json(action: &Action) -> Json {
    let (make_writable, command, cwd) = match action {
        Action::Delete { make_writable } => (Json::Bool(*make_writable), None, None),
        Action::Command { argv, cwd } => (Json::Null, Some(argv), *cwd),
        Action::CargoLockProtocol | Action::NixStoreGc => (Json::Null, None, None),
        Action::DockerBuilderPrune
        | Action::DockerImagePrune { .. }
        | Action::PodmanImagePrune { .. } => (Json::Null, None, None),
    };
    Json::Obj(vec![
        ("type", Json::str(action_type(action))),
        ("make_writable", make_writable),
        ("command", Json::opt(command, |argv| strings(argv))),
        ("cwd", Json::opt(cwd, |cwd| Json::str(cwd.name()))),
    ])
}

fn find_json(source: &Source) -> Json {
    let (paths, exclude, entry_type) = match &source.finds {
        Finds::Paths {
            patterns,
            exclude,
            entry_type,
        } => (
            strings(patterns),
            strings(exclude),
            Json::str(entry_type.name()),
        ),
        Finds::CargoTargets => (Json::Null, Json::Null, Json::Null),
        Finds::DockerBuildCache | Finds::DockerImages | Finds::PodmanImages | Finds::NixStore => {
            (Json::Null, Json::Null, Json::Null)
        }
    };
    // `roots` is always a list (TASK-192): one type for a script to read;
    // where the roots come from otherwise is its own field, spelled as in
    // YAML (`roots: required`, `roots: daemon`).
    let none: &[String] = &[];
    let (roots, required, backend) = match &source.roots {
        Roots::Fixed(list) => (list.as_slice(), false, None),
        Roots::Required => (none, true, None),
        Roots::Backend => (none, false, Some("daemon")),
    };
    Json::Obj(vec![
        ("roots", strings(roots)),
        ("roots_required", Json::Bool(required)),
        ("backend", Json::opt(backend, Json::str)),
        ("paths", paths),
        ("exclude", exclude),
        ("type", entry_type),
    ])
}

/// One profile as JSON (docs/reference.md, `profiles` command): every
/// field of the schema, always present, `null` where the profile's role or
/// kind has none.
pub fn profile_json(p: &Profile) -> Json {
    let source = match &p.body {
        Body::Source(source) => Some(source.as_ref()),
        Body::Filter(_) => None,
    };
    let where_ = match &p.body {
        Body::Source(source) => &source.where_,
        Body::Filter(where_) => where_,
    };
    // Kind nix-store takes no `checkouts` (docs/reference.md, `profiles`
    // command): it has no file candidates.
    let nix_store = source.is_some_and(|s| matches!(s.finds, Finds::NixStore));
    Json::Obj(vec![
        ("name", Json::str(&p.name)),
        ("origin", Json::str(p.origin.name())),
        ("override", Json::Bool(p.origin == Origin::UserOverride)),
        ("role", Json::str(p.role())),
        ("category", opt_str(p.category.as_deref())),
        ("description", opt_str(p.description.as_deref())),
        ("kind", Json::opt(source, |s| Json::str(s.kind().name()))),
        ("find", Json::opt(source, find_json)),
        ("where", where_json(where_)),
        (
            "min_idle_for",
            Json::opt(source.and_then(|s| s.min_idle_for), |d| {
                Json::str(format_duration(d.0))
            }),
        ),
        ("action", Json::opt(source, |s| action_json(&s.action))),
        (
            "checkouts",
            Json::opt(source.filter(|_| !nix_store), |s| {
                Json::str(s.checkouts.as_str())
            }),
        ),
        (
            "examples",
            Json::opt(
                source.filter(|s| matches!(s.finds, Finds::Paths { .. })),
                |s| {
                    Json::Obj(vec![
                        ("root", opt_str(s.examples.root.as_deref())),
                        ("match", strings(&s.examples.matches)),
                        ("no_match", strings(&s.examples.no_match)),
                    ])
                },
            ),
        ),
        (
            "verified_with",
            opt_str(source.and_then(|s| s.verified_with.as_deref())),
        ),
        (
            "liveness_class",
            opt_str(source.and_then(|s| s.liveness_class).map(|c| c.name())),
        ),
        (
            "releases",
            Json::Arr(p.releases.iter().map(|r| Json::str(r.name())).collect()),
        ),
    ])
}

fn config_json(config: Option<&Path>) -> Json {
    Json::opt(config, |path| Json::str(path.to_string_lossy()))
}

/// `profiles list --json`: `{"config": ..., "profiles": [...]}`, in
/// [`listing_order`].
pub fn list_json(library: &Library, config: Option<&Path>) -> Json {
    Json::Obj(vec![
        ("config", config_json(config)),
        (
            "profiles",
            Json::Arr(
                listing_order(library)
                    .into_iter()
                    .map(profile_json)
                    .collect(),
            ),
        ),
    ])
}

/// `profiles show NAME --json`: `{"config": ..., "profile": {...}}`.
pub fn show_json(profile: &Profile, config: Option<&Path>) -> Json {
    Json::Obj(vec![
        ("config", config_json(config)),
        ("profile", profile_json(profile)),
    ])
}

// ---- `diskgc profiles show NAME`

/// `text` as a YAML scalar: plain where that reads back as the same string
/// (a name, a path, a version), else double-quoted with JSON's escapes,
/// which YAML's double-quoted style reads the same way.
fn yaml_scalar(text: &str) -> String {
    let plain = text
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/')
        && !text.ends_with(' ')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-' | '+' | ' '))
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain {
        text.to_owned()
    } else {
        Json::str(text).to_text()
    }
}

fn yaml_list(list: &[String]) -> String {
    let items: Vec<String> = list.iter().map(|s| yaml_scalar(s)).collect();
    format!("[{}]", items.join(", "))
}

fn yaml_where(where_: &Where) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(idle) = where_.idle_for {
        parts.push(format!("idle_for: {}", format_duration(idle.0)));
    }
    if let Some(name) = &where_.has {
        parts.push(format!("has: {}", yaml_scalar(name)));
    }
    if let Some(name) = &where_.has_dir {
        parts.push(format!("has_dir: {}", yaml_scalar(name)));
    }
    if let Some(name) = &where_.has_not {
        parts.push(format!("has_not: {}", yaml_scalar(name)));
    }
    if let Some(path) = &where_.app_not_running {
        parts.push(format!("app_not_running: {}", yaml_scalar(path)));
    }
    if let Some(name) = &where_.lacks_entry {
        parts.push(format!("lacks_entry: {}", yaml_scalar(name)));
    }
    if let Some(anchor) = where_.source_anchor {
        parts.push(format!("source_anchor: {}", anchor.name()));
    }
    if let Some(contract) = where_.liveness_contract {
        parts.push(format!("liveness_contract: {}", contract.name()));
    }
    if let Some(count) = where_.keep_newest {
        parts.push(format!("keep_newest: {count}"));
    }
    if let Some(group) = where_.newest_group {
        parts.push(format!("newest_group: {}", group.name()));
    }
    if !where_.protect_links.is_empty() {
        parts.push(format!(
            "protect_links: {}",
            yaml_list(&where_.protect_links)
        ));
    }
    if !where_.protect_link_dirs.is_empty() {
        parts.push(format!(
            "protect_link_dirs: {}",
            yaml_list(&where_.protect_link_dirs)
        ));
    }
    if let Some(link) = &where_.protect_parent_link {
        parts.push(format!("protect_parent_link: {}", yaml_scalar(link)));
    }
    if let Some(manifest) = &where_.protect_cursor_extensions {
        parts.push(format!(
            "protect_cursor_extensions: {}",
            yaml_scalar(manifest)
        ));
    }
    if let Some(manager) = where_.protect_toolchains {
        parts.push(format!("protect_toolchains: {}", manager.name()));
    }
    (!parts.is_empty()).then(|| format!("{{{}}}", parts.join(", ")))
}

/// The `action:` line, or, for a kind that fixes its action, a comment
/// saying so (the schema refuses `action` there).
fn yaml_action(action: &Action) -> String {
    match action {
        Action::Delete {
            make_writable: false,
        } => "  action: delete\n".to_owned(),
        Action::Delete {
            make_writable: true,
        } => "  action: {delete: {make_writable: true}}\n".to_owned(),
        Action::Command { argv, cwd } => {
            let cwd = cwd.map_or_else(String::new, |cwd| format!(", cwd: {}", cwd.name()));
            format!("  action: {{command: {}{cwd}}}\n", yaml_list(argv))
        }
        Action::CargoLockProtocol => {
            "  # action: the Cargo lock protocol, fixed by kind cargo-target\n".to_owned()
        }
        Action::DockerBuilderPrune => "  action: docker-builder-prune\n".to_owned(),
        Action::DockerImagePrune { all: false } => "  action: docker-image-prune\n".to_owned(),
        Action::DockerImagePrune { all: true } => "  action: docker-image-prune-all\n".to_owned(),
        Action::PodmanImagePrune { all: false } => "  action: podman-image-prune\n".to_owned(),
        Action::PodmanImagePrune { all: true } => "  action: podman-image-prune-all\n".to_owned(),
        Action::NixStoreGc => {
            "  # action: one bounded GC through the bound Nix daemon, fixed by kind nix-store\n"
                .to_owned()
        }
    }
}

/// `profiles show NAME` for a human: a comment saying where the profile
/// comes from, then the profile as a YAML list item in the schema of
/// docs/reference.md (Profiles), as diskgc understood it: `kind` and
/// `checkouts` spelled out where they default, a command's `cwd` as
/// resolved. Pasted under a config's `profiles:`, it loads as the same
/// profile (a built-in's name needs `override: true` there, task-17).
pub fn show_yaml(profile: &Profile, config: Option<&Path>) -> String {
    let from = match (profile.origin, config) {
        (Origin::BuiltIn, _) => "built in".to_owned(),
        (Origin::User, Some(path)) => format!("from {}", path.display()),
        (Origin::User, None) => "a user profile".to_owned(),
        (Origin::UserOverride, Some(path)) => format!("override from {}", path.display()),
        (Origin::UserOverride, None) => "a user override".to_owned(),
    };
    let mut out = format!(
        "# {}: {} profile, {from}\n- name: {}\n",
        profile.name,
        profile.role(),
        profile.name
    );
    if profile.origin == Origin::UserOverride {
        out.push_str("  override: true\n");
    }
    for (field, value) in [
        ("category", &profile.category),
        ("description", &profile.description),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("  {field}: {}\n", yaml_scalar(value)));
        }
    }
    match &profile.body {
        Body::Filter(where_) => {
            if let Some(where_) = yaml_where(where_) {
                out.push_str(&format!("  where: {where_}\n"));
            }
        }
        Body::Source(source) => {
            out.push_str(&format!("  kind: {}\n", source.kind().name()));
            out.push_str("  find:\n");
            out.push_str(&format!(
                "    roots: {}\n",
                match &source.roots {
                    Roots::Fixed(list) => yaml_list(list),
                    Roots::Required => "required".to_owned(),
                    Roots::Backend => "daemon".to_owned(),
                }
            ));
            if let Finds::Paths {
                patterns,
                exclude,
                entry_type,
            } = &source.finds
            {
                out.push_str(&format!("    paths: {}\n", yaml_list(patterns)));
                if !exclude.is_empty() {
                    out.push_str(&format!("    exclude: {}\n", yaml_list(exclude)));
                }
                out.push_str(&format!("    type: {}\n", entry_type.name()));
            }
            if let Some(where_) = yaml_where(&source.where_) {
                out.push_str(&format!("  where: {where_}\n"));
            }
            if let Some(minimum) = source.min_idle_for {
                out.push_str(&format!("  min_idle_for: {}\n", format_duration(minimum.0)));
            }
            out.push_str(&yaml_action(&source.action));
            // Kind nix-store takes no `checkouts` (it has no file
            // candidates); every other source spells out its setting.
            if !matches!(source.finds, Finds::NixStore) {
                out.push_str(&format!("  checkouts: {}\n", source.checkouts.as_str()));
            }
            if matches!(source.finds, Finds::Paths { .. }) {
                out.push_str("  examples:\n");
                if let Some(root) = &source.examples.root {
                    out.push_str(&format!("    root: {}\n", yaml_scalar(root)));
                }
                out.push_str(&format!(
                    "    match: {}\n",
                    yaml_list(&source.examples.matches)
                ));
                out.push_str(&format!(
                    "    no_match: {}\n",
                    yaml_list(&source.examples.no_match)
                ));
            }
            if let Some(verified) = &source.verified_with {
                out.push_str(&format!("  verified_with: {}\n", yaml_scalar(verified)));
            }
            if let Some(class) = source.liveness_class {
                out.push_str(&format!("  liveness_class: {}\n", class.name()));
            }
        }
    }
    if !profile.releases.is_empty() {
        let names: Vec<String> = profile
            .releases
            .iter()
            .map(|r| format!("\"{}\"", r.name()))
            .collect();
        out.push_str(&format!("  releases: [{}]\n", names.join(", ")));
    }
    out
}

// ---- An unknown name

/// The most names [`close_names`] suggests.
const MAX_CLOSE: usize = 5;

/// The names in `library` close to `name`: those that contain it or that
/// it contains (a qualified `profile:cargo-target`, a bare `cargo`), then
/// those the fewest edits away, at most two (`idle-5d`: `idle-1d`,
/// `idle-2d`, ..., not `idle-10d`). At most [`MAX_CLOSE`], by name within
/// each group.
pub fn close_names<'a>(library: &'a Library, name: &str) -> Vec<&'a str> {
    let name_len = name.chars().count();
    if name_len > MAX_NAME_CHARS {
        return Vec::new();
    }
    let names = || library.profiles().map(|p| p.name.as_str());
    let contained =
        names().filter(|known| name.len() >= 3 && (known.contains(name) || name.contains(known)));
    let nearby = || names().filter(|known| name_len.abs_diff(known.chars().count()) <= 2);
    let nearest = nearby().map(|known| edit_distance(name, known)).min();
    let edited = nearby().filter(|known| {
        let distance = edit_distance(name, known);
        distance <= 2 && Some(distance) == nearest
    });
    let mut close: Vec<&str> = Vec::new();
    for known in contained.chain(edited) {
        if !close.contains(&known) {
            close.push(known);
        }
    }
    close.truncate(MAX_CLOSE);
    close
}

/// Why `show NAME` found nothing, naming the close names if any.
pub fn unknown_name(library: &Library, name: &str) -> String {
    let close = close_names(library, name);
    let escaped = name.escape_debug();
    if close.is_empty() {
        format!("no profile is named `{escaped}`; `diskgc profiles list` lists them")
    } else {
        let quoted: Vec<String> = close.iter().map(|n| format!("`{n}`")).collect();
        format!(
            "no profile is named `{escaped}`; close names: {}",
            quoted.join(", ")
        )
    }
}

#[cfg(test)]
mod tests;
