#!/usr/bin/env python3
"""Record build inputs and distinguish disk capacity, used blocks and host space."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def ssh_key():
    value = os.environ.get('SSH_PUBLIC_KEY')
    path = Path(value) if value else Path.home() / '.ssh/mindos-firecracker.pub'
    if value == '' or not path.exists():
        if value:
            raise FileNotFoundError(f'SSH public key unavailable: {path}')
        return b''
    data = path.read_bytes()
    lines = [line.split() for line in data.decode().splitlines() if line.strip()]
    allowed = {'ssh-ed25519', 'ssh-rsa', 'ecdsa-sha2-nistp256', 'ecdsa-sha2-nistp384', 'ecdsa-sha2-nistp521'}
    if not lines or any(len(line) < 2 or line[0] not in allowed for line in lines):
        raise ValueError('SSH_PUBLIC_KEY must contain public keys, never private key material')
    return data.rstrip() + b'\n'


def inputs(repo, size, flavor):
    digest = hashlib.sha256()
    files = [p for p in (repo / 'guest').rglob('*') if p.is_file()
             and not {'node_modules', '__pycache__'}.intersection(p.parts)]
    files += [repo / 'scripts/build-debian13.sh', repo / 'scripts/build-initramfs.py']
    for path in sorted(files):
        digest.update(str(path.relative_to(repo)).encode() + b'\0' + path.read_bytes())
    arguments = {name: os.environ.get(name, '') for name in (
        'DEBIAN_MIRROR', 'DEBIAN_SECURITY_MIRROR', 'NODE_IMAGE', 'NPM_REGISTRY')}
    return {'schema': 1, 'flavor': flavor, 'size_mib': int(size),
            'recipe_sha256': digest.hexdigest(),
            'ssh_public_key_sha256': hashlib.sha256(ssh_key()).hexdigest(),
            'build_args_sha256': hashlib.sha256(json.dumps(arguments, sort_keys=True).encode()).hexdigest()}


def filesystem(image):
    result = subprocess.run(['/usr/sbin/dumpe2fs', '-h', str(image)], check=True,
                            capture_output=True, text=True, env={**os.environ, 'LC_ALL': 'C'})
    values = dict(line.split(':', 1) for line in result.stdout.splitlines() if ':' in line)
    block_size = int(values['Block size'])
    total, free = (int(values[key]) for key in ('Block count', 'Free blocks'))
    return {'capacity_bytes': image.stat().st_size,
            'host_allocated_bytes': image.stat().st_blocks * 512,
            'filesystem_used_bytes': (total - free) * block_size,
            'filesystem_free_bytes': free * block_size}


def report(image, probe_file, docker_id):
    data = {'inputs': json.loads(Path(str(image) + '.inputs.json').read_text()),
            'docker_image_id': docker_id, 'size': filesystem(image),
            'probe': json.loads(probe_file.read_text())}
    Path(str(image) + '.json').write_text(json.dumps(data, indent=2, sort_keys=True) + '\n')
    print('[image] result=success ' + json.dumps(data['size'], sort_keys=True), file=sys.stderr)


if __name__ == '__main__':
    if sys.argv[1] == 'inputs':
        print(json.dumps(inputs(Path(sys.argv[2]), *sys.argv[3:5]), sort_keys=True))
    elif sys.argv[1] == 'report':
        report(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
    elif sys.argv[1] == 'ssh-key':
        sys.stdout.buffer.write(ssh_key())
    else:
        raise SystemExit('usage: image-info.py inputs REPO SIZE_MIB FLAVOR | report IMAGE PROBE_JSON DOCKER_ID')
