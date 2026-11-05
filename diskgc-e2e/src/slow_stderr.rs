//! A slow consumer of a traced run's stderr (TASK-342).
//!
//! The run's stderr is a pipe that [`SlowStderr`] empties one 4 KiB read
//! every 250 ms. Once the pipe's buffer is full, every write to it blocks
//! for up to that pause: far longer than a scheduling hiccup, and longer
//! than the 50 ms per line that once failed a command action (a slow
//! terminal, a busy log file or a stalled journal consumer, made
//! repeatable). Everything written still arrives: the reader drains the
//! pipe to end of file, which comes once every copy of the write end is
//! closed, so the caller hands the write end to the run and keeps none.

use std::os::fd::OwnedFd;
use std::thread::JoinHandle;
use std::time::Duration;

/// Bytes taken per read.
const CHUNK: usize = 4096;
/// The pause before each read.
const PAUSE: Duration = Duration::from_millis(250);

/// The reader thread; [`SlowStderr::finish`] returns all it read.
pub struct SlowStderr {
    reader: JoinHandle<Result<Vec<u8>, String>>,
}

impl SlowStderr {
    /// A blocking pipe whose read end this reader drains slowly, and its
    /// write end for the run's stderr. Both ends are close-on-exec, so no
    /// other child of the harness holds the write end open.
    pub fn start() -> Result<(Self, OwnedFd), String> {
        let (read_end, write_end) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
            .map_err(|e| format!("make slow stderr pipe: {e}"))?;
        let reader = std::thread::Builder::new()
            .name("slow-stderr".to_owned())
            .spawn(move || {
                let mut all = Vec::new();
                let mut chunk = [0_u8; CHUNK];
                loop {
                    std::thread::sleep(PAUSE);
                    match rustix::io::read(&read_end, &mut chunk) {
                        Ok(0) => return Ok(all),
                        Ok(n) => all.extend_from_slice(&chunk[..n]),
                        Err(rustix::io::Errno::INTR) => {}
                        Err(e) => return Err(format!("read slow stderr: {e}")),
                    }
                }
            })
            .map_err(|e| format!("start slow stderr reader: {e}"))?;
        Ok((Self { reader }, write_end))
    }

    /// Everything the run wrote to stderr, once its last writer closed it.
    pub fn finish(self) -> Result<String, String> {
        let bytes = self
            .reader
            .join()
            .map_err(|_| "the slow stderr reader panicked".to_owned())??;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}
