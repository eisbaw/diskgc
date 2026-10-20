//! Reviewable profile scenarios backed by a fresh `ModelHost` for planning
//! and reclaim. This module belongs to the test-only model crate; product
//! code never reads vector expectations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::Library;
use diskgc_core::units::parse_size;
use serde::Deserialize;

use crate::ModelHost;
use crate::spec::{HostModel, Op};

/// One versioned test case. The host is the ordinary strict host model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileVector {
    pub profile_vector: u32,
    pub profile: String,
    pub path: PathBuf,
    #[serde(default)]
    pub roots: Vec<String>,
    pub min_free: String,
    pub host: HostModel,
    pub expect: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// Complete dry-run plan, including skipped candidates, sorted by path.
    pub plan: Vec<PlanVerdict>,
    pub reclaim_exit: i32,
    /// Complete action set, sorted by path.
    pub actions: Vec<PathBuf>,
    /// Paths that must be gone after reclaim. Actions can retain their
    /// candidate (for example Cargo keeps the tagged target root).
    pub removed: Vec<PathBuf>,
    /// Complete reclaim skip set, sorted by path.
    pub skipped: Vec<SkipVerdict>,
    /// Existing paths that must survive reclaim, including candidates not
    /// discovered because required source evidence is absent.
    pub preserved: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerdict {
    pub candidate: PathBuf,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkipVerdict {
    pub candidate: PathBuf,
    pub reason: String,
}

impl ProfileVector {
    pub fn parse(yaml: &str) -> Result<Self, String> {
        let vector: Self = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
        if vector.profile_vector != 1 {
            return Err(format!(
                "unsupported profile_vector {} (expected 1)",
                vector.profile_vector
            ));
        }
        if vector.host.expect.is_some() {
            return Err("profile vector host must not contain a second expect block".to_owned());
        }
        ModelHost::new(&vector.host).map_err(|e| e.to_string())?;
        Ok(vector)
    }

    /// Runs both modes against independent copies of the same inode graph.
    /// The expectations are independent data: no expected path is derived
    /// from diskgc's report. A wrong matcher yields a mismatch here.
    pub fn check(&self, library: &Library) -> Result<(), String> {
        let roots = (!self.roots.is_empty()).then_some(self.roots.as_slice());
        let tier = Tier::select(
            &format!("profile:{}", self.profile),
            library,
            roots,
            self.host.own_uid.unwrap_or(1000),
        )?;
        let request = Request {
            path: self.path.clone(),
            min_free: parse_size(&self.min_free)?,
            target_free: None,
            timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            tier,
        };
        let planning = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        let uid = executor::privileges(&planning, false).map_err(|e| e.to_string())?;
        let report = executor::dry_run(&planning, &request, &[uid], &mut |_| {});
        if !report.errors.is_empty() {
            return Err(format!("dry-run errors: {:?}", report.errors));
        }
        let mut actual_plan: Vec<_> = report
            .plan
            .iter()
            .map(|entry| PlanVerdict {
                candidate: entry.candidate.clone(),
                reason: entry.reason.map(str::to_owned),
                source: entry.build_evidence.as_ref().map(|e| e.source.clone()),
            })
            .collect();
        actual_plan.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        let mut expected_plan = self.expect.plan.clone();
        expected_plan.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        if actual_plan != expected_plan {
            return Err(format!(
                "profile {} dry-run plan\nactual: {actual_plan:#?}\nexpected: {expected_plan:#?}",
                self.profile
            ));
        }
        if planning.trace().iter().any(|call| call.op == Op::Unlinkat) {
            return Err("dry-run called unlinkat".to_owned());
        }

        let reclaiming = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        for path in &self.expect.removed {
            if reclaiming.stat_path(path).is_none() {
                return Err(format!(
                    "removed path {} was absent before reclaim",
                    path.display()
                ));
            }
        }
        for path in &self.expect.preserved {
            if reclaiming.stat_path(path).is_none() {
                return Err(format!(
                    "preserved path {} was absent before reclaim",
                    path.display()
                ));
            }
        }
        let uid = executor::privileges(&reclaiming, false).map_err(|e| e.to_string())?;
        let report = executor::ensure(&reclaiming, &request, &[uid], &mut |_| {});
        if !report.errors.is_empty() {
            return Err(format!("reclaim errors: {:?}", report.errors));
        }
        let exit = i32::from(report.exit_code());
        if exit != self.expect.reclaim_exit {
            return Err(format!(
                "profile {} reclaim exit {exit}, expected {}",
                self.profile, self.expect.reclaim_exit
            ));
        }
        let mut actions: Vec<_> = report.actions.iter().map(|e| e.candidate.clone()).collect();
        actions.sort();
        let mut expected_actions = self.expect.actions.clone();
        expected_actions.sort();
        if actions != expected_actions {
            return Err(format!(
                "profile {} reclaim actions {actions:?}, expected {expected_actions:?}",
                self.profile
            ));
        }
        let mut skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|entry| SkipVerdict {
                candidate: entry.candidate.clone(),
                reason: entry.reason.to_owned(),
            })
            .collect();
        skipped.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        let mut expected_skipped = self.expect.skipped.clone();
        expected_skipped.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        if skipped != expected_skipped {
            return Err(format!(
                "profile {} reclaim skipped {skipped:?}, expected {expected_skipped:?}",
                self.profile
            ));
        }
        for path in &self.expect.removed {
            if !expected_actions
                .iter()
                .any(|action| path.starts_with(action))
            {
                return Err(format!(
                    "removed path {} is outside every action",
                    path.display()
                ));
            }
            if reclaiming.stat_path(path).is_some() {
                return Err(format!("removed path {} still exists", path.display()));
            }
        }
        for action in &expected_actions {
            if !self
                .expect
                .removed
                .iter()
                .any(|path| path.starts_with(action))
            {
                return Err(format!(
                    "action {} has no explicit removed-path postcondition",
                    action.display()
                ));
            }
        }
        for path in &self.expect.preserved {
            if reclaiming.stat_path(path).is_none() {
                return Err(format!("protected path {} disappeared", path.display()));
            }
        }
        let expected_paths: Vec<_> = expected_actions.iter().map(PathBuf::as_path).collect();
        for call in reclaiming
            .trace()
            .iter()
            .filter(|call| call.op == Op::Unlinkat)
        {
            let Some(path) = call.path.as_deref() else {
                return Err(format!("unlinkat without a path: {call}"));
            };
            if !expected_paths
                .iter()
                .any(|candidate| path.starts_with(candidate))
            {
                return Err(format!("unlinkat outside expected actions: {path:?}"));
            }
        }
        if !reclaiming.unfired_events().is_empty() {
            return Err(format!("unfired events: {:?}", reclaiming.unfired_events()));
        }
        Ok(())
    }

    pub fn expected_actions(&self) -> &[PathBuf] {
        &self.expect.actions
    }

    pub fn expected_preserved(&self) -> &[PathBuf] {
        &self.expect.preserved
    }

    pub fn profile_name(&self) -> &str {
        &self.profile
    }

    pub fn root_path(&self) -> &Path {
        &self.path
    }
}
