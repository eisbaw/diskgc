#!/usr/bin/env python3
"""A process population shaped like the development laptop's user processes
(measured 2026-10-24 for TASK-115: 281 processes, 2873 threads, 8481 fds,
125k map lines, in three classes). Each child holds its fds, threads and
file mappings until the parent is killed. Why: diskgc's in-use scan reads
every visible process, and an idle container hides scan-cost bugs (TASK-115:
a reclaim cleared nothing on a busy host before its fix).

Usage: load.py READY_FILE SCRATCH_DIR. Writes the child count to READY_FILE
once every child is set up."""
import ctypes, mmap, os, signal, sys, threading, time

CLASSES = [  # (processes, threads each, fds each, extra file mappings each)
    (87, 24, 69, 900),
    (57, 10, 22, 300),
    (137, 1, 8, 0),
]
ready, scratch = sys.argv[1], sys.argv[2]
os.makedirs(scratch, exist_ok=True)
page = os.path.join(scratch, "page")
with open(page, "wb") as f:
    f.write(b"\0" * mmap.PAGESIZE * 2)

def child(threads, fds, maps):
    held = [os.open("/dev/null", os.O_RDONLY) for _ in range(fds)]
    fd = os.open(page, os.O_RDONLY)
    # libc mmap, not Python's mmap.mmap, which keeps a dup'd fd per mapping.
    # Alternate offsets so neighbouring mappings never merge into one VMA.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
    for i in range(maps):
        addr = libc.mmap(None, mmap.PAGESIZE, mmap.PROT_READ, mmap.MAP_SHARED, fd, (i % 2) * mmap.PAGESIZE)
        assert addr not in (None, ctypes.c_void_p(-1).value), ctypes.get_errno()
    os.close(fd)
    for _ in range(threads - 1):
        threading.Thread(target=time.sleep, args=(10**6,), daemon=True).start()
    signal.pause()

kids = []
for procs, threads, fds, maps in CLASSES:
    for _ in range(procs):
        pid = os.fork()
        if pid == 0:
            try:
                child(threads, fds, maps)
            finally:
                os._exit(0)
        kids.append(pid)
time.sleep(3)
open(ready, "w").write(f"{len(kids)}\n")
signal.pause()
