//! A tier as the executor runs it: its selection, its sources (`fs`
//! profiles, a union of them, Cargo, Docker), the `!profile:` and `protect`
//! sources it is judged against, and its roots (docs/reference.md,
//! Selection grammar; docs/architecture.md, The reclaim walk).

use crate::config::{self, Order};
use crate::engine::Checkouts;
use crate::fs_kind::{EntryFilter, Find, NegatedMixedFilter};
use crate::profile::{Action, Body, Finds, IdleFor, IdleWindow, Library, LivenessContract, Where};
use crate::select::{self, Factor};
use std::path::PathBuf;

/// The name of the one group and tier `--select` makes, in the JSON and on
/// stderr alike (docs/reference.md, Commands). A word, not the flag: a
/// progress line `select: skip in_use ...` does not read as a complaint
/// about the command line (TASK-192). A configured group may also be called
/// `select`; a report never holds both, since `--select` replaces the
/// configuration's groups.
pub const SELECT_TIER: &str = "select";

/// One selected tier. Its ordinary fields hold the first source term;
/// [`Tier::fs_union`] holds any additional positive filesystem
/// terms. A same-profile Cargo union of positive idle filters is reduced
/// to its shortest bound. Built by [`Tier::select`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub group: String,
    pub name: String,
    /// Candidate order within this tier. An explicit `--select` uses oldest.
    pub order: Order,
    /// The expression, normalised (as `check` prints it).
    pub select: String,
    /// The source profile.
    pub profile: String,
    /// What the source finds, by kind.
    pub source: SourceKind,
    /// Where it looks: the bound roots, `{uid}` replaced.
    pub roots: Vec<PathBuf>,
    /// The term's idle requirement: the longest positive `filter:` bound
    /// and the source's mandatory `min_idle_for` floor, if present.
    pub idle_for: Option<IdleFor>,
    /// Every negated idle filter requires a last use strictly newer than
    /// this shortest bound. Checked again in the kind's reclaim protocol.
    pub idle_before: Option<IdleFor>,
    /// The source profile's `checkouts` setting: whether a candidate may
    /// hold a VCS checkout (docs/safety.md, No source checkout).
    pub checkouts: Checkouts,
    /// The source profile's action (for the code-backed kinds, the kind's).
    pub action: Action,
    /// Additional positive filesystem terms of a union. The first term is
    /// held in the tier's ordinary fields, so there is one source of truth.
    /// Empty for a single-source tier.
    pub fs_union: Vec<FsUnionSource>,
    /// Bound protected source matches, shared by every tier of a policy.
    pub protections: Vec<ProtectedSource>,
    /// Raw source matches excluded from this term by overlap.
    pub negated: Vec<NegatedSource>,
    /// The eligibility walk's entry budget per candidate (`too_many_entries`
    /// beyond it): always [`crate::engine::DEFAULT_MAX_ENTRIES`] in the
    /// binary. Only the model tests lower it, through
    /// [`crate::invocation::EnsureOptions::test_entry_budget`], so a vector
    /// can show the refusal with a small tree.
    pub max_entries: usize,
}

/// A protected source's raw find, with its own `where` deliberately omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedSource {
    pub profile: String,
    pub roots: Vec<PathBuf>,
    pub kind: ProtectedKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectedKind {
    Fs(Box<Find>),
    CargoTarget,
}

