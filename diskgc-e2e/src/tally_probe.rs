//! The background tally on the real kernel, for the VM pressure test
//! (docs/testing.md, `background_tally_backs_off_under_io_pressure`): the
//! driver's `tally-background SECONDS ROOT...` runs
//! [`diskgc_core::tally::background`] over the ROOTs (every directory
//! directly below each is a candidate) for SECONDS by the host's clock and
//! prints one line per event, stamped with `CLOCK_BOOTTIME` in ms (what
//! `/proc/uptime` counts), so the guest-side harness can line the batches
//! up with its own pressure samples:
//!
//! - `I <ms> started psi=<bool>` and, without PSI, `I <ms> no-psi -- <why>`;
//! - `I <ms> batch took_us=T ops=N listed=L sized=S rechecked=R
//!   evicted=E paused=P sleep_ms=Z`;
//! - `I <ms> braked io=I memory=M cpu=C sleep_ms=Z` (hundredths of a
//!   percent);
//! - `I <ms> paused` (the host lock is held), `I <ms> lock -- <why>`,
//!   `I <ms> failed -- <error>`, `I <ms> priority -- <why>`.
//!
//! Read-only: the tally reaches the filesystem through [`Host`] only.

use std::path::Path;
use std::time::Duration;

use diskgc_core::fs_kind::Find;
use diskgc_core::host::Host;
use diskgc_core::index::{BUDGET_BYTES, Finder, Index};
use diskgc_core::profile::{EntryType, Where};
use diskgc_core::tally::TallyState;
use diskgc_core::tally::background::{Background, Event};
use diskgc_core::tally::listing::{SourceFind, TallySource};

/// The line for `event` at `ms`.
pub fn line(ms: u128, event: &Event) -> String {
    match event {
        Event::Started { psi } => format!("{ms} started psi={psi}"),
        Event::PriorityNotLowered(e) => format!("{ms} priority -- {e}"),
        Event::NoPsi(e) => format!("{ms} no-psi -- {e}"),
        Event::PsiUnreadable(e) => format!("{ms} psi-unreadable -- {e}"),
        Event::Paused(_) => format!("{ms} paused"),
        Event::HostLockUnusable(e) => format!("{ms} lock -- {e}"),
        Event::Braked { reading, sleep } => format!(
            "{ms} braked io={} memory={} cpu={} sleep_ms={}",
            reading.io,
            reading.memory,
            reading.cpu,
            sleep.as_millis()
        ),
        Event::Batch(work) => format!(
            "{ms} batch took_us={} ops={} listed={} sized={} rechecked={} evicted={} paused={} sleep_ms={}",
            work.took.as_micros(),
            work.ops,
            work.listed,
            work.sized,
            work.rechecked,
            work.evicted,
            work.paused,
            work.sleep.as_millis()
        ),
        Event::Failed(e) => format!("{ms} failed -- {e}"),
    }
}

/// Runs the background tally over `roots` for `seconds`, handing each
/// event's line to `emit`.
pub fn run<H: Host>(host: &H, seconds: u64, roots: &[String], emit: &mut dyn FnMut(String)) {
    let sources: Vec<TallySource> = roots
        .iter()
        .map(|root| TallySource {
            finder: Finder {
                profile: "tally-probe".into(),
                profile_hash: 0,
                root: Path::new(root).into(),
            },
            find: SourceFind::Fs(Box::new(Find::new(
                &["*".to_owned()],
                EntryType::Dir,
                &Where::default(),
                0,
            ))),
            // A probe's plain `*` source: no source anchor, no guard.
            stamp_root: None,
        })
        .collect();
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let end = host.now().0 + Duration::from_secs(seconds);
    let mut log = |event: Event| emit(line(host.now().0.as_millis(), &event));
    let mut background = Background::start(host, &mut log);
    while host.now().0 < end && host.interrupted().is_none() {
        let sleep = background.turn(host, &mut state, &sources, &mut log);
        host.sleep(sleep.min(end.saturating_sub(host.now().0)));
    }
}
