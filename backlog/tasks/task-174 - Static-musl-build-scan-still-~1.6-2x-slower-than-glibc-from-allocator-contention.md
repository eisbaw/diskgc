---
id: TASK-174
title: >-
  Static musl build: scan still ~1.6-2x slower than glibc from allocator
  contention
status: To Do
assignee: []
created_date: '2026-10-25 02:19'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-145 removed the large per-read buffers (scratch.rs): per scan mmap/munmap ~5,400 -> ~2,000 and futex ~25,000 -> ~8,400 (strace -c, walk container, ~300 processes/2,800 threads). Remaining: ~50k allocations per scan, mostly small (ProcFd/LinkTarget vectors, environ/maps copies, status read_to_end), all on mallocng's one lock across 8 reader threads; glibc scan median still 1.6-2x lower at load 16-24 (scan_timing). Options to measure: parse environ/maps inside the scratch borrow instead of copying out; keep reader threads alive across scans (their scratch buffers are re-grown every scan today); a scalable allocator for the static build (a new C dependency: supply-chain decision for Mark). Measure with just scan-timing plus strace -c in a loaded walk container.
<!-- SECTION:DESCRIPTION:END -->