/// Bind all protection roots before planning or reclaim. Root mode must not
/// silently expand a protected `{uid}` to uid 0.
pub fn bind_protections(
    config: &config::Config,
    uid: u32,
    allow_root: bool,
) -> Result<Vec<ProtectedSource>, String> {
    config.protect.iter().map(|entry| {
        let Body::Source(source) = &config.library.get(&entry.profile)
            .ok_or_else(|| format!("protect: unknown profile `{}`", entry.profile))?.body else {
            return Err(format!("protect: `{}` is not a source", entry.profile));
        };
        if allow_root {
            let uses_uid = entry.roots.iter().any(|root| root.contains("{uid}"))
                || matches!(&source.finds, Finds::Paths { patterns, exclude, .. }
                    if patterns.iter().chain(exclude).any(|path| path.contains("{uid}")));
            if uses_uid {
                return Err(format!("--allow-root refuses `{{uid}}` in protected profile `{}`: it would expand to root (uid 0), not to an owners: entry", entry.profile));
            }
        }
        let kind = match &source.finds {
            Finds::Paths { patterns, exclude, entry_type } => ProtectedKind::Fs(Box::new(
                Find::with_excludes(patterns, exclude, *entry_type, &Where::default(), uid),
            )),
            Finds::CargoTargets => ProtectedKind::CargoTarget,
            _ => return Err(format!("protect: `{}` has an unsupported kind", entry.profile)),
        };
        Ok(ProtectedSource {
            profile: entry.profile.clone(),
            roots: entry.roots.iter().map(|root| PathBuf::from(root.replace("{uid}", &uid.to_string()))).collect(),
            kind,
        })
    }).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegatedSource {
    pub profile: String,
    pub source: NegatedKind,
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegatedKind {
    Fs(Box<Find>),
    CargoTarget,
}

/// One source contribution to a filesystem union. The same inode may be
/// found by several entries and is judged once with all its contributors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsUnionSource {
    pub profile: String,
    pub find: Find,
    pub roots: Vec<PathBuf>,
    pub idle: IdleWindow,
    pub checkouts: Checkouts,
    pub negated: Vec<NegatedSource>,
}

/// A borrowed view of either the tier's first source or an additional
/// union term. Discovery indices refer to this ordered list.
#[derive(Clone, Copy)]
pub(super) struct FsUnionSourceRef<'a> {
    pub(super) profile: &'a str,
    pub(super) find: &'a Find,
    pub(super) roots: &'a [PathBuf],
    pub(super) idle: IdleWindow,
    pub(super) checkouts: Checkouts,
    pub(super) negated: &'a [NegatedSource],
}

impl Tier {
    /// The hash of the source profile's definition (kind, `find` with its
    /// `where`, action) that the index's finders carry
    /// (docs/architecture.md, The index: Finders): a unit enters this
    /// tier's queue only when its finder's hash is this one. FNV-1a over
    /// the definition's `Debug` form: stable within one build, which is all
    /// it needs (a snapshot of another diskgc version is discarded whole).
    pub fn profile_hash(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0100_0000_01b3;
        let definition = format!("{:?}\0{:?}", self.source, self.action);
        definition.bytes().fold(OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
    }

    /// The action, as the report names it (`operation`, human-readable).
    pub fn operation(&self) -> String {
        self.action.to_string()
    }

    /// Whether this tier relies on diskgc's own in-use check: the `fs`
    /// kind (its `delete` and its command actions, rechecked against a
    /// scan before they start) and `cargo-target`. The Docker and Podman
    /// kinds leave liveness to their backend, and are not refused where the scan cannot
    /// see every process (docs/safety.md, In use: Visibility).
    pub fn relies_on_in_use_check(&self) -> bool {
        matches!(self.source, SourceKind::Fs(_) | SourceKind::CargoTarget)
    }

    /// Whether a source of this tier (its own or a union term) has a
    /// liveness contract: its in-use scans then also read what Owner PID
    /// needs (one more `status` read per process, docs/safety.md, Cost).
    pub fn pid_lookup(&self) -> bool {
        let own = matches!(&self.source, SourceKind::Fs(find) if find.liveness_contract.is_some());
        own || self
            .fs_union
            .iter()
            .any(|term| term.find.liveness_contract.is_some())
    }

    /// Whether a source of this tier (its own or a union term) has
    /// `liveness_contract: pytest`: its in-use scans then also read that
    /// contract's process facts (docs/safety.md, Producer liveness, Cost).
    pub fn pytest_facts(&self) -> bool {
        let pytest = |find: &Find| find.liveness_contract == Some(LivenessContract::Pytest);
        let own = matches!(&self.source, SourceKind::Fs(find) if pytest(find));
        own || self.fs_union.iter().any(|term| pytest(&term.find))
    }

    /// Whether a source of this tier (its own or a union term) has a
    /// project contract (`node-project`, `python-project`): its in-use
    /// scans then also keep what processes hold and climb their
    /// directories (docs/safety.md, Producer liveness, Cost).
    pub fn project_facts(&self) -> bool {
        let project = |find: &Find| {
            find.liveness_contract
                .is_some_and(LivenessContract::is_project)
        };
        let own = matches!(&self.source, SourceKind::Fs(find) if project(find));
        own || self.fs_union.iter().any(|term| project(&term.find))
    }

    pub(super) fn fs_sources(&self) -> Vec<FsUnionSourceRef<'_>> {
        let SourceKind::Fs(find) = &self.source else {
            unreachable!("a filesystem union retains a filesystem first source")
        };
        let mut sources = vec![FsUnionSourceRef {
            profile: &self.profile,
            find,
            roots: &self.roots,
            idle: IdleWindow {
                at_least: self.idle_for,
                before: self.idle_before,
            },
            checkouts: self.checkouts,
            negated: &self.negated,
        }];
        sources.extend(self.fs_union.iter().map(|source| FsUnionSourceRef {
            profile: &source.profile,
            find: &source.find,
            roots: &source.roots,
            idle: source.idle,
            checkouts: source.checkouts,
            negated: &source.negated,
        }));
        // The nested-build guard is the selection's (`from_checked` sets
        // it on every term): a term without it would take what it keeps.
        assert!(
            sources
                .iter()
                .all(|source| source.find.nested_build_guard == find.nested_build_guard),
            "the nested-build guard differs between the terms of `{}`",
            self.select
        );
        sources
    }
}

