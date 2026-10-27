#!/usr/bin/env python3
"""Apply owned nftables publications; the server serializes each VM's lifecycle."""
import fcntl
from importlib.util import module_from_spec, spec_from_file_location
import ipaddress
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time

SPEC = spec_from_file_location('network', Path(__file__).with_name('network-environment.py'))
network = module_from_spec(SPEC)
SPEC.loader.exec_module(network)
RUNTIME = Path('/run/mindos-firecracker-publish')
LEDGER = RUNTIME / 'ports.json'
PREFIX = 0xfc000000


class Failure(Exception):
    def __init__(self, code):
        self.code = code
        super().__init__(code)


def specification(value):
    if set(value) != {'listen', 'host_port', 'guest_port', 'protocol'}:
        raise Failure('INVALID_PUBLICATION')
    address = ipaddress.IPv4Address(value['listen'])
    for name in ('host_port', 'guest_port'):
        if type(value[name]) is not int or not 1 <= value[name] <= 65535:
            raise Failure('INVALID_PORT')
    if value['protocol'] not in ('tcp', 'udp'):
        raise Failure('INVALID_PROTOCOL')
    return {**value, 'listen': str(address)}


def guard_tables():
    result = network.run('/usr/sbin/nft', 'list', 'set', 'inet', 'mindos_firecracker', 'published_marks', check=False)
    if result.returncode != 0:
        raise Failure('PUBLICATION_NETWORK_NOT_READY')


def slot_for(request, plan):
    index = request['slot']
    if type(index) is not int or not 0 <= index < len(plan['slots']):
        raise Failure('PUBLICATION_NETWORK_MISMATCH')
    item = plan['slots'][index]
    expected = {'netns': '/run/netns/' + item['namespace'], 'tap': item['tap'], 'guest_mac': item['guest_mac']}
    if any(request.get(key) != value for key, value in expected.items()):
        raise Failure('PUBLICATION_NETWORK_MISMATCH')
    state = json.loads(network.STATE.read_text())
    if item not in state['slots']:
        raise Failure('PUBLICATION_NETWORK_NOT_READY')
    return item


def destination(record):
    if record['listen'] == '0.0.0.0':
        return 'fib daddr type local'
    return f'ip daddr {record["listen"]}'


def root_rules(records, plan):
    rules = 'flush set inet mindos_firecracker published_marks\n'
    for chain in ('published_output', 'published_prerouting', 'published_snat'):
        rules += f'flush chain ip mindos_firecracker_nat {chain}\n'
    if records:
        rules += 'add element inet mindos_firecracker published_marks { ' + ', '.join(str(r['token']) for r in records) + ' }\n'
    for record in records:
        item = plan['slots'][record['slot']]
        proto, host, guest, token = (record[k] for k in ('protocol', 'host_port', 'guest_port', 'token'))
        match = f'{destination(record)} {proto} dport {host}'
        action = f'ct mark set {token} dnat to {item["guest_ip"]}:{guest}\n'
        rules += f'add rule ip mindos_firecracker_nat published_output {match} {action}'
        if not ipaddress.IPv4Address(record['listen']).is_loopback:
            rules += f'add rule ip mindos_firecracker_nat published_prerouting ip daddr != 127.0.0.0/8 {match} {action}'
        rules += f'add rule ip mindos_firecracker_nat published_snat ct mark {token} oifname "{item["host_interface"]}" ip daddr {item["guest_ip"]} {proto} dport {guest} snat to {item["host_ip"]}\n'
    return rules


def namespace_rules(records, item):
    rules = 'flush chain inet mindos_firecracker published_forward\n'
    ports = {(r['protocol'], r['guest_port']) for r in records if r['slot'] == item['index']}
    for proto, port in sorted(ports):
        rules += f'add rule inet mindos_firecracker published_forward iifname "{item["peer_interface"]}" ip saddr {item["host_ip"]} ip daddr {item["guest_ip"]} {proto} dport {port} accept\n'
    return rules


def nft(rules, namespace=None):
    try:
        network.run('/usr/sbin/nft', '--check', '-f', '-', namespace=namespace, data=rules)
        network.run('/usr/sbin/nft', '-f', '-', namespace=namespace, data=rules)
    except subprocess.CalledProcessError as error:
        reason = next((line.split('Error:', 1)[1].strip() for line in error.stderr.splitlines() if 'Error:' in line), 'nft command rejected')
        print(json.dumps({'operation_id': globals().get('request', {}).get('operation_id'),
            'stage': 'nft_namespace' if namespace else 'nft_host', 'reason': reason[:240], 'result': 'failed'}), file=sys.stderr)
        raise Failure('NFT_PUBLICATION_APPLY_FAILED') from error


def apply(records, plan, slots=None):
    for item in plan['slots']:
        if slots is None or item['index'] in slots:
            nft(namespace_rules(records, item), item['namespace'])
    nft(root_rules(records, plan))


def purge(mark, namespace=None):
    arguments = ['/usr/sbin/conntrack', '-D', '-f', 'ipv4', '--mark', str(mark)]
    conntrack_delete(arguments, namespace)


