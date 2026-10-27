#!/usr/bin/env python3
"""Real KVM acceptance for the fork and the server; all writes use private fixtures."""
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

import smoke


def root_write(path, data):
    subprocess.run(["sudo", "-n", "tee", str(path)], input=data.encode(),
                   stdout=subprocess.DEVNULL, check=True)


def groups(vm):
    return [Path("/sys/fs/cgroup") / parent / vm
            for parent in ["mindos-firecracker", "mindos-firecracker-fs"]]


def assert_empty(vm):
    for group in groups(vm):
        if smoke.root_exists(group):
            assert not smoke.group_processes(group), f"live process remains in {group.name}"


def add_root_probe(root, fixture):
    # Only the disposable test copy gets a root port for adversarial remount tests.
    unit = """[Unit]
Requires=mindos-bindings.service
After=mindos-bindings.service
[Service]
ExecStart=/usr/bin/socat VSOCK-LISTEN:7001,reuseaddr,fork EXEC:/usr/local/libexec/harness-7000
[Install]
WantedBy=multi-user.target
"""
    (fixture / "root-probe.service").write_text(unit)
    commands = f"write {fixture}/root-probe.service /etc/systemd/system/root-probe.service\n"
    commands += "symlink /etc/systemd/system/multi-user.target.wants/root-probe.service /etc/systemd/system/root-probe.service\n"
    (fixture / "debugfs.txt").write_text(commands)
    smoke.sudo("/usr/sbin/debugfs", "-w", "-f", fixture / "debugfs.txt", root / "images/debian13.ext4")


def configure(root, fixture, port):
    config = smoke.configuration(root, fixture, port)
    config = f'virtiofsd = "{root}/bin/virtiofsd"\n' + config
    config = config.replace("size_mib = 64", 'backend = "virtiofs"')
    config = config.replace("ports = [7000]", "ports = [7000, 7001]")
    config = config.replace("startup_timeout_secs = 20", "startup_timeout_secs = 5")
    config = config.replace('bindings = ["project", "reference"]', 'bindings = ["project", "reference", "alias"]')
    config += f'\n[bindings.alias]\nsource = "{fixture}/alias"\ntarget = "/workspace/alias"\nread_only = false\nbackend = "virtiofs"\n'
    (fixture / "config.toml").write_text(config)
    smoke.sudo("install", "-m", "0600", fixture / "config.toml", root / "config.toml")


def start_server(root, env, log, address, key):
    process = subprocess.Popen(["sudo", "-n", "--preserve-env=FIRECRACKER_API_KEY",
        str(root / "bin/server"), "serve", str(root / "config.toml")], env=env, stderr=log)
    try:
        health = smoke.wait_ready(address, key, process)
    except BaseException:
        stop_server(process)
        raise
    return process, health["processId"]


def stop_server(process):
    if process and process.poll() is None:
        smoke.sudo("kill", "-TERM", process.pid)
        process.wait(timeout=30)


def timeout_case(root, fixture, address, key, env, log, vm):
    # The fake backend accepts a connection but never answers vhost-user requests.
    fake = "#!/usr/bin/python3\nimport socket, time\ns = socket.socket(socket.AF_UNIX)\n"
    fake += "s.bind('/socket/fs.sock')\ns.listen(1)\nc, _ = s.accept()\ntime.sleep(300)\n"
    (fixture / "unresponsive-daemon").write_text(fake)
    smoke.sudo("install", "-m", "0755", fixture / "unresponsive-daemon", root / "bin/virtiofsd")
    process, _ = start_server(root, env, log, address, key)
    try:
        started = time.monotonic()
        spec = {"id": vm, "profile": "debian13", "directories": ["project"]}
        error = smoke.request(address, key, "POST", "/v1/vms", spec, 503)
        assert error["stage"] == "vmm_start_timeout", error
        assert 5 <= time.monotonic() - started < 12, "startup deadline did not terminate the processes"
        assert_empty(vm)
        assert smoke.request(address, key, "GET", "/v1/vms")["vms"] == []
    finally:
        stop_server(process)