/// What a tier's source profile finds, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Cargo target directories, by their structure.
    CargoTarget,
    /// Entries matching its `find` (kind `fs`), deleted by the kind's own
    /// protocol ([`fs_kind::reclaim`]) when its action is `delete`.
    Fs(Box<Find>),
    DockerBuildCache,
    DockerImages,
    /// The bound local Podman store's images (kind `podman-images`).
    PodmanImages,
    /// The bound Nix daemon's dead store paths (kind `nix-store`).
    NixStore,
}

/// `--select EXPR` checked against a library and its roots bound
/// ([`Tier::compile_selection`]), before the user it runs for is known.
#[derive(Debug, Clone)]
pub struct CompiledSelection {
    expr: String,
    checked: select::Checked,
    bound: std::collections::BTreeMap<String, Vec<String>>,
}

impl CompiledSelection {
    /// The parser's nonfatal redundancy warnings.
    pub fn warnings(&self) -> &[String] {
        self.checked.warnings()
    }
}

/// An explicit caller selection failed before it became a runnable tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionFailure {
    Parse(String),
    Usage(String),
}

impl SelectionFailure {
    pub fn into_message(self) -> String {
        match self {
            Self::Parse(message) | Self::Usage(message) => message,
        }
    }
}

impl Tier {
    /// The tier `--select EXPR` makes (with `--roots`, `roots`), checked
    /// against `library` (the built-ins plus the config's profiles), for
    /// the user `uid` (the `{uid}` placeholder). Every error is a usage
    /// error (exit 2) and says what is wrong; what the grammar allows but
    /// this build cannot run yet is refused rather than approximated.
    pub fn select(
        expr: &str,
        library: &Library,
        roots: Option<&[String]>,
        uid: u32,
    ) -> Result<Tier, String> {
        Self::select_with_warnings(expr, library, roots, uid)
            .map(|(tier, _)| tier)
            .map_err(SelectionFailure::into_message)
    }

    /// Like `select`, returning the parser's nonfatal redundancy warnings.
    pub fn select_with_warnings(
        expr: &str,
        library: &Library,
        roots: Option<&[String]>,
        uid: u32,
    ) -> Result<(Tier, Vec<String>), SelectionFailure> {
        let compiled = Self::compile_selection(expr, library, roots)?;
        let tier = Self::from_selection(&compiled, library, uid)?;
        Ok((tier, compiled.checked.warnings().to_vec()))
    }

