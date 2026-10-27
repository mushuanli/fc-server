#!/usr/bin/env python3
"""Test the deployed network with disposable VMs and no shared user data."""
from importlib.util import module_from_spec, spec_from_file_location
import json
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request

SPEC = spec_from_file_location('network_environment', Path(__file__).with_name('network-environment.py'))
network = module_from_spec(SPEC)
SPEC.loader.exec_module(network)


def api():
    config = tomllib.loads(network.CONFIG.read_text())
    values = dict(line.split('=', 1) for line in network.CONFIG.with_name('environment').read_text().splitlines()
                  if line and not line.startswith('#'))
    key = values[config['api_key_env']].strip().strip('"').strip("'")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def request(method, path, data=None):
        headers = {'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'}
        req = urllib.request.Request('http://' + config['listen'] + path,
            data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
        with opener.open(req, timeout=45) as response:
            return json.load(response)
    return request


def ssh(identity, known_hosts, vm, command, tty=False):
    address, alias = network.connection(vm)
    arguments = ['ssh', '-F', '/dev/null', '-i', str(identity), '-tt' if tty else '-T',
        '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'ConnectTimeout=3',
        '-o', 'StrictHostKeyChecking=accept-new', '-o', f'UserKnownHostsFile={known_hosts}',
        '-o', f'HostKeyAlias={alias}', f'agent@{address}', command]
    return subprocess.run(arguments, capture_output=True, text=True, timeout=30)


def wait_ssh(identity, known_hosts, vm):
    started = time.monotonic()
    while time.monotonic() - started < 60:
        result = ssh(identity, known_hosts, vm, 'test "$(id -u)" = 1000 && echo NETWORK_SSH_READY')
        if result.returncode == 0 and 'NETWORK_SSH_READY' in result.stdout:
            return round((time.monotonic() - started) * 1000)
        time.sleep(0.25)
    raise RuntimeError(f'SSH readiness failed for {vm}: {result.stderr[-600:]}')


def check_ssh(identity, known_hosts, vm):
    result = ssh(identity, known_hosts, vm, 'test -t 0 && echo SSH_PTY_OK', tty=True)
    assert result.returncode == 0 and 'SSH_PTY_OK' in result.stdout, result.stderr
    result = ssh(identity, known_hosts, vm, 'ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub')
    assert result.returncode == 0, result.stderr
    return result.stdout.split()[1]


def check_network(identity, known_hosts, vm, other_ip):
    code = 'import socket,urllib.request; '
    code += 'print("DNS_OK="+socket.gethostbyname("example.com")); '
    code += 'print("HTTPS_OK="+str(urllib.request.urlopen("https://example.com",timeout=15).status))'
    result = ssh(identity, known_hosts, vm, f"python3 -c '{code}'")
    assert result.returncode == 0 and 'HTTPS_OK=200' in result.stdout, result.stderr
    print(result.stdout.strip(), flush=True)
    targets = private_targets(other_ip)
    code = 'import socket\n'
    code += f'for host in {targets!r}:\n'
    code += ' s=socket.socket(); s.settimeout(1)\n'
    code += ' try:\n  s.connect((host,22)); raise RuntimeError("private target reachable: "+host)\n'
    code += ' except (TimeoutError,OSError): pass\n'
    code += ' finally: s.close()\n'
    code += 'print("PRIVATE_DESTINATIONS_BLOCKED")\n'
    result = ssh(identity, known_hosts, vm, 'python3 - <<\'PY\'\n' + code + 'PY')
    assert result.returncode == 0 and 'PRIVATE_DESTINATIONS_BLOCKED' in result.stdout, result.stderr


def private_targets(other_ip):
    plan = json.loads(network.PLAN.read_text())
    addresses = json.loads(network.run('/usr/sbin/ip', '-j', '-4', 'address', 'show',
                                      'dev', plan['egress_interface']).stdout)
    host = next(info['local'] for link in addresses for info in link['addr_info'] if info['family'] == 'inet')
    routes = json.loads(network.run('/usr/sbin/ip', '-j', '-4', 'route', 'show', 'default').stdout)
    return [host, routes[0]['gateway'], '169.254.169.254', other_ip]


def main():
    identity = Path(sys.argv[1]).resolve()
    request, created = api(), []
    nonce = secrets.token_hex(4)
    evidence = []
    with tempfile.TemporaryDirectory(prefix='fc-network-test-') as fixture:
        known_hosts = Path(fixture) / 'known_hosts'
        try:
            for label in ('a', 'b'):
                vm = f'network-{label}-{nonce}'
                request('POST', '/v1/vms', {'id': vm, 'profile': 'debian13'})
                created.append(vm)
                ready = wait_ssh(identity, known_hosts, vm)
                evidence.append({'vm': vm, 'ip': network.connection(vm)[0], 'ssh_ready_ms': ready,
                    'host_key': check_ssh(identity, known_hosts, vm)})
            assert evidence[0]['host_key'] != evidence[1]['host_key'], 'VMs share an SSH host key'
            check_network(identity, known_hosts, created[0], evidence[1]['ip'])
            print(json.dumps({'ssh': True, 'pty': True, 'dns': True, 'https': True,
                'privateDestinationsBlocked': True, 'uniqueHostKeys': True, 'vms': evidence}), flush=True)
        finally:
            for vm in reversed(created):
                result = request('DELETE', '/v1/vms/' + vm)
                assert result['mode'] == 'guest-shutdown', result


if __name__ == '__main__':
    main()
