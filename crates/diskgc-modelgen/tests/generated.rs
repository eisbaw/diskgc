//! Generated host models are valid, printable and consistent, and events can
//! be placed at every call of a scenario.

use std::path::{Path, PathBuf};

use diskgc_core::host::{Host, ProcError};
use diskgc_core::resolve::{Want, resolve};
use diskgc_model::ModelHost;
use diskgc_model::load::parse_yaml;
use diskgc_model::spec::{EventSpec, NodeSpec, When};
use diskgc_modelgen::{ROOT, host_model, paths, with_event_at_call};
use proptest::prelude::*;

fn loaded(model: &diskgc_model::HostModel) -> Result<ModelHost, TestCaseError> {
    ModelHost::new(model).map_err(|e| TestCaseError::fail(format!("{e}\n{}", model.to_yaml())))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn generated_models_load_and_round_trip_through_yaml(model in host_model(3, 4, 3)) {
        loaded(&model)?;
        let yaml = model.to_yaml();
        let back = parse_yaml(&yaml).map_err(|e| TestCaseError::fail(format!("{e}\n{yaml}")))?;
        prop_assert_eq!(back, model);
    }

    #[test]
    fn generated_directories_resolve_and_open_files_are_the_trees(model in host_model(3, 4, 3)) {
        let host = loaded(&model)?;
        let NodeSpec::Dir(root) = &model.tree[Path::new(ROOT)] else {
            return Err(TestCaseError::fail("the root is a directory"));
        };
        let (dirs, _) = paths(Path::new(ROOT), root);
        for dir in &dirs {
            let resolved = resolve(&host, dir, Want::Directory);
            prop_assert!(resolved.is_ok(), "{}: {:?}", dir.display(), resolved.err());
        }
        for spec in &model.processes {
            let pid = diskgc_core::host::Pid(spec.pid);
            if spec.proc == diskgc_model::spec::ProcAccess::Readable {
                // Exe, map and environment are never empty in generated
                // processes, so they read like LinuxHost's live processes.
                prop_assert!(host.proc_maps(pid).is_ok());
                prop_assert!(host.proc_environ(pid).is_ok());
            }
            match host.proc_fds(pid) {
                Ok(fds) => {
                    prop_assert_eq!(fds.len(), spec.fds.len());
                    for (fd, wanted) in fds.iter().zip(&spec.fds) {
                        let stat = host.stat_path(&wanted.path).expect("in the tree");
                        prop_assert_eq!(fd.target.stat.dev_ino(), stat.dev_ino());
                    }
                }
                Err(ProcError::Denied(_) | ProcError::Failed(_)) => {}
                Err(ProcError::Gone) => return Err(TestCaseError::fail("listed pids are not gone")),
            }
        }
    }

    #[test]
    fn an_event_fires_at_whichever_call_it_is_placed(model in host_model(2, 3, 1)) {
        // A small scenario: resolve the root. Count its calls, then place an
        // event at each of them in turn.
        let scenario = |host: &ModelHost| resolve(host, Path::new(ROOT), Want::Directory).map(drop);
        let calls = {
            let host = loaded(&model)?;
            let _ = scenario(&host);
            host.trace().len() as u64
        };
        prop_assert!(calls >= 5);
        for k in 1..=calls {
            let marker = PathBuf::from(format!("/tmp/event-{k}"));
            let event = EventSpec { mkdir: Some(marker.clone()), ..EventSpec::default() };
            let host = loaded(&with_event_at_call(&model, k, When::After, event))?;
            let _ = scenario(&host);
            prop_assert!(host.unfired_events().is_empty(), "event at call {k} did not fire");
            prop_assert!(host.stat_path(&marker).is_some());
        }
    }
}
