//! Kind `nix-store`'s reclaim under the host lock (docs/safety.md,
//! nix-store, steps 7 to 10): the daemon bound again with its socket and
//! client pinned, the root check, one GC bounded by what is still needed,
//! measured. The binding itself is [`crate::nix_backend`].

use std::path::PathBuf;
use std::time::Duration;

use crate::backend::Binding;
use crate::engine::Reason;
use crate::host::{Cause, Delete, MonoTime, NixGc};
use crate::nix_backend;

use super::prune_backend::NIX;

/// The detail of a binding that differs at the recheck in a way
/// [`nix_backend::recheck`] does not name itself.
const CHANGED: &str = "Nix client, daemon version, daemon socket or store changed before the GC";
use super::{Outcome, Protect, ProtectAt, Reclaim, backend_child_end, not_forwarded_note, sample};

impl<H: Delete> Reclaim<'_, '_, H> {
    /// The Nix GC of the daemon `bound` under the host lock before any tier
    /// ran (docs/safety.md, nix-store, steps 7 to 10): bound again, its
    /// socket pinned, and compared (`changed`); the root check through that
    /// socket; one GC through it, bounded by what is still needed; the
    /// effect measured. One GC per tier: an undershoot moves on to the next
    /// tier.
    pub(super) fn walk_nix(&mut self, bound: &nix_backend::Bound) {
        if self.stop_if_interrupted() {
            return;
        }
        let candidate = PathBuf::from(nix_backend::STORE_DIR);
        if self.protected(&candidate, ProtectAt::BeforeReclaim) != Protect::Clear {
            return;
        }
        if self.stop_if_interrupted() {
            return;
        }
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        if let Err(error) = sampled {
            self.host_failure(error);
            return;
        }
        let before = sampled.expect("checked above").avail_bytes;
        if self.goal_reached(before) {
            return;
        }
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        // Step 7: the same client, daemon version, store and socket inode;
        // the socket pinned here is the one the root check and the GC use.
        let started = self.host.now();
        let rebound = nix_backend::recheck(self.host, self.measured.space, bound);
        self.record_overrun("preflight", &candidate, started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        let pinned = match rebound {
            Ok(Binding::Bound(pinned)) if pinned.bound == *bound => pinned,
            Ok(Binding::Bound(_)) => {
                // `recheck` compares each fact; a binding it returns that
                // still differs is `changed` all the same.
                self.skip_backend(NIX, Some(&candidate), Reason::Changed, CHANGED);
                return;
            }
            Ok(Binding::Skipped(skip)) => {
                self.skip_backend(NIX, Some(&candidate), skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.tier_error(format!("recheck {NIX} backend: {error}"));
                return;
            }
        };
        if self.stop_if_interrupted() {
            return;
        }
        // Step 8: every generation diskgc sees is a root of this daemon.
        let started = self.host.now();
        let listed = nix_backend::generations(self.host);
        let generations = match listed {
            Ok(Ok(generations)) => generations,
            Ok(Err(skip)) => {
                self.skip_backend(NIX, Some(&candidate), skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.tier_error(format!("list Nix generations: {error}"));
                return;
            }
        };
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        if self.stop_if_interrupted() {
            return;
        }
        let deadline = self.deadline.unwrap_or(MonoTime(Duration::MAX));
        let environment =
            nix_backend::child_environment(self.host.nix_environment().home.as_deref());
        let roots = self.host.nix_roots(
            &pinned.client,
            &pinned.socket,
            &nix_backend::targets(&generations),
            &environment,
            deadline,
        );
        self.record_overrun("root check", &candidate, started, self.host.now());
        let checked_in = self.host.now().saturating_since(started);
        let output = match roots {
            Ok(output) => output,
            Err(error) => {
                if self.stop_if_interrupted() {
                    return;
                }
                if self.past_deadline() {
                    self.deadline_hit = true;
                    return;
                }
                if error.cause == Cause::SandboxGate {
                    self.run_error(format!("Nix root check: {error}"));
                } else {
                    self.tier_error(format!("Nix root check: {error}"));
                }
                return;
            }
        };
        if let Err(skip) = nix_backend::check_roots(&generations, &output) {
            self.skip_backend(NIX, Some(&candidate), skip.reason, &skip.detail);
            return;
        }
        (self.log)(&format!(
            "{}: Nix root check: the daemon counts all {} generation link(s) below {} as \
             roots ({} ms)",
            self.tier.name,
            generations.len(),
            nix_backend::PROFILES,
            checked_in.as_millis()
        ));
        if self.stop_if_interrupted() {
            return;
        }
        // Step 9: one GC bounded by what is still needed now.
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        let avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(error) => {
                self.host_failure(error);
                return;
            }
        };
        if self.goal_reached(avail) {
            return;
        }
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        let max_freed = nix_backend::need(self.measured.target, avail);
        (self.log)(&format!(
            "{}: Nix GC through the daemon at {} (client {}, daemon {}), --max-freed {max_freed}",
            self.tier.name,
            nix_backend::SOCKET,
            bound.client_version,
            bound.daemon_version
        ));
        let run = NixGc {
            max_freed,
            environment,
            output_prefix: format!("{}/{}", self.tier.name, self.tier.profile),
            deadline,
            second_signal: self.second_signal,
        };
        let started = self.host.now();
        let invocation = self.host.nix_gc(&pinned.client, &pinned.socket, &run);
        drop(pinned);
        let note = not_forwarded_note(invocation.lines_not_forwarded());
        let (attempted, end) = backend_child_end(invocation, &candidate, NIX, "Nix GC");
        // Step 10: measured by the action's own samples.
        let _ = self.record(
            &candidate,
            Outcome {
                started,
                before: avail,
                expected_bytes: Some(max_freed),
                tallied_bytes: None,
                idle_age_seconds: None,
                removed_entries: 0,
                attempted_command: attempted,
                end,
                note,
            },
        );
    }
}
