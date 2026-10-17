//! Checks that the release package contains neither the test driver nor the
//! controlled `Host` wrapper (see `diskgc_e2e::release_check`). Reads files
//! only; run by the flake check `release-excludes-e2e`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(diskgc_e2e::release_check::main(&args));
}
