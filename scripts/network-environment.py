#!/usr/bin/env python3
"""Provision owned per-slot namespaces; preserve unrelated routes and firewall tables."""
from importlib.util import module_from_spec, spec_from_file_location
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request

CONFIG = Path('/etc/mindos-firecracker/config.toml')
PLAN = CONFIG.with_name('network.json')
RUNTIME = Path('/run/mindos-firecracker-network')
STATE = RUNTIME / 'state.json'
INSTALL = Path('/usr/local/libexec/mindos-firecracker')
BLOCKED = ('0.0.0.0/8', '10.0.0.0/8', '100.64.0.0/10', '127.0.0.0/8',
           '169.254.0.0/16', '172.16.0.0/12', '192.0.0.0/24', '192.0.2.0/24',
           '192.168.0.0/16', '198.18.0.0/15', '198.51.100.0/24', '203.0.113.0/24',
           '224.0.0.0/4', '240.0.0.0/4')
SPEC = spec_from_file_location('installer', Path(__file__).with_name('install-environment.py'))
installer = module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


def run(*arguments, namespace=None, data=None, check=True):
    command = list(map(str, arguments))
    if namespace:
        command = ['/usr/sbin/ip', 'netns', 'exec', namespace, *command]
    return subprocess.run(command, input=data, text=True, capture_output=True, check=check)


def save(path, data, mode=0o600):
    installer.atomic_file(path, mode, data=data if isinstance(data, str) else json.dumps(data, indent=2) + '\n')


def api_request(method, path, data=None):
    config = tomllib.loads(CONFIG.read_text())
    values = dict(line.split('=', 1) for line in CONFIG.with_name('environment').read_text().splitlines()
                  if line and not line.startswith('#'))
    key = values[config['api_key_env']].strip().strip('"').strip("'")
    request = urllib.request.Request('http://' + config['listen'] + path, method=method,
        data=None if data is None else json.dumps(data).encode(),
        headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=45) as response:
        return json.load(response)


def service_vms():
    try:
        return api_request('GET', '/v1/vms')['vms']
    except urllib.error.HTTPError:
        raise RuntimeError('cannot authenticate to the running service to check active VMs')
    except urllib.error.URLError as error:
        if getattr(error.reason, 'errno', None) == 111:
            return []
        raise RuntimeError('cannot determine whether VMs are active') from error


def require_empty():
    if service_vms():
        raise RuntimeError('active VMs exist; stop them before changing or stopping their network')


def slot(index, guest, uplink):
    g = ipaddress.ip_network(f'{guest.network_address + index * 256}/30')
    u = ipaddress.ip_network(f'{uplink.network_address + index * 256}/30')
    return {'index': index, 'namespace': f'mindos-fc-{index}', 'tap': 'fc-tap',
            'host_interface': f'mfc-h{index}', 'peer_interface': f'mfc-n{index}',
            'guest_subnet': str(g), 'uplink_subnet': str(u),
            'guest_ip': str(g[2]), 'gateway': str(g[1]),
            'host_ip': str(u[1]), 'peer_ip': str(u[2]),
            'guest_mac': f'02:fc:00:00:00:{index + 1:02x}'}


def dns_servers():
    servers = os.environ.get('FC_DNS_SERVERS', '').split(',')
    if servers == ['']:
        servers = re.findall(r'^nameserver\s+(\S+)', Path('/etc/resolv.conf').read_text(), re.M)
    result = []
    for server in servers:
        address = ipaddress.ip_address(server.strip())
        if address.version == 4 and address.is_global:
            result.append(str(address))
    if not result:
        raise RuntimeError('set FC_DNS_SERVERS to reachable public IPv4 DNS servers')
    return result


def make_plan():
    config = tomllib.loads(CONFIG.read_text())
    guest = ipaddress.ip_network(os.environ.get('FC_GUEST_NETWORK', '10.203.0.0/16'))
    uplink = ipaddress.ip_network(os.environ.get('FC_UPLINK_NETWORK', '10.204.0.0/16'))
    if guest.version != 4 or uplink.version != 4 or guest.prefixlen != 16 or uplink.prefixlen != 16:
        raise RuntimeError('guest and uplink networks must be private IPv4 /16 networks')
    if not guest.is_private or not uplink.is_private or guest.overlaps(uplink):
        raise RuntimeError('guest/uplink networks must be private and must not overlap')
    routes = json.loads(run('/usr/sbin/ip', '-j', '-4', 'route', 'show', 'default').stdout)
    interface = os.environ.get('FC_EGRESS_INTERFACE') or routes[0]['dev']
    if not re.fullmatch(r'[A-Za-z0-9_.:-]{1,15}', interface):
        raise RuntimeError('invalid egress interface')
    return {'schema': 1, 'egress_interface': interface, 'dns_servers': dns_servers(),
            'guest_network': str(guest), 'uplink_network': str(uplink),
            'slots': [slot(i, guest, uplink) for i in range(config['max_vms'])]}


