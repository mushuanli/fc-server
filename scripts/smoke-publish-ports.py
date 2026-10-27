#!/usr/bin/env python3
"""Real VM TCP/UDP publication and lifecycle isolation acceptance."""
from importlib.util import module_from_spec, spec_from_file_location
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

SPEC = spec_from_file_location('network_smoke', Path(__file__).with_name('smoke-network.py'))
network_smoke = module_from_spec(SPEC)
SPEC.loader.exec_module(network_smoke)
network = network_smoke.network

SERVER = '''import http.server,socket,sys,threading
marker=sys.argv[1].encode()
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(marker)
 def log_message(self,*args): pass
for port in (8080,8081):
 threading.Thread(target=http.server.ThreadingHTTPServer(('0.0.0.0',port),Handler).serve_forever,daemon=True).start()
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind(('0.0.0.0',8080))
while True:
 data,peer=s.recvfrom(4096);s.sendto(marker+b':'+data,peer)
'''


def call(*arguments):
    return subprocess.run(list(map(str, arguments)), check=True, capture_output=True, text=True, timeout=30)


def start_vm(request, identity, hosts, vm, marker):
    request('POST', '/v1/vms', {'id': vm, 'profile': 'debian13'})
    network_smoke.wait_ssh(identity, hosts, vm)
    command = "cat > /home/agent/publication-probe.py <<'PY'\n" + SERVER + 'PY\n'
    command += f'nohup python3 /home/agent/publication-probe.py {marker} >/home/agent/publication-probe.log 2>&1 </dev/null &\n'
    command += 'echo PROBE_STARTED\n'
    result = network_smoke.ssh(identity, hosts, vm, command)
    assert result.returncode == 0 and 'PROBE_STARTED' in result.stdout, result.stderr


def publish(request, vm, port, protocol='tcp', listen='127.0.0.1'):
    spec = {'listen': listen, 'host_port': port, 'guest_port': 8080, 'protocol': protocol}
    result = request('POST', f'/v1/vms/{vm}/ports', spec)
    assert result == spec, result
    return spec


def cli_publish(vm, port, protocol='tcp', listen='127.0.0.1'):
    script = Path(__file__).resolve().parents[1] / 'build.sh'
    result = json.loads(call(script, 'publish', vm, f'{port}:8080/{protocol}', '--listen', listen).stdout)
    assert result['listen'] == listen and result['protocol'] == protocol, result


def http(address, port):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(f'http://{address}:{port}/', timeout=2) as response:
        return response.read().decode()


def udp(address, port, client=None):
    own = client is None
    client = client or socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        client.settimeout(1)
        client.sendto(b'ping', (address, port))
        return client.recv(4096).decode()
    finally:
        if own:
            client.close()


def denied(action):
    try:
        action()
    except (OSError, urllib.error.URLError):
        return
    raise AssertionError('unexpected reachable endpoint')


def expect_error(action, code):
    try:
        action()
    except urllib.error.HTTPError as error:
        assert error.code == 409, error.code
        assert json.load(error)['error'] == code
        return
    raise AssertionError('expected publication rejection')


def external_client(namespace, host, peer):
    call('ip', 'netns', 'add', namespace)
    call('ip', 'link', 'add', host, 'type', 'veth', 'peer', 'name', peer)
    call('ip', 'link', 'set', peer, 'netns', namespace)
    call('ip', 'addr', 'add', '198.18.255.1/30', 'dev', host)
    call('ip', 'link', 'set', host, 'up')
    call('ip', '-n', namespace, 'addr', 'add', '198.18.255.2/30', 'dev', peer)
    call('ip', '-n', namespace, 'link', 'set', peer, 'up')
    call('ip', '-n', namespace, 'link', 'set', 'lo', 'up')


def ingress(namespace, wildcard, loopback, marker):
    code = f'''import socket,urllib.request
opener=urllib.request.build_opener(urllib.request.ProxyHandler({{}}))
assert opener.open('http://198.18.255.1:{wildcard}/',timeout=2).read().decode()=={marker!r}
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.settimeout(2)
s.sendto(b'ingress',('198.18.255.1',{wildcard}));assert s.recv(4096).decode()=={(marker+':ingress')!r}
try: opener.open('http://198.18.255.1:{loopback}/',timeout=1);raise AssertionError('loopback exposed')
except OSError: pass
print('INGRESS_TCP_UDP_AND_LOOPBACK_ISOLATION_OK')
'''
    return call('ip', 'netns', 'exec', namespace, 'python3', '-c', code).stdout.strip()


