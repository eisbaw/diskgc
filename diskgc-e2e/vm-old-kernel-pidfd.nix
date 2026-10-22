{ pkgs, image, underTest }:

let
  # Hold a pidfd across a deterministic child release. This probes the
  # kernel behavior independently of diskgc's own interpretation of it.
  pidfdProbe = pkgs.writeText "diskgc-pidfd-6.6-probe.py" ''
    import os
    import select
    import signal

    reader, writer = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(writer)
        os.read(reader, 1)
        os._exit(0)

    os.close(reader)
    pidfd = None
    try:
        pidfd = os.pidfd_open(child, 0)
        poller = select.poll()
        poller.register(pidfd, select.POLLIN | select.POLLHUP | select.POLLERR)
        assert poller.poll(0) == [], "held child was not live before release"
        signal.pidfd_send_signal(pidfd, 0, None, 0)

        os.write(writer, b"release")
        os.close(writer)
        writer = -1
        waited, status = os.waitpid(child, 0)
        assert waited == child and os.WIFEXITED(status), (waited, status)

        events = poller.poll(1000)
        assert len(events) == 1 and events[0][0] == pidfd, events
        mask = events[0][1]
        assert mask & select.POLLIN, mask
        assert not mask & select.POLLHUP, "expected Linux 6.6 poll without HUP: " + str(mask)
        try:
            signal.pidfd_send_signal(pidfd, 0, None, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError("released child was not reported as ESRCH")
        print("Linux 6.6: released pidfd is readable without HUP; signal 0 gives ESRCH")
    finally:
        if writer >= 0:
            os.close(writer)
        if pidfd is not None:
            os.close(pidfd)
        try:
            os.waitpid(child, 0)
        except ChildProcessError:
            pass
  '';
in
# TASK-64 G: a pinned old-kernel guest, with any destructive diskgc work
# confined to its disposable Podman container and private ext4 disk.
pkgs.testers.nixosTest {
  name = "diskgc-old-kernel-pidfd";

  nodes.machine = { ... }: {
    boot.kernelPackages = pkgs.linuxPackages_6_6;
    virtualisation.podman.enable = true;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 4096;
    environment.systemPackages = [ pkgs.python3 ];
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    kernel = machine.succeed("uname -r").strip()
    assert kernel.startswith("6.6.158"), kernel
    machine.succeed("python3 ${pidfdProbe}")

    # Podman's graph root is on the VM's private ext4 disk. The Nix store
    # and binaries are mounted read-only from the VM test driver/store.
    assert machine.succeed("findmnt -n -o FSTYPE -T /var/lib/containers").strip() == "ext4"
    assert machine.succeed("podman info --format '{{.Store.GraphDriverName}}'").strip() == "overlay"
    assert "mount_program" not in machine.succeed("podman info --format '{{.Store.GraphOptions}}'")
    assert machine.execute("${underTest}/bin/diskgc-e2e-canary")[0] == 86

    machine.succeed("${image} | podman load")
    for case in ["host_pidfd_release_after_reap", "host_inuse_scan_settles_after_fork_churn"]:
      machine.succeed(
        "podman run --rm --pull=never --network=none "
        "--cap-add SYS_ADMIN --cap-add SYS_PTRACE "
        "--volume /nix/store:/nix/store:ro "
        "--volume ${underTest}/bin:/opt/diskgc-e2e/bin:ro "
        "${image.imageName}:${image.imageTag} "
        "/opt/diskgc-e2e/bin/diskgc-e2e run "
        f"{case}"
      )
  '';
}