def check_routes(plan):
    networks = [ipaddress.ip_network(plan[name]) for name in ('guest_network', 'uplink_network')]
    routes = json.loads(run('/usr/sbin/ip', '-j', '-4', 'route', 'show', 'table', 'all').stdout)
    for route in routes:
        destination = route.get('dst', 'default')
        if destination == 'default' or route.get('dev', '').startswith('mfc-h'):
            continue
        if any(ipaddress.ip_network(destination).overlaps(network) for network in networks):
            raise RuntimeError(f'network overlaps existing route: {destination}')


def configure_slots(config, plan):
    text = config.read_text()
    if re.search(r'^\[\[profiles\.[^.]+\.network_slots\]\]', text, re.M):
        if PLAN.exists() and json.loads(PLAN.read_text()) == plan:
            return
        raise RuntimeError('network_slots already configured; inspect them before replacing')
    if re.search(r'^\s*network_slots\s*=', text, re.M):
        raise RuntimeError('network_slots already configured inline; inspect them before replacing')
    data = tomllib.loads(text)
    for name in data['profiles']:
        for item in plan['slots']:
            text += f'\n[[profiles.{name}.network_slots]]\n'
            text += f'tap = "{item["tap"]}"\nnetns = "/run/netns/{item["namespace"]}"\n'
            text += f'guest_mac = "{item["guest_mac"]}"\n'
    backup = config.with_name('config.before-network.toml')
    if not backup.exists():
        installer.atomic_file(backup, 0o600, source=config)
    save(config, text)


def install():
    require_empty()
    installer.trusted(CONFIG)
    plan = make_plan()
    check_routes(plan)
    if PLAN.exists() and json.loads(PLAN.read_text()) != plan:
        raise RuntimeError('network plan already exists with different settings; inspect it first')
    configure_slots(CONFIG, plan)
    configure_publisher()
    installer.directory(INSTALL)
    for name in ('network-environment.py', 'install-environment.py', 'publish-ports.py'):
        installer.atomic_file(INSTALL / name, 0o755, source=Path(__file__).with_name(name))
    save(PLAN, plan)
    install_dns(plan)
    save(Path('/etc/sysctl.d/90-mindos-firecracker.conf'), 'net.ipv4.ip_forward=1\n', 0o644)
    dns_units = ' '.join(f'mindos-firecracker-dns@{item["index"]}.service' for item in plan['slots'])
    unit = f'[Unit]\nDescription=MindOS microVM network namespaces\nWants=network-online.target {dns_units}\n'
    unit += 'After=network-online.target\nBefore=firecracker-server.service\n[Service]\nType=oneshot\n'
    unit += 'RemainAfterExit=yes\nUMask=0077\nTimeoutStopSec=30\n'
    unit += f'ExecStart={INSTALL}/network-environment.py up\nExecStop={INSTALL}/network-environment.py down\n'
    unit += '[Install]\nWantedBy=multi-user.target\n'
    save(Path('/etc/systemd/system/mindos-firecracker-network.service'), unit, 0o644)
    dropin = Path('/etc/systemd/system/firecracker-server.service.d')
    installer.directory(dropin)
    save(dropin / 'network.conf', f'[Unit]\nRequires=mindos-firecracker-network.service {dns_units}\nAfter=mindos-firecracker-network.service {dns_units}\n', 0o644)
    run('/usr/bin/systemctl', 'daemon-reload')
    print(json.dumps({'network': 'installed', 'slots': len(plan['slots']), 'egress': plan['egress_interface']}))


def configure_publisher():
    text = CONFIG.read_text()
    if 'port_publisher' not in tomllib.loads(text):
        backup = CONFIG.with_name('config.before-publish.toml')
        if not backup.exists():
            save(backup, text)
        save(CONFIG, f'port_publisher = "{INSTALL}/publish-ports.py"\n' + text)


def blocked_set():
    return 'set blocked { type ipv4_addr; flags interval; elements = { ' + ', '.join(BLOCKED) + ' }; }\n'


