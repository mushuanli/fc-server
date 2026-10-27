#!/usr/bin/env python3
"""Read-only checks; real boot behavior is validated by build.sh smoke."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib

from importlib.util import module_from_spec, spec_from_file_location


def check(condition, message):
    if not condition:
        raise RuntimeError(message)
    print(f"[check] ok: {message}")


def command(arguments):
    subprocess.run(arguments, check=True, stdout=subprocess.DEVNULL, timeout=15)


def installed_config():
    # Reuse the installer's trust checks without executing its deployment entrypoint.
    spec = spec_from_file_location("installer", Path(__file__).with_name("install-environment.py"))
    installer = module_from_spec(spec)
    spec.loader.exec_module(installer)
    config = Path("/etc/mindos-firecracker/config.toml")
    installer.trusted(config)
    data = tomllib.loads(config.read_text())
    paths = [config, Path("/usr/local/bin/firecracker-server"), Path(data["state_dir"])]
    paths += [Path(data[name]) for name in ("firecracker", "jailer", "virtiofsd")]
    if data.get('port_publisher'):
        paths.append(Path(data['port_publisher']))
        check(shutil.which('nft') is not None and shutil.which('conntrack') is not None,
              'nftables and conntrack for port publication')
    for profile in data["profiles"].values():
        paths += [Path(profile[name]) for name in ("kernel", "rootfs")]
        if profile.get('initrd'):
            paths.append(Path(profile['initrd']))
    for path in paths:
        check(path.exists(), f"installed path exists: {path}")
        installer.trusted(path)
    for binding in data.get("bindings", {}).values():
        path = Path(binding["source"])
        check(path.is_dir() and path.resolve() == path, f"canonical binding: {path}")
    environment = Path("/etc/mindos-firecracker/environment")
    installer.trusted(environment)
    check(environment.stat().st_mode & 0o777 == 0o600, "API key file permissions 0600")
    values = dict(line.split("=", 1) for line in environment.read_text().splitlines()
                  if line and not line.startswith("#"))
    key = values.get(data["api_key_env"], "")
    check(len(key) >= 32 and all(33 <= ord(c) <= 126 for c in key), "API key configured")
    help_text = subprocess.check_output([data["virtiofsd"], "--help"], text=True, timeout=15)
    check("--readonly" in help_text and "--sandbox" in help_text, "virtiofsd capabilities")


def host():
    check(os.geteuid() == 0, "root privileges")
    check(Path("/dev/kvm").is_char_device(), "/dev/kvm available")
    descriptor = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
    os.close(descriptor)
    controllers = Path("/sys/fs/cgroup/cgroup.controllers").read_text().split()
    check({"cpu", "memory", "pids"}.issubset(controllers), "cgroup v2 cpu/memory/pids")
    check(Path("/sys/fs/cgroup/cgroup.kill").exists() or
          tuple(map(int, os.uname().release.split("-")[0].split(".")[:2])) >= (5, 14),
          "Linux 5.14+ cgroup.kill support (root cgroup has no kill file)")
    for tool in ("bwrap", "unshare", "mkfs.ext4", "debugfs", "systemctl"):
        check(shutil.which(tool) is not None, f"host tool: {tool}")
    command(["unshare", "--mount", "--pid", "--net", "--fork", "true"])
    command(["bwrap", "--unshare-pid", "--unshare-net", "--ro-bind", "/", "/", "--", "/usr/bin/true"])
    print("[check] namespace probes passed; run ./build.sh smoke for real KVM acceptance")


if __name__ == "__main__":
    try:
        host()
        installed_config()
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"[check] result=failed reason={error}", file=sys.stderr)
        sys.exit(1)
