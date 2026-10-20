//! A process in a FUSE mount whose server has stopped answering. The tiny
//! server implements only the kernel requests needed to open the mount root;
//! its wire fields follow Linux `include/uapi/linux/fuse.h` protocol 7.31.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, UnmountFlags};

use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test;
use crate::{TESTER, ensure};

const FUSE_INIT: u32 = 26;
const FUSE_GETATTR: u32 = 3;
const FUSE_OPENDIR: u32 = 27;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_ACCESS: u32 = 34;
const FUSE_FORGET: u32 = 2;

struct FuseServer {
    mount: PathBuf,
    answering: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl FuseServer {
    fn mount(path: &Path) -> Result<Self, String> {
        let device = rustix::fs::open(
            "/dev/fuse",
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| format!("open /dev/fuse in disposable container: {error}"))?;
        let options = format!(
            "fd={},rootmode=40755,user_id=0,group_id=0,allow_other",
            device.as_raw_fd()
        );
        let options = std::ffi::CString::new(options).map_err(|error| error.to_string())?;
        rustix::mount::mount(
            "diskgc-e2e-stall",
            path,
            "fuse",
            MountFlags::NOSUID | MountFlags::NODEV,
            options.as_c_str(),
        )
        .map_err(|error| format!("mount FUSE at {}: {error}", path.display()))?;
        let answering = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let worker_answering = Arc::clone(&answering);
        let worker_stop = Arc::clone(&stop);
        let worker_error = Arc::clone(&error);
        let worker = std::thread::spawn(move || {
            if let Err(why) = serve(device, &worker_answering, &worker_stop) {
                eprintln!("FUSE fixture server failed: {why}");
                *worker_error.lock().expect("FUSE error mutex") = Some(why);
            }
        });
        Ok(Self {
            mount: path.to_owned(),
            answering,
            stop,
            error,
            worker: Some(worker),
        })
    }

    fn stop_answering(&self) {
        self.answering.store(false, Ordering::SeqCst);
    }

    fn check(&self) -> TestResult {
        if let Some(error) = self.error.lock().expect("FUSE error mutex").as_ref() {
            return Err(error.clone());
        }
        Ok(())
    }
}

impl Drop for FuseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = rustix::mount::unmount(&self.mount, UnmountFlags::DETACH);
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(at..at + 4)
            .ok_or("short FUSE request")?
            .try_into()
            .expect("four bytes"),
    ))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    Ok(u64::from_ne_bytes(
        bytes
            .get(at..at + 8)
            .ok_or("short FUSE request")?
            .try_into()
            .expect("eight bytes"),
    ))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

fn reply(device: &mut File, unique: u64, error: i32, body: &[u8]) -> Result<(), String> {
    let mut bytes = vec![0u8; 16 + body.len()];
    let len = bytes.len() as u32;
    put_u32(&mut bytes, 0, len);
    put_u32(&mut bytes, 4, error as u32);
    put_u64(&mut bytes, 8, unique);
    bytes[16..].copy_from_slice(body);
    device
        .write_all(&bytes)
        .map_err(|why| format!("reply to FUSE request: {why}"))
}

fn root_attr() -> [u8; 88] {
    let mut attr = [0u8; 88];
    put_u64(&mut attr, 0, 1); // ino
    put_u32(&mut attr, 60, 0o040755); // mode
    put_u32(&mut attr, 64, 2); // nlink
    put_u32(&mut attr, 68, TESTER.uid); // uid
    put_u32(&mut attr, 72, TESTER.gid); // gid
    put_u32(&mut attr, 80, 4096); // blksize
    attr
}