    /// The part of [`Self::select_with_warnings`] that does not depend on
    /// the user: `expr` parsed and checked against `library`, and its
    /// roots bound (with `--roots`, `roots`). `ensure` and `watch` do it
    /// before they read anything on the machine
    /// ([`crate::invocation::load`]).
    pub fn compile_selection(
        expr: &str,
        library: &Library,
        roots: Option<&[String]>,
    ) -> Result<CompiledSelection, SelectionFailure> {
        let checked = select::compile(expr, library)
            .map_err(|error| SelectionFailure::Parse(error.to_string()))?;
        let bound = library.bind_roots(&checked, roots).map_err(|error| {
            let hint = if matches!(&error, crate::profile::RootsError::Missing { .. }) {
                "; add --roots DIR"
            } else {
                ""
            };
            SelectionFailure::Usage(format!("--select {expr:?}: {error}{hint}"))
        })?;
        Ok(CompiledSelection {
            expr: expr.to_owned(),
            checked,
            bound,
        })
    }

    /// The tier of a [`CompiledSelection`] for the user `uid`, against the
    /// `library` it was compiled with.
    pub fn from_selection(
        compiled: &CompiledSelection,
        library: &Library,
        uid: u32,
    ) -> Result<Tier, SelectionFailure> {
        Self::from_checked(
            &compiled.expr,
            &compiled.checked,
            library,
            &compiled.bound,
            uid,
        )
        .map_err(SelectionFailure::Usage)
    }

    /// Compile a statically checked config tier using its already-bound
    /// roots. Rebinding would lose the parser's decision about fixed and
    /// required roots, especially when several sources form a union.
    pub fn configured(
        group: &str,
        tier: &config::Tier,
        library: &Library,
        uid: u32,
    ) -> Result<Self, String> {
        let expr = tier.select.expr().to_string();
        let mut selected = Self::from_checked(&expr, &tier.select, library, &tier.roots, uid)
            .map_err(|e| format!("group `{group}` tier `{}`: {e}", tier.name))?;
        selected.group = group.to_owned();
        selected.name = tier.name.clone();
        selected.order = tier.order;
        Ok(selected)
    }