def host_rules(plan):
    interface = plan['egress_interface']
    rules = 'table inet mindos_firecracker {\n' + blocked_set()
    rules += 'set published_marks { type mark; }\n'
    rules += 'chain published_guard { ct mark @published_marks ct status dnat accept\nct mark & 0xff000000 == 0xfc000000 drop\n}\n'
    rules += 'chain published_forward { }\n'
    rules += 'chain output { type filter hook output priority 0; policy accept; oifname "mfc-h*" jump published_guard\n}\n'
    rules += 'chain input { type filter hook input priority -20; policy accept;\n'
    rules += 'iifname "mfc-h*" jump published_guard\n'
    rules += 'iifname "mfc-h*" ct state established,related accept\niifname "mfc-h*" drop\n}\n'
    rules += 'chain forward { type filter hook forward priority -20; policy accept;\n'
    for item in plan['slots']:
        rules += f'iifname "{item["host_interface"]}" ip saddr != {{ {item["guest_ip"]}, {item["peer_ip"]} }} drop\n'
    rules += 'iifname "mfc-h*" jump published_guard\noifname "mfc-h*" jump published_guard\n'
    rules += 'iifname "mfc-h*" ip daddr @blocked drop\n'
    rules += f'iifname "mfc-h*" oifname "{interface}" meta nfproto ipv4 accept\n'
    rules += f'iifname "{interface}" oifname "mfc-h*" ct state established,related accept\n'
    rules += 'iifname "mfc-h*" drop\noifname "mfc-h*" drop\n}\n}\n'
    rules += 'table ip mindos_firecracker_nat {\n'
    rules += 'chain published_prerouting { }\nchain published_output { }\nchain published_snat { }\n'
    rules += 'chain prerouting { type nat hook prerouting priority dstnat; policy accept; iifname != "mfc-h*" jump published_prerouting\n}\n'
    rules += 'chain output { type nat hook output priority dstnat; policy accept; jump published_output\n}\n'
    rules += 'chain postrouting { type nat hook postrouting priority srcnat; policy accept; jump published_snat\n'
    rules += f'iifname "mfc-h*" oifname "{interface}" masquerade\n}}\n}}\n'
    return rules


def namespace_rules(item):
    tap, peer = item['tap'], item['peer_interface']
    rules = 'table inet mindos_firecracker {\n' + blocked_set()
    rules += 'chain input { type filter hook input priority 0; policy drop;\niifname "lo" accept\n'
    rules += 'ct state established,related accept\n'
    rules += f'iifname "{tap}" udp dport 67 accept\n'
    rules += f'iifname "{tap}" ip saddr {item["guest_ip"]} ip daddr {item["gateway"]} udp dport 53 accept\n'
    rules += f'iifname "{tap}" ip saddr {item["guest_ip"]} ip daddr {item["gateway"]} tcp dport 53 accept\n'
    rules += f'iifname "{tap}" ip daddr {item["gateway"]} icmp type echo-request accept\n}}\n'
    rules += 'chain published_forward { }\n'
    rules += 'chain forward { type filter hook forward priority 0; policy drop;\n'
    rules += f'iifname "{tap}" ip saddr != {item["guest_ip"]} drop\nct state established,related accept\n'
    rules += 'jump published_forward\n'
    rules += f'iifname "{peer}" ip saddr {item["host_ip"]} ip daddr {item["guest_ip"]} tcp dport 22 accept\n'
    rules += f'iifname "{tap}" ip daddr @blocked drop\n'
    rules += f'iifname "{tap}" oifname "{peer}" ip saddr {item["guest_ip"]} accept\n}}\n}}\n'
    return rules


def nft_apply(rules, namespace=None):
    prefix = ''
    for family, name in [('inet', 'mindos_firecracker'), ('ip', 'mindos_firecracker_nat')]:
        if run('/usr/sbin/nft', 'list', 'table', family, name, namespace=namespace, check=False).returncode == 0:
            prefix += f'delete table {family} {name}\n'
    run('/usr/sbin/nft', '-f', '-', namespace=namespace, data=prefix + rules)