def share_fixture(fixture):
    source = fixture / "source"
    source.mkdir()
    (source / "probe").write_text("authorized")
    (source / "nested").mkdir()
    (source / "sessions").mkdir()
    (fixture / "alias").mkdir()
    smoke.sudo("mount", "--bind", source / "sessions", fixture / "alias")
    (source / "nested/public").write_text("underlying")
    (fixture / "private").mkdir()
    (fixture / "private/secret").write_text("outside the authorization")
    (source / "escape").symlink_to(fixture / "private/secret")
    (fixture / "replacement").mkdir()
    (fixture / "replacement/probe").write_text("unauthorized replacement")
    smoke.sudo("mount", "--bind", fixture / "private", source / "nested")


def image_probe(binary, address, vm, env):
    command = 'if test -x /usr/local/libexec/image-check; then '
    command += 'printf "IMAGE_INFO="; /usr/local/libexec/image-check; '
    command += 'else echo "IMAGE_INFO={}"; fi\n'
    output = smoke.guest(binary, address, vm, env, command, 'IMAGE_INFO=')
    return json.loads(next(line.removeprefix('IMAGE_INFO=') for line in output.splitlines()
                           if line.startswith('IMAGE_INFO=')))


def host_io(binary, address, vm, env, fixture):
    assert (fixture / "pinned/.codex/session").read_text() == "session-fixture"
    assert not (fixture / "replacement/.codex").exists()
    (fixture / "pinned/from-host").write_text("host-update")
    smoke.guest(binary, address, vm, env,
                'test "$(cat /workspace/project/from-host)" = host-update && echo LIVE_HOST_OK\n', "LIVE_HOST_OK")


def read_write(binary, address, key, env, vm, fixture, evidence):
    spec = {"id": vm, "profile": "debian13", "directories": ["project"]}
    started = time.monotonic()
    smoke.request(address, key, "POST", "/v1/vms", spec)
    evidence['vmm_running_ms'] = round((time.monotonic() - started) * 1000)
    smoke.request(address, key, "POST", "/v1/vms", {**spec, "id": vm + "-busy"}, 409)
    smoke.request(address, key, "POST", "/v1/vms", {**spec, "id": vm + "-alias", "directories": ["alias"]}, 409)
    command = "test $(id -u) = 1000 && test \"$(cat /workspace/project/probe)\" = authorized && "
    command += "test -r /workspace/project/nested/public && test ! -e /workspace/project/nested/secret && "
    command += "test ! -e /workspace/project/escape && mkdir -p /workspace/project/.codex && "
    command += "printf session-fixture > /workspace/project/.codex/pending && "
    command += "mv /workspace/project/.codex/pending /workspace/project/.codex/session && echo LIVE_RW_OK\n"
    smoke.guest(binary, address, vm, env, command, "LIVE_RW_OK")
    evidence['guest_ready_ms'] = round((time.monotonic() - started) * 1000)
    evidence['image'] = image_probe(binary, address, vm, env)
    host_io(binary, address, vm, env, fixture)
    assert smoke.request(address, key, "DELETE", f"/v1/vms/{vm}")["mode"] == "guest-shutdown"
    assert_empty(vm)
    assert smoke.request(address, key, "GET", "/v1/volumes")["volumes"] == []
    return spec


def read_only(binary, address, key, env, vm, fixture):
    spec = {"id": vm, "profile": "debian13", "directories": ["reference"]}
    smoke.request(address, key, "POST", "/v1/vms", spec)
    command = "mount -o remount,rw /workspace/reference && "
    command += "findmnt -no OPTIONS /workspace/reference | grep -q '^rw,' && "
    command += "! touch /workspace/reference/denied 2>/dev/null && "
    command += "! rm /workspace/reference/probe 2>/dev/null && "
    command += "! mv /workspace/reference/probe /workspace/reference/renamed 2>/dev/null && echo RO_BACKEND_OK\n"
    smoke.guest(binary, address, vm, env, command, "RO_BACKEND_OK", port=7001)
    assert (fixture / "pinned/probe").read_text() == "authorized"
    assert not (fixture / "pinned/denied").exists()
    assert smoke.request(address, key, "DELETE", f"/v1/vms/{vm}")["mode"] == "guest-shutdown"
    assert_empty(vm)


