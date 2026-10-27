#!/usr/bin/env python3
"""Explicit real-KVM test; never substitutes a mock or skips unavailable backends."""
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def sudo(*args):
    return subprocess.run(["sudo", "-n", *map(str, args)], check=True, capture_output=True)


def group_processes(group):
    return sudo("cat", group / "cgroup.procs").stdout.decode().strip()


def root_exists(path):
    return subprocess.run(["sudo", "-n", "test", "-d", str(path)], capture_output=True).returncode == 0


def request(address, key, method, path, body=None, expected=200, extra_headers=None):
    headers = {"Authorization": f"Bearer {key}"}
    headers.update(extra_headers or {})
    if body is not None:
        headers["Content-Type"] = "application/json"
        body = json.dumps(body).encode()
    req = urllib.request.Request(address + path, body, headers, method=method)
    try:
        response = urllib.request.urlopen(req, timeout=40)
    except urllib.error.HTTPError as error:
        data = error.read()
        assert error.code == expected, (path, error.code, data.decode())
        return json.loads(data) if error.headers.get_content_type() == "application/json" else data.decode()
    with response:
        assert response.status == expected, (path, response.status)
        data = response.read()
        return json.loads(data) if data else None


def provision(binary, kernel, image, root, fixture, port, vmm=None, jailer=None):
    for name in ["bin", "images", "state"]:
        sudo("install", "-d", "-m", "0700", root / name)
    for source, name in [(binary, "server"), (jailer or shutil.which("jailer"), "jailer"),
                         (vmm or shutil.which("firecracker"), "firecracker")]:
        sudo("install", "-m", "0755", source, root / "bin" / name)
    for source, name in [(kernel, "vmlinux"), (image, "debian13.ext4")]:
        sudo("cp", "--reflink=auto", "--sparse=always", source, root / "images" / name)
        # Deployment publishes the shared base read-only; private mode must still copy it writable.
        sudo("chmod", "0444" if name == "debian13.ext4" else "0600", root / "images" / name)
    config = configuration(root, fixture, port)
    (fixture / "config.toml").write_text(config)
    sudo("install", "-m", "0600", fixture / "config.toml", root / "config.toml")


def configuration(root, fixture, port):
    return f'''listen = "127.0.0.1:{port}"
api_key_env = "FIRECRACKER_API_KEY"
jailer = "{root}/bin/jailer"
firecracker = "{root}/bin/firecracker"
state_dir = "{root}/state"
uid_base = 230000
max_vms = 2
[profiles.debian13]
kernel = "{root}/images/vmlinux"
rootfs = "{root}/images/debian13.ext4"
vcpus = 2
memory_mib = 512
max_lifetime_secs = 300
startup_timeout_secs = 20
max_connections = 2
ports = [7000]
bindings = ["project", "reference"]
[bindings.project]
source = "{fixture}/source"
target = "/workspace/project"
read_only = false
size_mib = 64
[bindings.reference]
source = "{fixture}/source"
target = "/workspace/reference"
read_only = true
size_mib = 64
'''


def wait_ready(address, key, process):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("server exited before listening; inspect the private stderr log")
        try:
            return request(address, key, "GET", "/v1/health")
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise RuntimeError("server readiness timed out")


def guest(binary, address, vm, env, command, expected, port=7000):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        result = subprocess.run([binary, "connect", address, vm, str(port)],
                                input=command + "exit\n", text=True, capture_output=True, env=env, timeout=15)
        if result.returncode == 0 and expected in result.stdout:
            return result.stdout
        time.sleep(0.25)
    raise RuntimeError(f"guest evidence missing: status={result.returncode}, stdoutBytes={len(result.stdout)}, stderr={result.stderr[:300]}")


def exercise(binary, address, key, env, vm):
    request(address, "wrong", "GET", "/v1/health", expected=401)
    request(address, key, "POST", "/v1/vms", {"id": vm, "profile": "debian13", "command": "sh"}, 422)
    for name in ["project", "reference"]:
        request(address, key, "POST", "/v1/volumes", {"id": name, "binding": name})
    spec = {"id": vm, "profile": "debian13", "volumes": ["project", "reference"]}
    request(address, key, "POST", "/v1/vms", spec)
    assert request(address, key, "POST", "/v1/vms", spec)["id"] == vm
    request(address, key, "POST", f"/v1/vms/{vm}/connect/7001", expected=403,
            extra_headers={"Connection": "upgrade", "Upgrade": "mindos-vsock"})
    request(address, key, "POST", "/v1/vms", {**spec, "id": vm + "-busy"}, 409)
    command = "grep -q '^VERSION_ID=\"13\"' /etc/os-release && test $(id -u) = 1000 && "
    command += "test -r /workspace/project/probe && printf durable > /workspace/project/marker && "
    command += "if touch /workspace/reference/denied 2>/dev/null; then echo RO_FAILED; else echo DEBIAN13_RW_RO_OK; fi\n"
    guest(binary, address, vm, env, command, "DEBIAN13_RW_RO_OK")
    assert request(address, key, "DELETE", f"/v1/vms/{vm}")["mode"] == "guest-shutdown"
    request(address, key, "POST", "/v1/vms", spec)
    guest(binary, address, vm, env, "test \"$(cat /workspace/project/marker)\" = durable && echo PERSISTED\n", "PERSISTED")
    assert request(address, key, "DELETE", f"/v1/vms/{vm}")["mode"] == "guest-shutdown"
    export_disk(address, key)
    return spec