    pub(super) fn from_checked(
        expr: &str,
        checked: &select::Checked,
        library: &Library,
        bound: &std::collections::BTreeMap<String, Vec<String>>,
        uid: u32,
    ) -> Result<Self, String> {
        let diagnostic_expr = format!("{expr:?}");
        let not_yet = |what: &str| {
            format!(
                "--select {diagnostic_expr}: {what} is not supported by ensure in this version \
                 yet; select one source profile (`profile:NAME`, kind cargo-target, or \
                 kind fs), optionally with supported filters, a positive fs union with one action, or a \
                 same-source Cargo union with only positive idle filters"
            )
        };
        let mut terms = Vec::new();
        for term in &checked.expr().terms {
            terms.push(Self::select_term(
                expr, term, library, bound, uid, &not_yet,
            )?);
        }
        let mut first = terms.remove(0);
        if !matches!(
            first.action,
            Action::Delete { .. } | Action::Command { .. } | Action::CargoLockProtocol
        ) && (!first.negated.is_empty() || terms.iter().any(|term| !term.negated.is_empty()))
        {
            return Err(not_yet("!profile: with this source action"));
        }
        if !terms.is_empty() {
            // For one Cargo source, positive idle filters are lower age
            // bounds on the same candidates. Their union is the weakest
            // bound, including the source's mandatory minimum when one
            // member has no tier filter. Keep the
            // existing Cargo discovery and lock-time protocol as the one
            // authority for the reduced selection.
            if matches!(first.source, SourceKind::CargoTarget)
                && checked.expr().terms.iter().all(|term| {
                    term.source == first.profile
                        && term
                            .factors
                            .iter()
                            .all(|factor| matches!(factor, select::Factor::Filter(_)))
                })
            {
                first.idle_for = terms
                    .iter()
                    .fold(first.idle_for, |bound, term| bound.min(term.idle_for));
                first.select = checked.expr().to_string();
                return Ok(first);
            }
            if !matches!(first.source, SourceKind::Fs(_))
                || !matches!(first.action, Action::Delete { .. } | Action::Command { .. })
            {
                return Err(not_yet("a union of this kind or action"));
            }
            if terms.iter().any(|term| term.action != first.action) {
                return Err(format!(
                    "--select {diagnostic_expr}: filesystem union sources must have exactly the same action and options"
                ));
            }
            let mut extra = Vec::with_capacity(terms.len());
            for tier in &terms {
                let SourceKind::Fs(find) = &tier.source else {
                    return Err(not_yet("a union of non-filesystem sources"));
                };
                extra.push(FsUnionSource {
                    profile: tier.profile.clone(),
                    find: (**find).clone(),
                    roots: tier.roots.clone(),
                    idle: IdleWindow {
                        at_least: tier.idle_for,
                        before: tier.idle_before,
                    },
                    checkouts: tier.checkouts,
                    negated: tier.negated.clone(),
                });
            }
            // The nested-build guard keeps a build from the whole selection
            // (TASK-367, the sixth review): a build another term finds is
            // judged by it too, by its path alone, whatever the guarded
            // term's own discovery saw of it.
            let SourceKind::Fs(first_find) = &mut first.source else {
                unreachable!("checked above: a filesystem union")
            };
            if first_find.nested_build_guard
                || extra.iter().any(|term| term.find.nested_build_guard)
            {
                first_find.nested_build_guard = true;
                for term in &mut extra {
                    term.find.nested_build_guard = true;
                }
            }
            first.fs_union = extra;
            first.select = checked.expr().to_string();
        }
        Ok(first)
    }

