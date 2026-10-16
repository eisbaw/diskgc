//! Host-side e2e runner: one container per test, durations, time budget.
//! Runs nothing destructive itself (see `diskgc_e2e::runner`).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(diskgc_e2e::runner::main(&args));
}
