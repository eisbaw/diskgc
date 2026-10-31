//! `watch` (docs/reference.md, `watch`): so far its live index and the
//! background tally on a thread of its own ([`live`], TASK-236); the
//! watcher's loop is TASK-32.

pub mod live;