def firewall_isolation(identity, hosts, vm, peer_ip, public_port):
    denied(lambda: http(peer_ip, 8081))
    code = f'''import socket
s=socket.socket();s.settimeout(1)
try: s.connect(('10.204.1.1',{public_port}));raise AssertionError('guest accessed host publication')
except OSError: pass
print('GUEST_PUBLICATION_ACCESS_BLOCKED')
'''
    result = network_smoke.ssh(identity, hosts, vm, "python3 - <<'PY'\n" + code + 'PY')
    assert result.returncode == 0 and 'GUEST_PUBLICATION_ACCESS_BLOCKED' in result.stdout, result.stdout


def collisions(request, vm, wildcard_port):
    expect_error(lambda: publish(request, vm, wildcard_port), 'HOST_PORT_IN_USE')
    with socket.socket() as occupied:
        occupied.bind(('0.0.0.0', 0))
        occupied.listen()
        expect_error(lambda: publish(request, vm, occupied.getsockname()[1]), 'HOST_PORT_IN_USE')
    response = request('GET', f'/v1/vms/{vm}/ports')
    assert len(response['ports']) == 2, response


def unpublish(request, vm, port):
    for protocol in ('tcp', 'udp'):
        call(Path(__file__).resolve().parents[1] / 'build.sh', 'unpublish', vm, f'{port}/{protocol}')
    assert request('GET', f'/v1/vms/{vm}/ports')['ports'] == []
    denied(lambda: http('127.0.0.1', port))
    denied(lambda: udp('127.0.0.1', port))


def reuse(request, identity, hosts, vm, port, client):
    old_ip = network.connection(vm)[0]
    assert request('DELETE', '/v1/vms/' + vm)['mode'] == 'guest-shutdown'
    denied(lambda: udp('127.0.0.1', port, client))
    start_vm(request, identity, hosts, vm, 'recreated')
    assert network.connection(vm)[0] == old_ip, 'test did not reuse an IP slot'
    assert request('GET', f'/v1/vms/{vm}/ports')['ports'] == []
    denied(lambda: http('127.0.0.1', port))
    denied(lambda: udp('127.0.0.1', port, client))
    publish(request, vm, port)
    publish(request, vm, port, 'udp')
    assert http('127.0.0.1', port) == 'recreated'
    assert udp('127.0.0.1', port, client) == 'recreated:ping'


def exercise(request, identity, hosts, vms, namespace, first, second, client):
    for protocol in ('tcp', 'udp'):
        publish(request, vms[0], first, protocol)
        cli_publish(vms[1], second, protocol, '0.0.0.0')
    publish(request, vms[0], first)
    assert http('127.0.0.1', first) == 'first'
    assert udp('127.0.0.1', first, client) == 'first:ping'
    assert http('127.0.0.1', second) == 'second'
    assert udp('127.0.0.1', second) == 'second:ping'
    print(ingress(namespace, second, first, 'second'), flush=True)
    collisions(request, vms[0], second)
    firewall_isolation(identity, hosts, vms[0], network.connection(vms[0])[0], second)
    unpublish(request, vms[1], second)
    reuse(request, identity, hosts, vms[0], first, client)
    print(json.dumps({'tcp': True, 'udp': True, 'wildcardIngress': True, 'loopbackPrivate': True,
        'hostPortConflicts': True, 'unpublish': True, 'sameVmIdAndIpReuseIsolated': True}), flush=True)


