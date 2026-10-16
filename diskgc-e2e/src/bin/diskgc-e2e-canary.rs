//! Interlock canary: does nothing, behind the same interlock as the
//! destructive test binary. The runner executes it on the host before every
//! run and requires it to refuse (docs/testing.md, Negative feedback:
//! "Interlock tests bite too"); a container test requires it to pass inside.

fn main() {
    diskgc_e2e::interlock::enforce("diskgc-e2e-canary");
    println!("diskgc-e2e-canary: interlock passed; doing nothing");
}