def backend_crash(address, key, spec):
    smoke.request(address, key, "POST", "/v1/vms", spec)
    root_write(groups(spec["id"])[1] / "cgroup.kill", "1")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if not smoke.request(address, key, "GET", "/v1/vms")["vms"]:
            assert_empty(spec["id"])
            return
        time.sleep(0.1)
    raise RuntimeError("backend crash did not reclaim the VM")


def parent_crash(address, key, spec, process_id):
    smoke.request(address, key, "POST", "/v1/vms", spec)
    assert all(smoke.group_processes(group) for group in groups(spec["id"]))
    smoke.sudo("kill", "-KILL", process_id)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if all(not smoke.group_processes(group) for group in groups(spec["id"])):
            return
        time.sleep(0.1)
    raise RuntimeError("parent death left backend or VMM processes")


def exercise(binary, root, fixture, daemon, address, key, env, vm, log):
    evidence = {}
    timeout_case(root, fixture, address, key, env, log, vm)
    smoke.sudo("install", "-m", "0755", daemon, root / "bin/virtiofsd")
    process, process_id = start_server(root, env, log, address, key)
    try:
        (fixture / "source").rename(fixture / "pinned")
        (fixture / "source").symlink_to(fixture / "replacement", target_is_directory=True)
        spec = read_write(binary, address, key, env, vm, fixture, evidence)
        read_only(binary, address, key, env, vm, fixture)
        backend_crash(address, key, spec)
        for kind in ["__supervise", "__supervise-fs"]:
            smoke.supervisor_crash(address, key, spec, process_id, kind)
            assert_empty(vm)
        parent_crash(address, key, spec, process_id)
        process.wait(timeout=15)
        return evidence
    finally:
        stop_server(process)


def cleanup(root, fixture, vm):
    for group in groups(vm):
        if smoke.root_exists(group):
            assert not smoke.group_processes(group), "refusing to delete a live sandbox"
            smoke.sudo("rmdir", group)
    source = fixture / ("pinned" if (fixture / "pinned").exists() else "source")
    if subprocess.run(["mountpoint", "-q", str(source / "nested")]).returncode == 0:
        smoke.sudo("umount", source / "nested")
    if subprocess.run(["mountpoint", "-q", str(fixture / "alias")]).returncode == 0:
        smoke.sudo("umount", fixture / "alias")
    smoke.sudo("rm", "-rf", "--", root)


def main():
    binary, kernel, image, vmm, jailer, daemon = [Path(p).resolve() for p in sys.argv[1:7]]
    port = int(os.environ.get("FIRECRACKER_TEST_PORT", "18791"))
    address, key = f"http://127.0.0.1:{port}", secrets.token_hex(32)
    env = {**os.environ, "FIRECRACKER_API_KEY": key}
    root = Path(smoke.sudo("mktemp", "-d", "/var/lib/fc-fs-test.XXXXXXXX").stdout.decode().strip())
    vm = "fs-" + secrets.token_hex(4)
    with tempfile.TemporaryDirectory(prefix="fc-fs-fixture-", dir="/var/tmp") as directory:
        fixture = Path(directory)
        try:
            share_fixture(fixture)
            smoke.provision(binary, kernel, image, root, fixture, port, vmm, jailer)
            configure(root, fixture, port)
            add_root_probe(root, fixture)
            with (fixture / "stderr.log").open("wb") as log:
                evidence = exercise(str(binary), root, fixture, daemon, address, key, env, vm, log)
            assert key not in (fixture / "stderr.log").read_text()
            print(json.dumps({"virtiofs": True, "liveReadWrite": True, "sourcePinned": True,
                "submountExcluded": True, "hostReadOnly": True, "startupDeadline": True,
                "backendCrashCleanup": True, "supervisorDeathCleanup": True,
                "parentDeathCleanup": True, "credentialLogLeak": False, "bootEvidence": evidence}))
        except BaseException:
            if (fixture / "stderr.log").exists():
                descriptor, name = tempfile.mkstemp(prefix="fc-fs-smoke-failure-", suffix=".log", dir="/var/tmp")
                os.close(descriptor)
                shutil.copyfile(fixture / "stderr.log", name)
                print(f"Private diagnostic log: {name}", file=sys.stderr)
            raise
        finally:
            cleanup(root, fixture, vm)


if __name__ == "__main__":
    main()
