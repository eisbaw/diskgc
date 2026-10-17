//! The directory stack both engine steps walk with: one frame per directory
//! from the candidate down to the current one, at most [`FD_WINDOW`] of
//! them below the candidate holding an fd (see the module docs of
//! [`super`]).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::host::{Access, Host, MonoTime, Stat};
use crate::resolve::{ResolveError, Want, open_beneath};

use super::{Candidate, FD_WINDOW, Identity, deadline_passed};

/// One directory on the way down.
pub(super) struct Frame<F> {
    /// Its listing in the record.
    pub listing: usize,
    /// Its name in its parent (for frame 0, the candidate's name).
    pub name: OsString,
    pub identity: Identity,
    /// Its allocated bytes, as recorded.
    pub bytes: u64,
    /// The next entry of its listing to handle.
    pub cursor: usize,
    /// The walk has listed it.
    pub listed: bool,
    fd: Option<F>,
}

/// A frame that could not be (re)opened: which one (possibly an ancestor
/// of the one wanted), and why.
#[derive(Debug)]
pub(super) struct OpenFailed {
    pub frame: usize,
    pub why: OpenFailure,
}

/// Why a frame could not be (re)opened.
#[derive(Debug)]
pub(super) enum OpenFailure {
    /// `open_beneath` refused or failed.
    Resolve(ResolveError),
    /// Opened, but not the recorded directory (`found`: what it is now).
    NotSame { found: Option<Identity> },
    /// The deadline passed before the next open.
    DeadlinePassed,
}

pub(super) struct Stack<'c, H: Host> {
    host: &'c H,
    candidate: &'c Candidate<'c, H::Fd>,
    deadline: Option<MonoTime>,
    frames: Vec<Frame<H::Fd>>,
    /// Frames below the candidate (index 1 and up) holding an fd.
    open_below: usize,
}

impl<'c, H: Host> Stack<'c, H> {
    pub fn new(
        host: &'c H,
        candidate: &'c Candidate<'c, H::Fd>,
        deadline: Option<MonoTime>,
    ) -> Self {
        Self {
            host,
            candidate,
            deadline,
            frames: Vec::new(),
            open_below: 0,
        }
    }

    /// Adds a directory below the top frame (or the candidate itself, as
    /// the first frame). It is opened by [`Stack::open`].
    pub fn push(&mut self, listing: usize, name: OsString, identity: Identity, bytes: u64) {
        self.frames.push(Frame {
            listing,
            name,
            identity,
            bytes,
            cursor: 0,
            listed: false,
            fd: None,
        });
    }

    /// Removes the top frame, closing its fd at once (before any reopening
    /// of its ancestors, so the window holds).
    pub fn pop(&mut self) -> Option<Frame<H::Fd>> {
        let mut frame = self.frames.pop()?;
        if frame.fd.take().is_some() && !self.frames.is_empty() {
            self.open_below -= 1;
        }
        Some(frame)
    }

    /// The index of the top frame.
    pub fn top(&self) -> Option<usize> {
        self.frames.len().checked_sub(1)
    }

    pub fn frame(&self, i: usize) -> &Frame<H::Fd> {
        &self.frames[i]
    }

    pub fn frame_mut(&mut self, i: usize) -> &mut Frame<H::Fd> {
        &mut self.frames[i]
    }

    /// The fd of frame `i`, which [`Stack::open`] opened.
    pub fn fd(&self, i: usize) -> &H::Fd {
        self.frames[i]
            .fd
            .as_ref()
            .expect("engine bug: frame used before Stack::open")
    }

    /// The directory holding frame `i`: the candidate's parent for frame 0.
    fn parent_fd(&self, i: usize) -> &H::Fd {
        match i {
            0 => self.candidate.parent,
            _ => self.fd(i - 1),
        }
    }

    /// Makes sure frame `i` holds an fd, reopening it and any ancestors
    /// without one from the nearest open ancestor down, each by its name
    /// and matched against its recorded identity, with a deadline check
    /// before each. Returns frame `i`'s fresh stat when this call opened it
    /// (`None`: it was open already).
    pub fn open(&mut self, i: usize) -> Result<Option<Stat>, OpenFailed> {
        if self.frames[i].fd.is_some() {
            return Ok(None);
        }
        let first = (0..i)
            .rev()
            .find(|&k| self.frames[k].fd.is_some())
            .map_or(0, |k| k + 1);
        let mut stat = None;
        for k in first..=i {
            let failed = |why| OpenFailed { frame: k, why };
            if deadline_passed(self.host, self.deadline) {
                return Err(failed(OpenFailure::DeadlinePassed));
            }
            if k > 0 {
                self.make_room(k);
            }
            let (fd, opened) = open_beneath(
                self.host,
                self.parent_fd(k),
                self.candidate.mount_id,
                Path::new(&self.frames[k].name),
                Access::Path,
                Want::Directory,
            )
            .map_err(|e| failed(OpenFailure::Resolve(e)))?;
            let found = Identity::of(&opened);
            if found != Some(self.frames[k].identity) {
                return Err(failed(OpenFailure::NotSame { found }));
            }
            self.frames[k].fd = Some(fd);
            if k > 0 {
                self.open_below += 1;
            }
            stat = Some(opened);
        }
        Ok(stat)
    }

    /// Before frame `k` (below the candidate) is opened: closes the fds of
    /// the frames nearest the candidate (never the candidate's, never `k`'s
    /// parent's) until opening `k` keeps at most [`FD_WINDOW`] frames below
    /// the candidate open.
    fn make_room(&mut self, k: usize) {
        let mut j = 1;
        while self.open_below >= FD_WINDOW && j + 1 < k {
            if self.frames[j].fd.take().is_some() {
                self.open_below -= 1;
            }
            j += 1;
        }
    }

    /// The path of frame `i` (with `name` joined, if given), for reports.
    pub fn path(&self, i: usize, name: Option<&OsStr>) -> PathBuf {
        let mut path = self.candidate.path.to_path_buf();
        for frame in &self.frames[1..=i] {
            path.push(&frame.name);
        }
        if let Some(name) = name {
            path.push(name);
        }
        path
    }
}
