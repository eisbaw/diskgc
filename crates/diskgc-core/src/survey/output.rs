//! `survey`'s output (docs/reference.md, `survey`): the JSON object and
//! the human text, both views of [`Survey`] computed when printed, so
//! they cannot disagree. Per `PATH`: the plan's per-term summary
//! ([`summary::terms_without_goal`], the `ensure` dry run's blocks
//! without `would_reclaim`), the candidates (the plan's rows, largest
//! first, at most [`LISTED_PER_PROFILE`] per profile), and the unmatched
//! entries by name family ([`crate::index::name_family`]).

use std::collections::BTreeMap;
use std::path::Path;

use super::unmatched::{Entry, Size, Unsized};
use super::{PathSurvey, Survey, Why};
use crate::executor::age_seconds;
use crate::host::Timestamp;
use crate::index::name_family;
use crate::report::json::Json;
use crate::report::summary;
use crate::report::{ErrorEntry, PlanEntry, Status, printable, term_text, term_to_json};
use crate::units::{format_age, format_bytes};

/// The version of `survey --json`'s schema. Unlike `ensure`'s report it
/// is not frozen: a release may change its fields, and says so by
/// raising this number (Mark, TASK-116). Consumers check it.
pub const SCHEMA_VERSION: u64 = 1;

/// Candidates listed per profile and `PATH` (`--list`, JSON
/// `candidates`); the rest are counted (`left_out`).
pub const LISTED_PER_PROFILE: usize = 1_000;

/// Example paths per name family.
pub const FAMILY_EXAMPLES: usize = 5;

/// Name families the human text lists per `PATH`, largest first; the rest
/// are summed in one line. The JSON lists every family.
pub const FAMILIES_SHOWN: usize = 20;

impl Survey {
    /// The JSON object (one line).
    pub fn to_json(&self) -> String {
        self.json().to_text()
    }

    fn json(&self) -> Json {
        Json::Obj(vec![
            ("schema_version", Json::U64(SCHEMA_VERSION)),
            ("status", Json::str(self.status.as_str())),
            ("exit_code", Json::U64(self.exit_code().into())),
            (
                "started_at",
                Json::opt(self.started_at.as_deref(), Json::str),
            ),
            ("duration_ms", Json::opt(self.duration_ms, Json::U64)),
            ("version", Json::str(env!("CARGO_PKG_VERSION"))),
            (
                "paths",
                Json::Arr(self.paths.iter().map(path_json).collect()),
            ),
            ("errors", errors_json(&self.errors)),
        ])
    }

    /// The human text: per `PATH` a heading, one line per profile that
    /// found something, the profiles that found nothing in one line, and
    /// the largest unmatched name families; with `list`, each profile's
    /// candidates under its line. Per-`PATH` errors are summarised; they
    /// were logged in full on stderr as they happened.
    pub fn to_text(&self, list: bool) -> String {
        let mut out = String::new();
        // A refusal before any plan prints nothing here: its error is on
        // stderr already, and stdout carries only results.
        for path in &self.paths {
            path_text(path, list, &mut out);
        }
        match self.status {
            Status::Deadline => out
                .push_str("survey stopped at the deadline (--timeout): what is above is partial\n"),
            Status::Interrupted(signal) => out.push_str(&format!(
                "survey interrupted by {}: what is above is partial\n",
                signal.name()
            )),
            _ => {}
        }
        out
    }
}

/// A plan row's candidate, if it has one.
fn candidate(row: &PlanEntry) -> Option<&Path> {
    row.candidate.as_deref()
}

/// The rows of `profile` with a candidate, largest first (unsized last),
/// then by path: the order `--list` and `candidates` show them in.
fn rows_of<'p>(path: &'p PathSurvey, profile: &str) -> Vec<&'p PlanEntry> {
    let mut rows: Vec<&PlanEntry> = path
        .plan
        .plan
        .iter()
        .filter(|row| row.profile == profile && candidate(row).is_some())
        .collect();
    rows.sort_by(|a, b| {
        b.estimated_bytes
            .cmp(&a.estimated_bytes)
            .then_with(|| a.candidate.cmp(&b.candidate))
    });
    rows
}

/// A row's idle age: an eligible row's, when its last change is known and
/// not in the future; `None` for a kept row.
fn idle_age(row: &PlanEntry, now: Timestamp) -> Option<u64> {
    row.reason
        .is_none()
        .then(|| age_seconds(now, row.idle_since))
        .flatten()
}