def main():
    if os.geteuid() != 0:
        raise RuntimeError('run as root for disposable network namespace and API credentials')
    identity = Path(sys.argv[1]).resolve()
    request = network_smoke.api()
    nonce = secrets.token_hex(3)
    vms = ['publish-a-' + nonce, 'publish-b-' + nonce]
    namespace, host, peer = 'fc-pub-test-' + nonce, 'fcp' + nonce + 'h', 'fcp' + nonce + 'p'
    first, second = 18080, 18081
    created = []
    with tempfile.TemporaryDirectory(prefix='fc-publish-') as directory, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        hosts = Path(directory) / 'known_hosts'
        try:
            external_client(namespace, host, peer)
            for vm, marker in zip(vms, ('first', 'second')):
                created.append(vm)
                start_vm(request, identity, hosts, vm, marker)
            exercise(request, identity, hosts, vms, namespace, first, second, client)
        finally:
            for vm in reversed(created):
                assert request('DELETE', '/v1/vms/' + vm)['mode'] in ('guest-shutdown', 'absent')
            call('ip', 'link', 'delete', host)
            call('ip', 'netns', 'delete', namespace)
    if sys.argv[2:] == ['--crash']:
        crash_test(identity)


def wait_stopped():
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        result = subprocess.run(['systemctl', 'is-active', 'firecracker-server'], capture_output=True, text=True)
        if result.stdout.strip() not in ('active', 'deactivating', 'activating'):
            return
        time.sleep(0.1)
    raise RuntimeError('server stop/ExecStopPost did not complete')


def wait_api(request):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            request('GET', '/v1/health')
            return
        except urllib.error.URLError:
            time.sleep(0.1)
    raise RuntimeError('server did not become ready after restart')


def clean_crashed_fixture(vm):
    import tomllib
    config = tomllib.loads(network.CONFIG.read_text())
    root = Path(config['state_dir']) / Path(config['firecracker']).name / vm
    group = Path('/sys/fs/cgroup/mindos-firecracker') / vm
    if group.exists():
        assert 'populated 1' not in (group / 'cgroup.events').read_text(), 'fixture VMM survived server death'
        group.rmdir()
    if root.exists():
        shutil.rmtree(root)


def crash_cycle(request, identity, hosts, vm, port, client):
    rows = request('GET', '/v1/vms')['vms']
    assert [row['id'] for row in rows] == [vm], 'refusing to interrupt unrelated VMs'
    call('systemctl', 'kill', '--kill-whom=main', '--signal=SIGKILL', 'firecracker-server')
    wait_stopped()
    ledger = json.loads(Path('/run/mindos-firecracker-publish/ports.json').read_text())
    assert ledger == {'records': []}, ledger
    denied(lambda: http('127.0.0.1', port))
    denied(lambda: udp('127.0.0.1', port, client))
    clean_crashed_fixture(vm)
    call('systemctl', 'reset-failed', 'firecracker-server')
    call('systemctl', 'start', 'firecracker-server')
    wait_api(request)
    start_vm(request, identity, hosts, vm, 'after-crash')
    assert request('GET', f'/v1/vms/{vm}/ports')['ports'] == []
    denied(lambda: udp('127.0.0.1', port, client))
    cli_publish(vm, port)
    cli_publish(vm, port, 'udp')
    assert http('127.0.0.1', port) == 'after-crash'
    assert udp('127.0.0.1', port, client) == 'after-crash:ping'


def crash_test(identity):
    request = network_smoke.api()
    assert not request('GET', '/v1/vms')['vms'], '--crash requires an idle server'
    vm = 'publish-crash-' + secrets.token_hex(3)
    with tempfile.TemporaryDirectory(prefix='fc-pub-crash-') as directory, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        hosts = Path(directory) / 'known_hosts'
        try:
            start_vm(request, identity, hosts, vm, 'before-crash')
            publish(request, vm, 18082)
            publish(request, vm, 18082, 'udp')
            assert udp('127.0.0.1', 18082, client) == 'before-crash:ping'
            crash_cycle(request, identity, hosts, vm, 18082, client)
            print(json.dumps({'serverSigkillCleanup': True, 'crashIpReuseIsolated': True}), flush=True)
        finally:
            active = subprocess.run(['systemctl', 'is-active', '--quiet', 'firecracker-server']).returncode == 0
            if not active:
                wait_stopped()
                clean_crashed_fixture(vm)
                call('systemctl', 'start', 'firecracker-server')
            wait_api(request)
            request('DELETE', '/v1/vms/' + vm)


if __name__ == '__main__':
    main()
