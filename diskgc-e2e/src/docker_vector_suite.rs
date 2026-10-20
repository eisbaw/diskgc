//! Replay a checked-in ModelHost profile case through the real diskgc binary
//! while a pinned native Docker Engine runs in the disposable dind fixture.
//! This proves the vector's plan, action, and deletion contract at the kernel
//! boundary without mounting a developer project or host Docker socket.

use crate::docker_suite::{Daemon, assert_native_server_identity};
use crate::profile_vector_suite::replay_python_vector;
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

pub fn checked_in_python_profile_vector_replays_under_native_docker(ctx: &Ctx) -> TestResult {
    ensure!(
        ctx.image.name == "docker-dind",
        "wrong image for native Docker vector replay: {}",
        ctx.image.name
    );
    ctx.scratch.mkdir("docker-client", 0o755, TESTER)?;
    let _daemon = Daemon::start(ctx)?;
    assert_native_server_identity(ctx)?;

    // The daemon may change its private tmpfs while diskgc is under test.
    // No profile candidate or neighboring project path is exempted.
    replay_python_vector(
        ctx,
        &["/var/lib/docker", "/var/lib/containerd", "/run/docker"],
    )
}