/// The oldest and the newest idle age among `rows`' eligible ones.
fn age_range(rows: &[&PlanEntry], now: Timestamp) -> Option<(u64, u64)> {
    let ages = rows.iter().filter_map(|row| idle_age(row, now));
    ages.fold(None, |range, age| match range {
        None => Some((age, age)),
        Some((oldest, newest)) => Some((oldest.max(age), newest.min(age))),
    })
}

fn path_json(path: &PathSurvey) -> Json {
    let plan = &path.plan;
    let terms = summary::terms_without_goal(plan);
    let mut candidates = Vec::new();
    let summary: Vec<Json> = terms
        .iter()
        .map(|term| {
            let rows = rows_of(path, term.profile);
            let listed = rows.len().min(LISTED_PER_PROFILE);
            candidates.extend(rows[..listed].iter().map(|row| candidate_json(row, path)));
            let range = age_range(&rows, path.surveyed_at);
            let Json::Obj(mut fields) = term_to_json(term) else {
                unreachable!("a summary block is an object");
            };
            fields.extend([
                (
                    "oldest_idle_age_seconds",
                    Json::opt(range.map(|r| r.0), Json::U64),
                ),
                (
                    "newest_idle_age_seconds",
                    Json::opt(range.map(|r| r.1), Json::U64),
                ),
                ("listed", Json::U64(listed as u64)),
                ("left_out", Json::U64((rows.len() - listed) as u64)),
            ]);
            Json::Obj(fields)
        })
        .collect();
    let refused: Vec<Json> = plan
        .plan
        .iter()
        .filter(|row| candidate(row).is_none())
        .map(|row| {
            Json::Obj(vec![
                ("profile", Json::str(&row.profile)),
                ("reason", Json::opt(row.reason, |r| Json::str(r.as_str()))),
            ])
        })
        .collect();
    Json::Obj(vec![
        ("path", Json::opt(plan.path.as_deref(), path_str)),
        ("mount", Json::opt(plan.mount.as_deref(), path_str)),
        ("status", Json::str(plan.status.as_str())),
        ("avail_bytes", Json::opt(plan.avail_before_bytes, Json::U64)),
        ("summary", Json::Arr(summary)),
        ("refused", Json::Arr(refused)),
        (
            "not_surveyed",
            Json::Arr(
                path.not_surveyed
                    .iter()
                    .map(|n| {
                        Json::Obj(vec![
                            ("profile", Json::str(&n.profile)),
                            ("why", Json::str(n.why.as_str())),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("candidates", Json::Arr(candidates)),
        (
            "unmatched",
            Json::opt(path.unmatched.as_deref(), |entries| {
                unmatched_json(path, entries)
            }),
        ),
        ("errors", errors_json(&plan.errors)),
    ])
}

fn candidate_json(row: &PlanEntry, path: &PathSurvey) -> Json {
    Json::Obj(vec![
        ("profile", Json::str(&row.profile)),
        ("candidate", Json::opt(candidate(row), path_str)),
        ("eligible", Json::Bool(row.reason.is_none())),
        ("reason", Json::opt(row.reason, |r| Json::str(r.as_str()))),
        ("estimated_bytes", Json::opt(row.estimated_bytes, Json::U64)),
        (
            "idle_age_seconds",
            Json::opt(idle_age(row, path.surveyed_at), Json::U64),
        ),
    ])
}

/// The unmatched entries grouped by name family, largest family first.
struct Family<'e> {
    family: String,
    entries: Vec<&'e Entry>,
}

impl Family<'_> {
    fn bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|e| sized(e).map(|s| s.est_bytes))
            .fold(0, u64::saturating_add)
    }

    fn unmeasured(&self) -> u64 {
        self.entries.iter().filter(|e| sized(e).is_none()).count() as u64
    }

    /// The oldest and newest idle ages of its sized members.
    fn ages(&self, now: Timestamp) -> Option<(u64, u64)> {
        self.entries
            .iter()
            .filter_map(|e| age_seconds(now, sized(e)?.newest_change))
            .fold(None, |range, age| match range {
                None => Some((age, age)),
                Some((oldest, newest)) => Some((oldest.max(age), newest.min(age))),
            })
    }
}

fn sized(entry: &Entry) -> Option<crate::tally::size::Sized> {
    match entry.size {
        Size::Sized(sized) => Some(sized),
        Size::Unsized(_) => None,
    }
}

/// `entries` by family, largest first (then by family), each family's
/// members largest first (then by name).
fn families(entries: &[Entry]) -> Vec<Family<'_>> {
    let mut by_family: BTreeMap<Vec<u8>, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        let family = name_family(entry.name.as_encoded_bytes()).into_vec();
        by_family.entry(family).or_default().push(entry);
    }
    let mut families: Vec<Family<'_>> = by_family
        .into_iter()
        .map(|(family, mut entries)| {
            entries.sort_by(|a, b| {
                let bytes = |e: &Entry| sized(e).map(|s| s.est_bytes);
                bytes(b).cmp(&bytes(a)).then_with(|| a.name.cmp(&b.name))
            });
            Family {
                family: String::from_utf8_lossy(&family).into_owned(),
                entries,
            }
        })
        .collect();
    families.sort_by(|a, b| {
        b.bytes()
            .cmp(&a.bytes())
            .then_with(|| a.family.cmp(&b.family))
    });
    families
}