def conntrack_delete(arguments, namespace=None):
    result = network.run(*arguments, namespace=namespace, check=False)
    if result.returncode != 0 and '0 flow entries' not in result.stderr:
        raise Failure('PUBLICATION_CONNTRACK_FAILED')


def purge_listener(record):
    addresses = [record['listen']]
    if record['listen'] == '0.0.0.0':
        interfaces = json.loads(network.run('/usr/sbin/ip', '-j', '-4', 'addr').stdout)
        addresses = ['127.0.0.0/8'] + [a['local'] for i in interfaces for a in i['addr_info'] if a['family'] == 'inet']
    for address in set(addresses):
        conntrack_delete(['/usr/sbin/conntrack', '-D', '-f', 'ipv4', '-p', record['protocol'],
            '--orig-dst', address, '--dport', str(record['host_port'])])


def flush_slot(item):
    network.run('/usr/sbin/conntrack', '-F', '-f', 'ipv4', namespace=item['namespace'])


def transaction(old, new, plan, flush=()):
    if old == new:
        for item in flush:
            flush_slot(item)
        return
    changed = changed_slots(old, new)
    network.save(LEDGER, {'records': old, 'pending': new})
    try:
        apply(new, plan, changed)
        purge_changes(old, new)
        for item in flush:
            flush_slot(item)
        network.save(LEDGER, {'records': new})
    except Exception:
        try:
            apply(old, plan, changed)
            for record in new:
                if record['token'] not in {r['token'] for r in old}:
                    purge(record['token'])
            network.save(LEDGER, {'records': old})
        except Exception:
            nft('flush set inet mindos_firecracker published_marks\n')
        raise


def changed_slots(old, new):
    slots = {r['slot'] for r in old + new}
    return {i for i in slots if [r for r in old if r['slot'] == i] != [r for r in new if r['slot'] == i]}


def purge_changes(old, new):
    current = {r['token'] for r in new}
    for record in old:
        if record['token'] not in current:
            purge(record['token'])
    prior = {r['token'] for r in old}
    for record in new:
        if record['token'] not in prior:
            purge_listener(record)


def recover(data, plan):
    if 'pending' not in data:
        return data['records']
    old, pending = data['records'], data['pending']
    apply(old, plan)
    for record in pending:
        if record['token'] not in {r['token'] for r in old}:
            purge(record['token'])
    network.save(LEDGER, {'records': old})
    return old


def add(records, request):
    spec = specification(request['spec'])
    for record in records:
        same_port = (record['protocol'], record['host_port']) == (spec['protocol'], spec['host_port'])
        overlap = record['listen'] == spec['listen'] or '0.0.0.0' in (record['listen'], spec['listen'])
        if same_port and overlap:
            if all(record.get(k) == request.get(k) for k in ('id', 'instance', 'slot')) and all(record[k] == spec[k] for k in spec):
                return records
            raise Failure('HOST_PORT_IN_USE')
    token = PREFIX | secrets.randbelow(1 << 24)
    while token in {r['token'] for r in records}:
        token = PREFIX | secrets.randbelow(1 << 24)
    return records + [{**spec, 'id': request['id'], 'instance': request['instance'], 'slot': request['slot'], 'token': token}]


def change(request, records, plan):
    item = slot_for(request, plan)
    own = lambda r: (r['id'], r['instance']) == (request['id'], request['instance'])
    if request['action'] == 'add':
        new = add(records, request)
    elif request['action'] == 'remove':
        spec = request['spec']
        new = [r for r in records if not (own(r) and r['host_port'] == spec['host_port'] and r['protocol'] == spec['protocol'])]
    elif request['action'] == 'clear':
        new = [r for r in records if not own(r)]
    else:
        raise Failure('INVALID_ACTION')
    transaction(records, new, plan, [item] if request['action'] == 'clear' else [])


def execute(request):
    network.installer.directory(RUNTIME, 0o700)
    with open(RUNTIME / 'lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        data = json.loads(LEDGER.read_text()) if LEDGER.exists() else {'records': []}
        if request['action'] == 'reset' and not network.PLAN.exists() and not data.get('records'):
            return
        guard_tables()
        plan = json.loads(network.PLAN.read_text())
        if request['action'] == 'reset':
            all_records = {r['token']: r for r in data.get('records', []) + data.get('pending', [])}
            transaction(list(all_records.values()), [], plan, plan['slots'])
        else:
            change(request, recover(data, plan), plan)


if __name__ == '__main__':
    started = time.monotonic()
    request = {'action': 'reset', 'operation_id': secrets.token_hex(16)} if sys.argv[1:] == ['reset'] else json.load(sys.stdin)
    code = None
    errno = None
    try:
        if os.geteuid() != 0:
            raise Failure('PUBLICATION_REQUIRES_ROOT')
        execute(request)
    except Exception as error:
        code = error.code if isinstance(error, Failure) else 'PUBLICATION_HOST_IO'
        errno = getattr(error, 'errno', None)
    print(json.dumps({'operation_id': request.get('operation_id'), 'operation': 'port_publication',
        'stage': request.get('action'), 'result': 'failed' if code else 'success', 'category': code, 'errno': errno,
        'elapsed_ms': int((time.monotonic()-started)*1000)}), file=sys.stderr)
    print(json.dumps({'code': code}))
    sys.exit(1 if code else 0)
