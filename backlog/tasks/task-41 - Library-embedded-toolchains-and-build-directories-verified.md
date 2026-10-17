---
id: TASK-41
title: 'Library: embedded toolchains and build directories, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 11:12'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-40
references:
  - docs/reference.md#built-in-profile-library
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Zephyr/west, Renode and PlatformIO leave large scratch on embedded developers' machines. Ship zephyr-tmp, west-build, renode-tmp, platformio-cache, cmake-build and pytest-cache, settling how profiles that find at any depth under a project root work without ** (explicit depths or one root per project).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile has examples, a golden set and verified_with
- [ ] #2 The profiles use the any-depth rule decided in the selection-engine task
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/): BUG in the cmake-build profile as written in docs/reference.md. It matched ~/topics/cmake_nix_backend/CMake, a git checkout built in-source (CMakeCache.txt at the source root next to .git and CMakeLists.txt); 8 of 42 matches had CMakeLists.txt beside the cache. Fix the definition before it ships: has_not CMakeLists.txt and .git in the matched directory. The catalog has not shipped, so the name may stay; the path recorded in CMakeCache.txt cannot be used (container builds record /fw_build). Add these cases to the profile's golden match set. Also: bottles temp (~/.local/share/bottles/temp) as a candidate profile.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