def prepare_slot(item):
    ns, host, peer, tap = (item[key] for key in ('namespace', 'host_interface', 'peer_interface', 'tap'))
    run('/usr/sbin/ip', 'netns', 'add', ns)
    run('/usr/sbin/ip', 'link', 'add', host, 'type', 'veth', 'peer', 'name', peer)
    run('/usr/sbin/ip', 'link', 'set', host, 'alias', 'mindos-firecracker-network')
    run('/usr/sbin/ip', 'link', 'set', peer, 'netns', ns)
    run('/usr/sbin/ip', 'address', 'add', item['host_ip'] + '/30', 'dev', host)
    run('/usr/sbin/ip', 'link', 'set', host, 'up')
    run('/usr/sbin/sysctl', '-q', '-w', f'net.ipv4.conf.{host}.route_localnet=1')
    run('/usr/sbin/ip', 'address', 'add', item['peer_ip'] + '/30', 'dev', peer, namespace=ns)
    run('/usr/sbin/ip', 'link', 'set', peer, 'up', namespace=ns)
    run('/usr/sbin/ip', 'link', 'set', 'lo', 'up', namespace=ns)
    run('/usr/sbin/ip', 'tuntap', 'add', tap, 'mode', 'tap', namespace=ns)
    run('/usr/sbin/ip', 'address', 'add', item['gateway'] + '/30', 'dev', tap, namespace=ns)
    run('/usr/sbin/ip', 'link', 'set', tap, 'up', namespace=ns)
    run('/usr/sbin/ip', 'route', 'add', 'default', 'via', item['host_ip'], namespace=ns)
    run('/usr/sbin/ip', 'route', 'add', item['guest_subnet'], 'via', item['peer_ip'], 'dev', host)
    run('/usr/sbin/sysctl', '-q', '-w', 'net.ipv4.ip_forward=1', 'net.ipv6.conf.all.disable_ipv6=1', namespace=ns)
    nft_apply(namespace_rules(item), ns)


def install_dns(plan):
    directory = PLAN.with_name('network')
    installer.directory(directory, 0o700)
    for item in plan['slots']:
        options = ['no-resolv', 'bind-interfaces', 'interface=' + item['tap'], 'except-interface=lo',
                   'user=nobody', 'group=nogroup', 'leasefile-ro', 'dhcp-leasefile=/dev/null',
                   f'dhcp-range={item["guest_ip"]},{item["guest_ip"]},255.255.255.252,12h',
                   f'dhcp-host={item["guest_mac"]},{item["guest_ip"]}',
                   f'dhcp-option=option:router,{item["gateway"]}',
                   f'dhcp-option=option:dns-server,{item["gateway"]}']
        options += ['server=' + server for server in plan['dns_servers']]
        save(directory / f'dns-{item["index"]}.conf', '\n'.join(options) + '\n')
    unit = '[Unit]\nDescription=MindOS DHCP/DNS for VM slot %i\n'
    unit += 'Requires=mindos-firecracker-network.service\nAfter=mindos-firecracker-network.service\n'
    unit += 'PartOf=mindos-firecracker-network.service\nBefore=firecracker-server.service\n'
    unit += '[Service]\nType=simple\nRestart=on-failure\nRestartSec=1\n'
    unit += f'ExecStart=/usr/sbin/ip netns exec mindos-fc-%i /usr/sbin/dnsmasq --keep-in-foreground --conf-file={directory}/dns-%i.conf\n'
    save(Path('/etc/systemd/system/mindos-firecracker-dns@.service'), unit, 0o644)


def down():
    require_empty()
    if not STATE.exists():
        return
    state = json.loads(STATE.read_text())
    for item in reversed(state['slots']):
        run('/usr/sbin/ip', 'link', 'delete', item['host_interface'], check=False)
        run('/usr/sbin/ip', 'netns', 'delete', item['namespace'], check=False)
    for family, name in [('inet', 'mindos_firecracker'), ('ip', 'mindos_firecracker_nat')]:
        run('/usr/sbin/nft', 'delete', 'table', family, name, check=False)
    STATE.unlink()


def up():
    require_empty()
    plan = json.loads(PLAN.read_text())
    installer.directory(RUNTIME, 0o700)
    if STATE.exists():
        down()
    check_routes(plan)
    for family, name in [('inet', 'mindos_firecracker'), ('ip', 'mindos_firecracker_nat')]:
        if run('/usr/sbin/nft', 'list', 'table', family, name, check=False).returncode == 0:
            raise RuntimeError('firewall table exists without our ownership state: ' + name)
    for item in plan['slots']:
        if Path('/run/netns/' + item['namespace']).exists():
            raise RuntimeError('namespace already exists without our ownership state: ' + item['namespace'])
        if run('/usr/sbin/ip', 'link', 'show', item['host_interface'], check=False).returncode == 0:
            raise RuntimeError('network interface already exists: ' + item['host_interface'])
    state = {'slots': []}
    save(STATE, state)
    try:
        run('/usr/sbin/sysctl', '-q', '-w', 'net.ipv4.ip_forward=1')
        nft_apply(host_rules(plan))
        for item in plan['slots']:
            state['slots'].append(item)
            save(STATE, state)
            prepare_slot(item)
    except BaseException:
        down()
        raise


