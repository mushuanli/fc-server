#!/usr/bin/env python3
"""Enable the built-in shared-root profile while the server is stopped."""
import fcntl
from importlib.util import module_from_spec, spec_from_file_location
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

SPEC = spec_from_file_location('installer', Path(__file__).with_name('install-environment.py'))
installer = module_from_spec(SPEC)
SPEC.loader.exec_module(installer)
CONFIG = Path('/etc/mindos-firecracker/config.toml')


def updated(text, name):
    data = tomllib.loads(text)
    profile = data['profiles'][name]
    if profile['rootfs'] != '/var/lib/mindos-images/debian13.ext4':
        raise RuntimeError('shared-root command requires the built-in Debian image path')
    if profile['kernel'] != '/var/lib/mindos-images/vmlinux':
        raise RuntimeError('shared-root command requires the built-in PMEM/DAX kernel')
    pattern = rf'(?m)(^\[profiles\.{re.escape(name)}\]\s*\n)([\s\S]*?)(?=^\[|\Z)'
    def replace(match):
        body = re.sub(r'(?m)^\s*(rootfs_mode|initrd|upper_size_mib)\s*=.*\n?', '', match[2])
        options = 'rootfs_mode = "shared"\ninitrd = "/var/lib/mindos-images/debian13.ext4.initrd"\n'
        options += f'upper_size_mib = {profile.get("upper_size_mib", 2048)}\n'
        return match[1] + options + body
    result, count = re.subn(pattern, replace, text)
    if count != 1:
        raise RuntimeError('expected one explicit profile table; inspect config before migration')
    tomllib.loads(result)
    return result


def enable(name):
    installer.trusted(CONFIG)
    initrd = Path('/var/lib/mindos-images/debian13.ext4.initrd')
    installer.trusted(initrd)
    base = initrd.with_suffix('')
    installer.trusted(base)
    if base.stat().st_mode & 0o777 != 0o444:
        raise RuntimeError('shared base requires mode 0444; deploy current artifacts first')
    text = CONFIG.read_text()
    result = updated(text, name)
    backup = CONFIG.with_name('config.before-shared-root.toml')
    if not backup.exists():
        installer.atomic_file(backup, 0o600, data=text)
    installer.atomic_file(CONFIG, 0o600, data=result)
    print(f'[shared-root] profile={name} stage=config_write result=success')


if __name__ == '__main__':
    if os.geteuid() != 0:
        raise SystemExit('shared-root configuration requires root')
    with open('/run/mindos-firecracker.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if subprocess.run(['systemctl', 'is-active', '--quiet', 'firecracker-server']).returncode == 0:
            raise RuntimeError('stop firecracker-server before changing shared-root configuration')
        enable(sys.argv[1] if len(sys.argv) > 1 else 'debian13')
