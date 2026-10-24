//! Times the in-use process scan on the real host, read-only: the number
//! that docs/safety.md's Freshness bound (one second from a scan's first
//! read to a deletion) is measured against. Nothing is deleted, opened for
//! writing or locked; the scan only reads `/proc`.
//!
//! Usage: `scan_timing [SCANS] [--pytest-facts]` (default 20 scans). Prints
//! one line per scan, then the minimum, median, p95 and maximum of its age
//! when returned, with the process count and the 1-minute load average
//! before the first scan. `--pytest-facts` also reads what a source with
//! `liveness_contract: pytest` needs (and Owner PID's table).
//! `just scan-timing` builds it for the host (gnu) and for the shipped static
//! musl target and runs both.

use std::time::Duration;

use diskgc_core::host::Host;
use diskgc_core::inuse::{self, Options};
use diskgc_linux::LinuxHost;

/// The most scans one run makes: bounds a mistyped argument.
const SCANS_MAX: usize = 1000;

fn main() {
    let mut scans = 20;
    let mut pytest_facts = false;
    for arg in std::env::args().skip(1) {
        if arg == "--pytest-facts" {
            pytest_facts = true;
        } else {
            scans = arg
                .parse()
                .unwrap_or_else(|_| panic!("SCANS must be a number, got {arg:?}"));
        }
    }
    assert!(
        (1..=SCANS_MAX).contains(&scans),
        "SCANS must be 1..={SCANS_MAX}"
    );
    let host = LinuxHost::new().expect("a procfs /proc of this pid namespace");
    let users = [host.effective_uid()];
    let options = Options {
        users: &users,
        sockets: false,
        pid_lookup: pytest_facts,
        pytest_facts,
    };
    let load = std::fs::read_to_string("/proc/loadavg").expect("read /proc/loadavg");
    println!(
        "processes listed: {}, load: {}, pytest facts: {pytest_facts}",
        host.pids().expect("list /proc").len(),
        load.split_whitespace().next().expect("loadavg has fields"),
    );
    let mut took: Vec<Duration> = Vec::with_capacity(scans);
    for i in 0..scans {
        let scan = inuse::scan(&host, &options).expect("scan");
        // The age of the scan when it is returned: from its first read
        // (its start) to now, which includes the work after the process
        // reads (Owner PID's table, the pytest facts, the environment
        // index) that `Scan::took` leaves out. This is what a deletion's
        // freshness check sees at best.
        let age = host.now().saturating_since(scan.started());
        println!(
            "scan {i}: {:.0} ms old when returned ({:.0} ms of process reads), {} processes \
             read, {} listings, settled: {}",
            age.as_secs_f64() * 1e3,
            scan.took().as_secs_f64() * 1e3,
            scan.processes(),
            scan.listings(),
            scan.settled(),
        );
        took.push(age);
    }
    took.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    // Nearest-rank percentiles over the sorted sample.
    let rank = |p: usize| took[(p * scans).div_ceil(100).max(1) - 1];
    println!(
        "min {:.0} ms, median {:.0} ms, p95 {:.0} ms, max {:.0} ms over {scans} scans",
        ms(took[0]),
        ms(rank(50)),
        ms(rank(95)),
        ms(took[scans - 1]),
    );
}