fn serve(device: OwnedFd, answering: &AtomicBool, stop: &AtomicBool) -> Result<(), String> {
    let mut device = File::from(device);
    let mut request = [0u8; 256 * 1024];
    while !stop.load(Ordering::SeqCst) {
        let len = match device.read(&mut request) {
            Ok(len) => len,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(format!("read FUSE request: {error}")),
        };
        if len < 40 || read_u32(&request[..len], 0)? as usize != len {
            return Err(format!("malformed FUSE request: {len} bytes"));
        }
        let opcode = read_u32(&request[..len], 4)?;
        let unique = read_u64(&request[..len], 8)?;
        if !answering.load(Ordering::SeqCst) {
            // Hold the request without a reply. Closing the device on stop
            // aborts it, so a broken diskgc scan cannot hang the harness.
            continue;
        }
        match opcode {
            FUSE_INIT => {
                let major = read_u32(&request[..len], 40)?;
                let minor = read_u32(&request[..len], 44)?;
                if major != 7 || minor < 31 {
                    return Err(format!("unsupported FUSE protocol {major}.{minor}"));
                }
                let mut body = [0u8; 64];
                put_u32(&mut body, 0, 7);
                put_u32(&mut body, 4, 31);
                put_u32(&mut body, 20, 128 * 1024); // max_write
                put_u32(&mut body, 24, 1); // time_gran
                reply(&mut device, unique, 0, &body)?;
            }
            FUSE_GETATTR => {
                let mut body = [0u8; 104];
                body[16..].copy_from_slice(&root_attr());
                reply(&mut device, unique, 0, &body)?;
            }
            FUSE_OPENDIR => reply(&mut device, unique, 0, &[0u8; 16])?,
            FUSE_RELEASEDIR | FUSE_ACCESS => reply(&mut device, unique, 0, &[])?,
            FUSE_FORGET => {}
            _ => return Err(format!("unexpected FUSE opcode {opcode}")),
        }
    }
    Ok(())
}

/// A stopped FUSE server must not turn another process's open candidate
/// file into a false "not in use" answer or hold ensure beyond its deadline.
pub fn inuse_scan_finishes_with_a_process_in_a_stalled_fuse_mount(ctx: &Ctx) -> TestResult {
    let scratch = &ctx.scratch;
    scratch.mkdir("fuse-stalled", 0o755, TESTER)?;
    scratch.mkdir("fuse-candidate", 0o755, TESTER)?;
    scratch.write_file("fuse-candidate/held", b"precious", 0o644, TESTER)?;
    ctx.home.write_file(
        "diskgc.yaml",
        b"version: 1\nprofiles:\n  - name: fuse-scan-candidate\n    find: {roots: [/tmp], paths: [fuse-candidate], type: dir}\n    action: delete\n    examples: {match: [/tmp/fuse-candidate]}\n",
        0o644,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let server = FuseServer::mount(&scratch.path("fuse-stalled"))?;
    let mount = "/tmp/fuse-stalled";
    let fuse_holder = Holder::tester(ctx, &["hold", mount, mount], &[])?;
    let candidate_holder = Holder::tester(ctx, &["hold", "/tmp/fuse-candidate/held", mount], &[])?;
    server.check()?;
    server.stop_answering();

    let (send, receive) = std::sync::mpsc::channel();
    let stalled = std::thread::spawn(move || {
        let result = std::fs::symlink_metadata(mount);
        let _ = send.send(result.map(|_| ()));
    });
    ensure!(
        receive.recv_timeout(Duration::from_secs(2)).is_err(),
        "FUSE server answered after it was paused"
    );
    let abort = Arc::clone(&server.stop);
    let (cancel, canceled) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if canceled.recv_timeout(Duration::from_secs(8)).is_err() {
            abort.store(true, Ordering::SeqCst);
        }
    });
    let started = Instant::now();
    let run = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--no-index",
            "--timeout",
            "3s",
            "--min-free",
            "100%",
            "--config",
            "/home/tester/diskgc.yaml",
            "--select",
            "profile:fuse-scan-candidate",
            "/tmp",
        ],
    )?;
    let elapsed = started.elapsed();
    let _ = cancel.send(());
    watchdog.join().map_err(|_| "FUSE watchdog panicked")?;
    server.check()?;
    drop(server);
    stalled.join().map_err(|_| "FUSE probe panicked")?;
    oracle.judge(&run)?.assert_clean()?;
    ensure!(
        elapsed < Duration::from_secs(3),
        "in-use scan exceeded its 3s CLI deadline: {elapsed:?}"
    );
    run.expect_code(1)?;
    let report = crate::ensure_suite::report(&run)?;
    ensure!(
        report["skipped"]
            .as_array()
            .is_some_and(|skipped| skipped.iter().any(|entry| {
                entry["candidate"] == "/tmp/fuse-candidate" && entry["reason"] == "in_use"
            })),
        "held candidate was not refused as in use: {report}"
    );
    ensure!(
        report["actions"].as_array().is_some_and(Vec::is_empty),
        "unexpected actions: {report}"
    );
    ensure!(
        scratch.path("fuse-candidate/held").exists(),
        "held candidate was removed"
    );
    drop(candidate_holder);
    drop(fuse_holder);
    Ok(())
}