fn unmatched_json(path: &PathSurvey, entries: &[Entry]) -> Json {
    let families = families(entries);
    let all = Family {
        family: String::new(),
        entries: entries.iter().collect(),
    };
    let dir = path.plan.path.as_deref().unwrap_or(Path::new(""));
    Json::Obj(vec![
        ("count", Json::U64(entries.len() as u64)),
        ("estimated_bytes", Json::U64(all.bytes())),
        ("unsized", Json::U64(all.unmeasured())),
        (
            "families",
            Json::Arr(
                families
                    .iter()
                    .map(|family| {
                        let ages = family.ages(path.surveyed_at);
                        Json::Obj(vec![
                            ("family", Json::str(&family.family)),
                            ("count", Json::U64(family.entries.len() as u64)),
                            ("estimated_bytes", Json::U64(family.bytes())),
                            ("unsized", Json::U64(family.unmeasured())),
                            (
                                "oldest_idle_age_seconds",
                                Json::opt(ages.map(|a| a.0), Json::U64),
                            ),
                            (
                                "newest_idle_age_seconds",
                                Json::opt(ages.map(|a| a.1), Json::U64),
                            ),
                            (
                                "examples",
                                Json::Arr(
                                    family
                                        .entries
                                        .iter()
                                        .take(FAMILY_EXAMPLES)
                                        .map(|e| path_str(&dir.join(&e.name)))
                                        .collect(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn errors_json(errors: &[ErrorEntry]) -> Json {
    Json::Arr(errors.iter().map(ErrorEntry::to_json).collect())
}

/// A path as a JSON string (bytes that are not UTF-8 as U+FFFD, as in the
/// `ensure` report).
fn path_str(path: &Path) -> Json {
    Json::str(path.to_string_lossy())
}

/// `idle 3 h to 12 d` (or `idle 3 h`), for an age range.
fn ages_text(range: Option<(u64, u64)>) -> Option<String> {
    let (oldest, newest) = range?;
    let (oldest, newest) = (format_age(oldest), format_age(newest));
    // Ages that read alike (`625 d` and `625 d`) are said once.
    Some(if oldest == newest {
        format!("idle {oldest}")
    } else {
        format!("idle {newest} to {oldest}")
    })
}

fn path_text(path: &PathSurvey, list: bool, out: &mut String) {
    let plan = &path.plan;
    let shown = plan
        .path
        .as_deref()
        .map_or_else(|| "?".to_owned(), |p| printable(&p.to_string_lossy()));
    let mount = plan
        .mount
        .as_deref()
        .map(|m| format!(" (filesystem {}", printable(&m.to_string_lossy())))
        .unwrap_or_default();
    let avail = match (plan.mount.is_some(), plan.avail_before_bytes) {
        (true, Some(bytes)) => format!(", {} available)", format_bytes(bytes)),
        (true, None) => ")".to_owned(),
        _ => String::new(),
    };
    let elapsed = plan
        .duration_ms
        .map(|ms| format!(" in {:.1} s", ms as f64 / 1000.0))
        .unwrap_or_default();
    out.push_str(&format!("survey {shown}{mount}{avail}{elapsed}\n"));
    let why_stopped = if plan.status == Status::Deadline {
        "deadline"
    } else {
        "the plan stopped"
    };
    let terms = summary::terms_without_goal(plan);
    let mut quiet: Vec<&str> = Vec::new();
    for term in &terms {
        if term.found() == Some(0) {
            quiet.push(term.profile);
            continue;
        }
        let rows = rows_of(path, term.profile);
        let ages = ages_text(age_range(&rows, path.surveyed_at))
            .map(|a| format!(" · {a}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  profile {}: {}{ages}\n",
            printable(term.profile),
            term_text(term, why_stopped)
        ));
        if list {
            list_rows(&rows, path.surveyed_at, out);
        }
    }
    for row in plan.plan.iter().filter(|row| candidate(row).is_none()) {
        out.push_str(&format!(
            "  profile {}: not planned: {}\n",
            printable(&row.profile),
            row.reason.map_or("refused", |r| r.as_str())
        ));
    }
    for (why, label) in [
        (
            Why::RootsElsewhere,
            "not looked for here (their roots are elsewhere)",
        ),
        (
            Why::Backend,
            "not surveyed below a PATH (select one with --select)",
        ),
    ] {
        let names: Vec<String> = path
            .not_surveyed
            .iter()
            .filter(|n| n.why == why)
            .map(|n| printable(&n.profile))
            .collect();
        if !names.is_empty() {
            out.push_str(&format!("  {label}: {}\n", names.join(", ")));
        }
    }
    if !quiet.is_empty() {
        out.push_str(&format!(
            "  found nothing: {}\n",
            quiet
                .iter()
                .map(|p| printable(p))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    match path.unmatched.as_deref() {
        Some(entries) => unmatched_text(path, entries, out),
        None => out.push_str("  unmatched entries: not listed\n"),
    }
    for error in &plan.errors {
        out.push_str(&format!("  error: {}\n", printable(&error.message)));
    }
}

fn list_rows(rows: &[&PlanEntry], now: Timestamp, out: &mut String) {
    for row in rows.iter().take(LISTED_PER_PROFILE) {
        let size = row
            .estimated_bytes
            .map_or_else(|| "unsized".to_owned(), format_bytes);
        let verdict = row.reason.map_or("eligible", |r| r.as_str());
        let age = idle_age(row, now)
            .map(|a| format!(" idle {}", format_age(a)))
            .unwrap_or_default();
        let at = candidate(row).map_or_else(String::new, |c| printable(&c.to_string_lossy()));
        out.push_str(&format!("      {size:>10}  {verdict}{age}  {at}\n"));
    }
    if rows.len() > LISTED_PER_PROFILE {
        out.push_str(&format!(
            "      ... {} more not listed\n",
            rows.len() - LISTED_PER_PROFILE
        ));
    }
}

fn size_text(bytes: u64, unmeasured: u64) -> String {
    match unmeasured {
        0 => format!("est. {}", format_bytes(bytes)),
        n => format!("est. {} + {n} unsized", format_bytes(bytes)),
    }
}

fn unmatched_text(path: &PathSurvey, entries: &[Entry], out: &mut String) {
    if entries.is_empty() {
        out.push_str("  unmatched entries: none\n");
        return;
    }
    let families = families(entries);
    let all = Family {
        family: String::new(),
        entries: entries.iter().collect(),
    };
    out.push_str(&format!(
        "  unmatched: {} top-level entr{}, {}, in {} name famil{}{}:\n",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" },
        size_text(all.bytes(), all.unmeasured()),
        families.len(),
        if families.len() == 1 { "y" } else { "ies" },
        if families.len() > FAMILIES_SHOWN {
            format!(" ({FAMILIES_SHOWN} largest)")
        } else {
            String::new()
        }
    ));
    for family in families.iter().take(FAMILIES_SHOWN) {
        let ages = ages_text(family.ages(path.surveyed_at))
            .map(|a| format!("  {a}"))
            .unwrap_or_default();
        let example = family
            .entries
            .first()
            .map(|e| printable(&e.name.to_string_lossy()))
            .unwrap_or_default();
        out.push_str(&format!(
            "    {}  {} entr{}  {}{ages}  e.g. {example}\n",
            printable(&family.family),
            family.entries.len(),
            if family.entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            size_text(family.bytes(), family.unmeasured()),
        ));
    }
    let rest = &families[families.len().min(FAMILIES_SHOWN)..];
    if !rest.is_empty() {
        let members: usize = rest.iter().map(|f| f.entries.len()).sum();
        let bytes = rest.iter().map(Family::bytes).fold(0, u64::saturating_add);
        let unmeasured = rest.iter().map(Family::unmeasured).sum();
        out.push_str(&format!(
            "    ... {} more families, {members} entries, {}\n",
            rest.len(),
            size_text(bytes, unmeasured)
        ));
    }
    let refused = entries
        .iter()
        .filter(|e| matches!(e.size, Size::Unsized(Unsized::Refused(_))))
        .count();
    if refused > 0 {
        out.push_str(&format!(
            "    ({refused} unsized: more entries than a walk takes, or an unreadable or private entry inside)\n"
        ));
    }
}