    pub(super) fn select_term(
        expr: &str,
        term: &select::Term,
        library: &Library,
        bound: &std::collections::BTreeMap<String, Vec<String>>,
        uid: u32,
        not_yet: &impl Fn(&str) -> String,
    ) -> Result<Tier, String> {
        let diagnostic_expr = format!("{expr:?}");
        let Some(Body::Source(source)) = library.get(&term.source).map(|p| &p.body) else {
            return Err(format!(
                "--select {diagnostic_expr}: no source profile `{}`",
                term.source
            ));
        };
        // The loader admits only these finds, each paired with its action.
        let mut kind = match &source.finds {
            Finds::CargoTargets => SourceKind::CargoTarget,
            Finds::Paths { .. } => SourceKind::Fs(Box::new(
                Find::of(source, uid).expect("fs source has a filesystem find"),
            )),
            Finds::DockerBuildCache => SourceKind::DockerBuildCache,
            Finds::DockerImages => SourceKind::DockerImages,
            Finds::PodmanImages => SourceKind::PodmanImages,
            Finds::NixStore => SourceKind::NixStore,
        };
        let mut idle_before: Option<IdleFor> = None;
        let mut negated = Vec::new();
        for factor in &term.factors {
            let (name, is_negated) = match factor {
                Factor::Filter(name) => (name, false),
                Factor::NotFilter(name) => (name, true),
                Factor::NotProfile(name) => {
                    let Some(Body::Source(other)) = library.get(name).map(|p| &p.body) else {
                        return Err(format!(
                            "--select {diagnostic_expr}: no negated source profile `{name}`"
                        ));
                    };
                    let source = match &other.finds {
                        Finds::Paths {
                            patterns,
                            exclude,
                            entry_type,
                        } => NegatedKind::Fs(Box::new(Find::with_excludes(
                            patterns,
                            exclude,
                            *entry_type,
                            &crate::profile::Where::default(),
                            uid,
                        ))),
                        Finds::CargoTargets => NegatedKind::CargoTarget,
                        _ => return Err(not_yet(&format!("`!profile:{name}`"))),
                    };
                    let roots = bound
                        .get(name)
                        .ok_or_else(|| {
                            format!(
                                "--select {diagnostic_expr}: no bound roots for `!profile:{name}`"
                            )
                        })?
                        .iter()
                        .map(|root| PathBuf::from(root.replace("{uid}", &uid.to_string())))
                        .collect();
                    negated.push(NegatedSource {
                        profile: name.clone(),
                        source,
                        roots,
                    });
                    continue;
                }
            };
            let Some(Body::Filter(where_)) = library.get(name).map(|p| &p.body) else {
                return Err(format!(
                    "--select {diagnostic_expr}: no filter profile `{name}`"
                ));
            };
            if is_negated && where_.predicates() == [crate::kind::PredicateName::IdleFor] {
                let bound = where_.idle_for.expect("idle predicate present");
                idle_before = Some(idle_before.map_or(bound, |old| old.min(bound)));
                continue;
            }
            // A positive filter is a conjunction: its entry checks, idle
            // requirement and version guards judge the same candidate.
            // A negated mixed filter complements the entire conjunction,
            // using the walk's newest-change measure under the lock.
            let entry_count =
                usize::from(where_.has.is_some()) + usize::from(where_.has_not.is_some());
            let entry_and_idle_only = where_.predicates().iter().all(|predicate| {
                matches!(
                    predicate,
                    crate::kind::PredicateName::Has
                        | crate::kind::PredicateName::HasNot
                        | crate::kind::PredicateName::IdleFor
                )
            });
            if matches!(kind, SourceKind::Fs(_))
                && entry_count > 0
                && (!is_negated || entry_and_idle_only)
            {
                let SourceKind::Fs(find) = &mut kind else {
                    unreachable!()
                };
                if is_negated && (entry_count > 1 || where_.idle_for.is_some()) {
                    find.negated_mixed_filters.push(NegatedMixedFilter {
                        name: name.clone(),
                        has: where_.has.clone(),
                        has_not: where_.has_not.clone(),
                        idle_for: where_.idle_for,
                    });
                    continue;
                }
                for (entry, has) in [(&where_.has, true), (&where_.has_not, false)] {
                    if let Some(entry) = entry {
                        find.entry_filters.push(EntryFilter {
                            name: name.clone(),
                            entry: entry.clone(),
                            has,
                            negated: is_negated,
                        });
                    }
                }
                // Entry checks narrow this term after discovery; version
                // guards are independent retention checks. A filter can
                // specify both, so neither may disappear when compiling it.
                if !is_negated {
                    find.apply_version_filter(where_).map_err(|error| {
                        format!("--select {diagnostic_expr}: filter:{name}: {error}")
                    })?;
                }
                continue;
            }
            if is_negated {
                return Err(not_yet(&format!(
                    "`!filter:{name}` (unsupported predicates)"
                )));
            }
            if where_.has.is_some() || where_.has_dir.is_some() || where_.has_not.is_some() {
                return Err(not_yet(&format!(
                    "`filter:{name}` (a `has` / `has_dir` / `has_not` filter)"
                )));
            }
            if let SourceKind::Fs(find) = &mut kind {
                find.apply_version_filter(where_).map_err(|error| {
                    format!("--select {diagnostic_expr}: filter:{name}: {error}")
                })?;
            }
        }
        let idle_for = library.idle_for(term)?;
        let roots: Vec<PathBuf> = bound
            .get(&term.source)
            .map(|roots| {
                roots
                    .iter()
                    .map(|r| PathBuf::from(r.replace("{uid}", &uid.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        if let SourceKind::Fs(find) = &mut kind {
            find.bound_roots = roots.clone();
        }
        Ok(Tier {
            group: SELECT_TIER.to_owned(),
            name: SELECT_TIER.to_owned(),
            order: Order::Oldest,
            select: term.to_string(),
            profile: term.source.clone(),
            source: kind,
            roots,
            idle_for,
            idle_before,
            checkouts: source.checkouts,
            action: source.action.clone(),
            fs_union: Vec::new(),
            protections: Vec::new(),
            negated,
            max_entries: crate::engine::DEFAULT_MAX_ENTRIES,
        })
    }
}
