//! Fixture processes that announce `ready` once their state holds and live
//! until their stdin closes: the in-use fixtures ([`crate::inuse_probe`])
//! and the Cargo lock holder ([`crate::cargo_probe`]). Started untraced by
//! the harness: they are fixtures, not code under test.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::suite::{Ctx, TestResult};
use crate::{ensure, info};

/// A fixture process that printed `ready`; it ends when dropped (its stdin
/// closes).
pub struct Holder {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    what: String,
}

impl Holder {
    /// `diskgc-e2e probe ARGS` as tester (through `as-tester`), with only
    /// `PATH` and `env` in its environment.
    pub fn tester(ctx: &Ctx, args: &[&str], env: &[(&str, &str)]) -> Result<Self, String> {
        let mut cmd = Command::new(&ctx.self_exe);
        cmd.args(["as-tester", "--"])
            .arg(&ctx.self_exe)
            .arg("probe")
            .args(args);
        Self::start(cmd, args, env)
    }

    /// Run a copied probe binary from a candidate directory, so the
    /// kernel's `/proc/<pid>/exe` and executable mappings name that version.
    pub fn tester_program(
        ctx: &Ctx,
        program: &std::path::Path,
        args: &[&str],
    ) -> Result<Self, String> {
        let mut cmd = Command::new(&ctx.self_exe);
        cmd.args(["as-tester", "--"])
            .arg(program)
            .arg("probe")
            .args(args);
        Self::start(cmd, args, &[])
    }

    /// `diskgc-e2e probe ARGS` as container root (it drops privileges or
    /// sets up a namespace itself).
    pub fn root(ctx: &Ctx, args: &[&str]) -> Result<Self, String> {
        let mut cmd = Command::new(&ctx.self_exe);
        cmd.arg("probe").args(args);
        Self::start(cmd, args, &[])
    }

    fn start(mut cmd: Command, args: &[&str], env: &[(&str, &str)]) -> Result<Self, String> {
        let what = args.join(" ");
        let mut child = cmd
            .env_clear()
            .env("PATH", "/bin")
            .envs(env.iter().copied())
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {what}: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().ok_or("holder stdout")?);
        let mut holder = Self {
            child,
            stdin,
            stdout,
            what,
        };
        let ready = holder.line()?;
        ensure!(ready == "ready", "{} said {ready:?}", holder.what);
        info!("fixture {} is pid {}", holder.what, holder.pid());
        Ok(holder)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("read {}: {e}", self.what))?;
        ensure!(n > 0, "{} ended early", self.what);
        Ok(line.trim_end().to_owned())
    }

    pub fn send(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("holder stdin closed")?;
        writeln!(stdin, "{line}").map_err(|e| format!("write {}: {e}", self.what))
    }

    /// Closes its stdin and requires a clean exit.
    pub fn finish(mut self) -> TestResult {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|e| format!("wait {}: {e}", self.what))?;
        ensure!(status.success(), "{} exited with {status}", self.what);
        Ok(())
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}