def export_disk(address, key):
    req = urllib.request.Request(address + "/v1/volumes/project/disk",
                                 headers={"Authorization": f"Bearer {key}"})
    with urllib.request.urlopen(req, timeout=20) as response:
        assert int(response.headers["Content-Length"]) == 64 * 1024 * 1024
        first = response.read(4096)
        assert first[1080:1082] == b"\x53\xef", "missing ext4 superblock"
        while response.read(1024 * 1024):
            pass


def crash_check(address, key, process_id, spec):
    request(address, key, "POST", "/v1/vms", spec)
    group = Path("/sys/fs/cgroup/mindos-firecracker") / spec["id"]
    assert group_processes(group), "VMM not in the resource cgroup"
    sudo("kill", "-KILL", process_id)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if not group_processes(group):
            return
        time.sleep(0.1)
    raise RuntimeError("VMM survived supervisor parent death")


def supervisor_crash(address, key, spec, process_id, kind="__supervise"):
    request(address, key, "POST", "/v1/vms", spec)
    script = "import pathlib, sys\npid, kind = sys.argv[1:]\n"
    script += "children = {child for task in pathlib.Path(f'/proc/{pid}/task').glob('*/children') for child in task.read_text().split()}\n"
    script += "for child in children:\n args = pathlib.Path(f'/proc/{child}/cmdline').read_bytes().split(b'\\0')\n if kind.encode() in args: print(child)\n"
    pids = sudo("python3", "-c", script, process_id, kind).stdout.decode().split()
    assert len(pids) == 1, "expected one fixture supervisor"
    sudo("kill", "-KILL", pids[0])
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if not request(address, key, "GET", "/v1/vms")["vms"]:
            return
        time.sleep(0.1)
    raise RuntimeError("supervisor death left the VM registered")


def cleanup(process, root, vm):
    if process and process.poll() is None:
        sudo("kill", "-TERM", process.pid)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            raise RuntimeError("server did not stop; leaving test artifacts for inspection")
    group = Path("/sys/fs/cgroup/mindos-firecracker") / vm
    if root_exists(group):
        assert not group_processes(group), "refusing to delete a live VM"
        sudo("rmdir", group)
    sudo("rm", "-rf", "--", root)


def main():
    binary, kernel, image = map(lambda p: Path(p).resolve(), sys.argv[1:4])
    port = int(os.environ.get("FIRECRACKER_TEST_PORT", "18790"))
    key = secrets.token_hex(32)
    env = {**os.environ, "FIRECRACKER_API_KEY": key}
    address = f"http://127.0.0.1:{port}"
    root = Path(sudo("mktemp", "-d", "/var/lib/fc-test.XXXXXXXX").stdout.decode().strip())
    vm = "smoke-" + secrets.token_hex(4)
    process = None
    with tempfile.TemporaryDirectory(prefix="fc-fixture-") as directory:
        fixture = Path(directory)
        (fixture / "source").mkdir()
        (fixture / "source" / "probe").write_text("fixture")
        try:
            provision(binary, kernel, image, root, fixture, port)
            with (fixture / "stderr.log").open("wb") as log:
                process = subprocess.Popen(["sudo", "-n", "--preserve-env=FIRECRACKER_API_KEY",
                    str(root / "bin/server"), "serve", str(root / "config.toml")], env=env, stderr=log)
                health = wait_ready(address, key, process)
                spec = exercise(str(binary), address, key, env, vm)
                supervisor_crash(address, key, spec, health["processId"])
                crash_check(address, key, health["processId"], spec)
                process.wait(timeout=15)
                assert key not in (fixture / "stderr.log").read_text()
            print(json.dumps({"debian13": True, "readWrite": True, "readOnly": True,
                "persistentVolume": True, "supervisorDeathCleanup": True,
                "parentDeathCleanup": True, "credentialLogLeak": False}))
        except Exception:
            if (fixture / "stderr.log").exists():
                descriptor, name = tempfile.mkstemp(prefix="fc-smoke-failure-", suffix=".log")
                os.close(descriptor)
                shutil.copyfile(fixture / "stderr.log", name)
                print(f"Private diagnostic log: {name}", file=sys.stderr)
            raise
        finally:
            cleanup(process, root, vm)


if __name__ == "__main__":
    main()
