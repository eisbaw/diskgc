//! The safety interlock for destructive tests (docs/testing.md, Safety
//! interlock).
//!
//! - Layer 1 (build time) is structural: this crate is not a workspace member.
//! - Layer 2 (run time) is [`enforce`]: the first call in `main` of every
//!   binary of this crate that runs in a container. Outside a diskgc test
//!   container it prints why and exits with [`exit::INTERLOCK`].
//! - Layer 3 (per operation) is [`abort_outside_scratch`], called by
//!   [`crate::scratch`] when an operation would touch a path that is not on
//!   the scratch mount.

use std::path::Path;

use crate::exit;

/// Refuses to continue unless this process runs in a diskgc test container
/// (image marker with the compiled-in constant, and Podman's marker).
pub fn enforce(program: &str) {
    if let Err(why) = diskgc_linux::sandbox::check() {
        eprintln!(
            "{program}: INTERLOCK: refusing to run outside a diskgc test container: {why}\n\
             {program}: destructive tests run only through `just e2e` (docs/testing.md)."
        );
        std::process::exit(exit::INTERLOCK);
    }
}

/// Stops the whole test process: a fixture operation was about to touch a
/// path outside the scratch mount. Never returns; nothing after the check
/// runs, so the offending operation is never performed.
pub fn abort_outside_scratch(op: &str, path: &Path, why: &str) -> ! {
    eprintln!(
        "INTERLOCK: {op} {}: {why}; aborting the test before touching it",
        path.display()
    );
    std::process::exit(exit::INTERLOCK)
}