def connection(vm):
    if not re.fullmatch(r'[A-Za-z0-9-]{1,48}', vm):
        raise RuntimeError('invalid VM ID')
    config = tomllib.loads(CONFIG.read_text())
    root = Path(config['state_dir']) / Path(config['firecracker']).name / vm / 'root'
    uid = (root / 'rootfs.ext4').stat().st_uid
    index = uid - config['uid_base']
    plan = json.loads(PLAN.read_text())
    item = plan['slots'][index] if 0 <= index < len(plan['slots']) else None
    if item is None:
        raise RuntimeError('VM UID does not match a configured network slot')
    generation = (root / 'firecracker.pid').stat().st_mtime_ns
    return item['guest_ip'], f'mindos-{vm}-{generation}'


def vm_action(command, vm, directories):
    if not re.fullmatch(r'[A-Za-z0-9-]{1,48}', vm):
        raise RuntimeError('invalid VM ID')
    if command == 'start':
        print(json.dumps(api_request('POST', '/v1/vms', {
            'id': vm, 'profile': 'debian13', 'directories': directories})))
    else:
        print(json.dumps(api_request('DELETE', '/v1/vms/' + vm)))


def publication_action(command, arguments):
    import argparse
    parser = argparse.ArgumentParser(prog='build.sh ' + command)
    parser.add_argument('vm')
    if command != 'ports':
        parser.add_argument('mapping')
    if command == 'publish':
        parser.add_argument('--listen', default='127.0.0.1')
    options = parser.parse_args(arguments)
    if not re.fullmatch(r'[A-Za-z0-9-]{1,48}', options.vm):
        raise RuntimeError('invalid VM ID')
    path = '/v1/vms/' + options.vm + '/ports'
    if command == 'ports':
        result = api_request('GET', path)
    else:
        pattern = r'(\d+):(\d+)(?:/(tcp|udp))?' if command == 'publish' else r'(\d+)(?:/(tcp|udp))?'
        match = re.fullmatch(pattern, options.mapping)
        if not match:
            raise RuntimeError('expected HOST:GUEST[/tcp|udp]' if command == 'publish' else 'expected HOST[/tcp|udp]')
        protocol = match.groups()[-1] or 'tcp'
        if command == 'publish':
            result = api_request('POST', path, {'listen': str(ipaddress.IPv4Address(options.listen)),
                'host_port': int(match[1]), 'guest_port': int(match[2]), 'protocol': protocol})
        else:
            result = api_request('DELETE', path + f'/{protocol}/{int(match[1])}')
    print(json.dumps(result))


if __name__ == '__main__':
    try:
        if os.geteuid() != 0:
            raise RuntimeError('network management requires root')
        command = sys.argv[1]
        if command == 'install':
            install()
        elif command == 'up':
            up()
        elif command == 'down':
            down()
        elif command == 'address':
            print(*connection(sys.argv[2]))
        elif command == 'idle':
            require_empty()
        elif command in ('start', 'stop'):
            vm_action(command, sys.argv[2], sys.argv[3:])
        elif command in ('publish', 'unpublish', 'ports'):
            publication_action(command, sys.argv[2:])
        else:
            raise RuntimeError('usage: network-environment.py install|up|down|address VM_ID|idle')
    except (OSError, ValueError, KeyError, IndexError, RuntimeError, subprocess.CalledProcessError) as error:
        detail = getattr(error, 'stderr', '') or str(error)
        if isinstance(error, urllib.error.HTTPError):
            try:
                response = json.load(error)
                detail = f'HTTP {error.code} category={response.get("error", "API_REJECTED")} stage={response.get("stage", "request_validate")} operation_id={response.get("operationId", "unknown")}'
            except (ValueError, AttributeError):
                detail = f'HTTP {error.code}: request rejected; check VM ID, port range and protocol'
        print(f'[network] operation={sys.argv[1]} result=failed reason={detail}', file=sys.stderr)
        sys.exit(1)
