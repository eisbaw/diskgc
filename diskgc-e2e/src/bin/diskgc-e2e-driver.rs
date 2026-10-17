//! The e2e test driver: single diskgc operations on a container fixture,
//! pausable and killable at a chosen port call (see `diskgc_e2e::driver`).
//! Runs only inside a diskgc test container; never part of the release
//! package (`diskgc-e2e-release-check`).

fn main() {
    diskgc_e2e::interlock::enforce("diskgc-e2e-driver");
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(diskgc_e2e::driver::main(&args));
}
