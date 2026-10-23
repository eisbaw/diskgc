{
  pkgs,
  image,
  underTest,
  # The diskgc-linux unit-test executable; its *_kernel_fact tests run here.
  kernelFactTests,
  kernelPackages,
  expectedKernel,
  expectPollHup,
  cores,
  nixosTest ? pkgs.testers.nixosTest,
  probePython ? pkgs.python3,
  probePythonCommand ? "${probePython}/bin/python3",
  # Extra attributes for the test driver (e.g. an old driver's lint switch).
  driverAttrs ? { },
  # Extra `podman run` options for the guest's runtime.
  podmanRunOptions ? "",
}:

let
  # Hold a pidfd across a deterministic child release. This probes the
  # kernel behavior independently of diskgc's own interpretation of it.
  pidfdProbe = pkgs.writeText "diskgc-pidfd-${expectedKernel}-probe.py" ''
    import os
    import select
    import signal

    expect_hup = ${if expectPollHup then "True" else "False"}
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
        assert bool(mask & select.POLLHUP) == expect_hup, (mask, expect_hup)
        try:
            signal.pidfd_send_signal(pidfd, 0, None, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError("released child was not reported as ESRCH")
        print("released pidfd poll mask=" + str(mask) + "; signal 0 gives ESRCH")
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
# TASK-64 G: a pinned kernel guest, with any destructive diskgc work
# confined to its disposable Podman container and private ext4 disk.
assert kernelPackages.kernel.version == expectedKernel;
nixosTest ({
  name = "diskgc-pidfd-${expectedKernel}";

  nodes.machine = { ... }: {
    boot.kernelPackages = kernelPackages;
    virtualisation.podman.enable = true;
    # The scan and fork workers need separate vCPUs to overlap reliably.
    virtualisation.cores = cores;
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 4096;
    environment.systemPackages = [ probePython ];
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    kernel = machine.succeed("uname -r").strip()
    assert kernel == "${expectedKernel}", kernel
    print(f"guest kernel: {kernel}")
    online_cores = int(machine.succeed("nproc").strip())
    assert online_cores == ${toString cores}, online_cores
    print(f"guest vCPUs: {online_cores}")
    print("guest pidfd: " + machine.succeed("${probePythonCommand} ${pidfdProbe}").strip())

    # Podman's graph root is on the VM's private ext4 disk. The Nix store
    # and binaries are mounted read-only from the VM test driver/store.
    # podman info first: older Podman (2.1, the 5.8 guest's) creates its
    # graph root on first use.
    assert machine.succeed("podman info --format '{{.Store.GraphDriverName}}'").strip() == "overlay"
    assert machine.succeed("findmnt -n -o FSTYPE -T /var/lib/containers").strip() == "ext4"
    assert "mount_program" not in machine.succeed("podman info --format '{{.Store.GraphOptions}}'")
    assert machine.execute("${underTest}/bin/diskgc-e2e-canary")[0] == 86

    # The kernel facts the host tests pin (diskgc-linux *_kernel_fact) must
    # hold on this kernel too: which /proc reads delay a lock's release
    # differs by version (docs/safety.md, Host lock). Both must run and pass:
    # the exact count also catches a test renamed off the suffix, or a filter
    # that matches nothing.
    facts = machine.succeed(
      "cd /tmp && ${kernelFactTests}/bin/diskgc-linux-tests kernel_fact "
      "--nocapture --test-threads 1 2>&1"
    )
    print(facts)
    assert "test result: ok. 2 passed; 0 failed" in facts, facts

    machine.succeed("${image} | podman load")
    for case in ["host_pidfd_release_after_reap", "host_inuse_scan_settles_after_fork_churn"]:
      machine.succeed(
        "podman run --rm --pull=never --network=none "
        "--cap-add SYS_ADMIN --cap-add SYS_PTRACE ${podmanRunOptions} "
        "--volume /nix/store:/nix/store:ro "
        "--volume ${underTest}/bin:/opt/diskgc-e2e/bin:ro "
        "${image.imageName}:${image.imageTag} "
        "/opt/diskgc-e2e/bin/diskgc-e2e run "
        f"{case}"
      )
  '';
} // driverAttrs)
