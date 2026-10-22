{ pkgs, image, underTest }:

# TASK-25.1: a disposable QEMU kernel is the only place where CLOCK_REALTIME
# may be moved to give two tmpfs inodes genuinely different kernel ctimes.
pkgs.testers.nixosTest {
  name = "diskgc-historical-ctime";

  nodes.machine = { ... }: {
    boot.kernelParams = [ "diskgc.e2e.historical_ctime=1" ];
    virtualisation.podman.enable = true;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 4096;
  };

  testScript = ''
    import time

    host_started = time.time()
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Podman's graph root lives on the VM's private ext4 disk. The Nix store
    # comes from the test driver's read-only virtiofs export.
    assert machine.succeed("findmnt -n -o FSTYPE -T /var/lib/containers").strip() == "ext4"
    assert machine.succeed("podman info --format '{{.Store.GraphDriverName}}'").strip() == "overlay"
    assert "mount_program" not in machine.succeed("podman info --format '{{.Store.GraphOptions}}'")
    assert machine.execute("${underTest}/bin/diskgc-e2e-canary")[0] == 86
    assert "diskgc.e2e.historical_ctime=1" in machine.succeed("cat /proc/cmdline").split()

    # Observe the driver host clock while the guest still has an old wall
    # clock. A final-only check would miss a temporary leaked clock shift
    # because each replay restores the guest clock before returning.
    host_wall = time.time()
    host_mono = time.monotonic()
    try:
      machine.succeed("date -u -s '@1790812800' >/dev/null")
      guest_old = int(machine.succeed("date -u +%s"))
      assert abs(guest_old - 1790812800) < 10, guest_old
      host_while_guest_old = time.time()
      expected_host = host_wall + (time.monotonic() - host_mono)
      assert abs(host_while_guest_old - expected_host) < 5, (host_while_guest_old, expected_host)
    finally:
      restored_guest = int(host_wall + (time.monotonic() - host_mono))
      machine.succeed(f"date -u -s '@{restored_guest}' >/dev/null")

    machine.succeed("${image} | podman load")
    # The image carries the ordinary E2E marker. Removing it only inside a
    # disposable Podman overlay must make the compiled canary refuse.
    code, output = machine.execute(
      "podman run --rm --pull=never --network=none "
      "--volume /nix/store:/nix/store:ro "
      "--volume ${underTest}/bin:/opt/diskgc-e2e/bin:ro "
      "--entrypoint /bin/sh ${image.imageName}:${image.imageTag} "
      "-c 'rm /.diskgc-test-sandbox; /opt/diskgc-e2e/bin/diskgc-e2e-canary'"
    )
    # The VM driver sends container stderr to the console rather than the
    # execute() return value. Exit 86 is the canary's missing-marker refusal.
    assert code == 86, (code, output)
    guest_started = int(machine.succeed("date -u +%s"))
    for profile in ["cmake", "autotools", "meson", "containers_image"]:
      machine.succeed(
        "podman run --rm --pull=never --network=none "
        "--cap-add SYS_ADMIN --cap-add SYS_PTRACE --cap-add SYS_TIME "
        "--volume /nix/store:/nix/store:ro "
        "--volume ${underTest}/bin:/opt/diskgc-e2e/bin:ro "
        "${image.imageName}:${image.imageTag} "
        "/opt/diskgc-e2e/bin/diskgc-e2e run "
        f"vm_{profile}_original_matches_linux_outcomes_and_deletion_diff"
      )
    guest_finished = int(machine.succeed("date -u +%s"))
    host_finished = time.time()
    assert 0 <= guest_finished - guest_started < 300, (guest_started, guest_finished)
    assert 0 <= host_finished - host_started < 600, (host_started, host_finished)
  '';
}
