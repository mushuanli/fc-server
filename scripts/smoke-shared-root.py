#!/usr/bin/env python3
"""Real KVM proof of shared file PFNs, DAX mmap and private OverlayFS writes."""
import json
import os
from pathlib import Path
import secrets
import shutil
import struct
import sys
import tempfile

import smoke
from importlib.util import module_from_spec, spec_from_file_location

SPEC = spec_from_file_location('fs_smoke', Path(__file__).with_name('smoke-virtiofs.py'))
fs_smoke = module_from_spec(SPEC)
SPEC.loader.exec_module(fs_smoke)

MAPPER = '''import ctypes,json,mmap,os,struct,time
f=open('/usr/local/bin/node','rb')
m=mmap.mmap(f.fileno(),0,flags=mmap.MAP_PRIVATE,prot=mmap.PROT_READ)
checksum=sum(m[::4096])
cow=mmap.mmap(f.fileno(),4096,flags=mmap.MAP_PRIVATE,prot=mmap.PROT_READ|mmap.PROT_WRITE)
original=m[0]
cow[0]=(original+1)%256
assert m[0]==original and cow[0]!=original
sections=open('/proc/self/smaps').read().split('\\n')
selected=False
flags=[]
for line in sections:
 if line and line[0] in '0123456789abcdef' and '-' in line.split()[0]:
  selected=line.endswith('/usr/local/bin/node')
 if selected and line.startswith('VmFlags:'): flags=line.split()[1:]
st=ctypes.create_string_buffer(256)
assert ctypes.CDLL(None).statx(-100,b'/.mindos/lower/usr/local/bin/node',0,0xfff,st)==0
dax=bool(struct.unpack_from('Q',st.raw,8)[0]&0x200000)
json.dump(dict(pid=os.getpid(),bytes=len(m),statxDax=dax,privateMmapWriteIsolated=True,vmFlags=flags,checksum=checksum),open('/tmp/dax-probe.json','w'))
time.sleep(1800)
'''

GUEST_PFNS = '''import glob,json,struct
d={'pid':PROBE_PID}
resources=glob.glob('/sys/bus/nd/devices/namespace*/resource')
ranges=[]
for r in resources:
 base=int(open(r).read().strip(),0)
 size=int(open(r.rsplit('/',1)[0]+'/size').read().strip(),0)
 ranges.append((base//4096,(base+size)//4096))
total=pmem=zero=private=0
flags=open('/proc/kpageflags','rb')
with open('/proc/%s/pagemap'%d['pid'],'rb') as f:
 for line in open('/proc/%s/maps'%d['pid']):
  if not line.rstrip().endswith('/usr/local/bin/node'): continue
  lo,hi=[int(x,16) for x in line.split()[0].split('-')]
  f.seek(lo//4096*8)
  for (v,) in struct.iter_unpack('<Q',f.read((hi-lo)//4096*8)):
   pfn=v&((1<<55)-1)
   if not v>>63: continue
   total+=1
   if any(lo<=pfn<hi for lo,hi in ranges): pmem+=1; continue
   flags.seek(pfn*8)
   bits=struct.unpack('<Q',flags.read(8))[0]
   zero+=bool(bits&(1<<24))
   private+=bool(bits&(1<<12))
print(json.dumps(dict(mappedPages=total,pmemPages=pmem,zeroPages=zero,privateCowPages=private,pmemRanges=ranges)))
assert pmem>0 and private==1 and pmem+zero+private==total,(total,pmem,zero,private,ranges)
print('GUEST_PFNS_OK')
'''


def inspect(pids):
    result, common, different = [], None, 0
    for pid in pids:
        lines = Path(f'/proc/{pid}/maps').read_text().splitlines()
        line = next(line for line in lines if line.endswith('/base.ext4'))
        assert line.split()[1] == 'r--s', 'base mapping must be shared and read-only'
        start, end = [int(value, 16) for value in line.split()[0].split('-')]
        with open(f'/proc/{pid}/pagemap', 'rb') as file:
            file.seek(start // 4096 * 8)
            entries = struct.unpack(f'<{(end-start)//4096}Q', file.read((end-start)//4096*8))
        pages = {index: value & ((1 << 55)-1) for index, value in enumerate(entries)
                 if value >> 63 and value & ((1 << 55)-1)}
        assert pages, 'physical PFNs unavailable: run with host root privileges'
        if common is None:
            common = pages
        else:
            offsets = set(common).intersection(pages)
            different += sum(common[i] != pages[i] for i in offsets)
            common = {i: common[i] for i in offsets if common[i] == pages[i]}
        result.append({'pid': pid, 'residentBaseBytes': len(pages)*4096})
    shared = len(common)*4096
    assert shared > 32*1024*1024, f'insufficient actual shared physical pages: {shared}'
    return {'vms': result, 'identicalPhysicalBaseBytes': shared,
            'differentPFNsAtCommonOffsets': different}


def configure(root, fixture, port, initrd, daemon, count):
    fs_smoke.add_root_probe(root, fixture)
    smoke.sudo('install', '-m', '0600', initrd, root / 'images/initrd')
    smoke.sudo('install', '-m', '0755', daemon, root / 'bin/virtiofsd')
    smoke.sudo('chmod', '0444', root / 'images/debian13.ext4')
    text = smoke.configuration(root, fixture, port)
    text = f'virtiofsd = "{root}/bin/virtiofsd"\n' + text
    text = text.replace('size_mib = 64', 'backend = "virtiofs"')
    text = text.replace('ports = [7000]', 'ports = [7000, 7001]')
    text = text.replace('max_vms = 2', f'max_vms = {count}')
    text = text.replace('max_lifetime_secs = 300', 'max_lifetime_secs = 1800')
    if count > 2:
        text = text.replace('memory_mib = 512', 'memory_mib = 256').replace('vcpus = 2', 'vcpus = 1')
    text = text.replace('rootfs = ', f'rootfs_mode = "shared"\ninitrd = "{root}/images/initrd"\nupper_size_mib = 128\nrootfs = ')
    (fixture / 'persistent').mkdir()
    text = text.replace('bindings = ["project", "reference"]', 'bindings = ["project", "reference", "state"]')
    text += f'\n[bindings.state]\nsource = "{fixture}/persistent"\ntarget = "/home/agent/.shared-state"\nread_only = false\nsize_mib = 64\n'
    (fixture / 'shared.toml').write_text(text)
    smoke.sudo('install', '-m', '0600', fixture / 'shared.toml', root / 'config.toml')


def mapping(binary, address, env, vm):
    command = "cat > /tmp/dax-probe.py <<'PY'\n" + MAPPER + 'PY\n'
    command += 'nohup python3 /tmp/dax-probe.py >/tmp/dax-probe.log 2>&1 </dev/null &\necho MAPPER_STARTED\n'
    smoke.guest(binary, address, vm, env, command, 'MAPPER_STARTED')
    output = smoke.guest(binary, address, vm, env,
        "test -s /tmp/dax-probe.json && cat /tmp/dax-probe.json && echo MAPPING_READY\n", 'MAPPING_READY')
    data = json.loads(output.split('MAPPING_READY')[0].strip())
    assert data['statxDax'], data
    output = smoke.guest(binary, address, vm, env,
        "python3 - 2>&1 <<'PY'\n" + GUEST_PFNS.replace('PROBE_PID', str(data['pid'])) + 'PY\n', 'GUEST_PFNS_OK', port=7001)
    data['guestPhysicalPages'] = json.loads(output.split('GUEST_PFNS_OK')[0].strip())
    return data


def private_writes(binary, address, env, vms):
    original = "sha256sum /home/agent/.bashrc | cut -d ' ' -f 1\n"
    before = [smoke.guest(binary, address, vm, env, original + 'echo HASH_OK\n', 'HASH_OK') for vm in vms]
    assert before[0] == before[1], 'initial base content differs'
    for vm, value in zip(vms, ('private-a', 'private-b')):
        smoke.guest(binary, address, vm, env,
            f"printf '{value}' > /home/agent/.bashrc; echo WRITE_OK\n", 'WRITE_OK')
    for vm, value in zip(vms, ('private-a', 'private-b')):
        smoke.guest(binary, address, vm, env,
            f'test "$(cat /home/agent/.bashrc)" = {value} && echo COPY_UP_ISOLATED\n', 'COPY_UP_ISOLATED')


def physical_evidence(root, vms):
    paths = [root / f'state/firecracker/{vm}/root/base.ext4' for vm in vms]
    stats = [smoke.sudo('stat', '-c', '%u:%a:%d:%i', path).stdout.strip() for path in paths]
    assert all(value.startswith(b'0:444:') for value in stats), 'base must stay root-owned and read-only'
    assert stats[0] == stats[1], 'base files must reference the very same inode'
    pids = []
    for vm in vms:
        group = Path('/sys/fs/cgroup/mindos-firecracker') / vm
        candidates = smoke.group_processes(group).split()
        assert len(candidates) == 1, candidates
        pids.append(int(candidates[0]))
    evidence = smoke.sudo('python3', Path(__file__).resolve(), '--inspect', *pids).stdout
    return json.loads(evidence)


def digest(path):
    return smoke.sudo('sha256sum', path).stdout.split()[0]


def boot_probes(binary, address, key, env, vms):
    for index, vm in enumerate(vms):
        available = int(next(line.split()[1] for line in Path('/proc/meminfo').read_text().splitlines()
                             if line.startswith('MemAvailable:')))
        if available < 4*1024*1024:
            raise RuntimeError('host has less than 4 GiB available; stop scale test and clean up')
        smoke.request(address, key, 'POST', '/v1/vms', {'id': vm, 'profile': 'debian13',
            'volumes': ['shared-state'] if index == 0 else [],
            'directories': ['project'] if index == 0 else []})
    probes = []
    for index, vm in enumerate(vms):
        probes.append(mapping(binary, address, env, vm))
        print(f'[shared-root] stage=dax_probe completed={index+1} total={len(vms)}', file=sys.stderr, flush=True)
    return probes


def exercise(binary, root, fixture, address, key, env, vms):
    base = root / 'images/debian13.ext4'
    before = digest(base)
    smoke.request(address, key, 'POST', '/v1/volumes', {'id': 'shared-state', 'binding': 'state'})
    probes = boot_probes(binary, address, key, env, vms)
    private_writes(binary, address, env, vms[:2])
    command = "printf guest-live > /workspace/project/guest-marker && printf durable > /home/agent/.shared-state/probe && echo VIRTIOFS_OK\n"
    smoke.guest(binary, address, vms[0], env, command, 'VIRTIOFS_OK')
    assert (fixture / 'source/guest-marker').read_text() == 'guest-live'
    evidence = physical_evidence(root, vms)
    assert evidence['differentPFNsAtCommonOffsets'] == 0, evidence
    # Guest root may try to overwrite PMEM. KVM's read-only memory slot must protect the host.
    command = "timeout 3 sh -c 'printf ATTACK | dd of=/dev/pmem0 bs=1 seek=4096 conv=notrunc status=none'; echo GUARD_ATTEMPTED\n"
    smoke.guest(binary, address, vms[0], env, command, 'GUARD_ATTEMPTED', port=7001)
    assert digest(base) == before, 'guest changed the shared host base image'
    recreate(binary, address, key, env, vms)
    evidence.update(daxMmap=probes, privateCopyUp=True, sharedBaseReadOnly=True, virtiofsLive=True, recreatedClean=True, persistentVolume=True)
    return evidence


def recreate(binary, address, key, env, vms):
    for vm in vms:
        assert smoke.request(address, key, 'DELETE', f'/v1/vms/{vm}')['mode'] == 'guest-shutdown'
    smoke.request(address, key, 'POST', '/v1/vms',
        {'id': vms[0], 'profile': 'debian13', 'volumes': ['shared-state']})
    smoke.guest(binary, address, vms[0], env,
        "! grep -q private-a /home/agent/.bashrc && test \"$(cat /home/agent/.shared-state/probe)\" = durable && echo RECREATED_CLEAN\n",
        'RECREATED_CLEAN')


def main():
    binary, kernel, image, initrd, vmm, jailer, daemon = map(lambda p: Path(p).resolve(), sys.argv[1:8])
    port = int(os.environ.get('FIRECRACKER_TEST_PORT', '18792'))
    address, key = f'http://127.0.0.1:{port}', secrets.token_hex(32)
    env = {**os.environ, 'FIRECRACKER_API_KEY': key}
    count = int(os.environ.get('FC_SHARED_TEST_VMS', '2'))
    if not 2 <= count <= 128:
        raise ValueError('FC_SHARED_TEST_VMS must be 2..128')
    root = Path(smoke.sudo('mktemp', '-d', '/var/lib/fc-shared-test.XXXXXXXX').stdout.decode().strip())
    vms = ['shared-' + secrets.token_hex(4) for _ in range(count)]
    process = None
    with tempfile.TemporaryDirectory(prefix='fc-shared-fixture-', dir='/var/tmp') as directory:
        fixture = Path(directory)
        (fixture / 'source').mkdir()
        try:
            smoke.provision(binary, kernel, image, root, fixture, port, vmm, jailer)
            configure(root, fixture, port, initrd, daemon, count)
            with (fixture / 'stderr.log').open('wb') as log:
                process, _ = fs_smoke.start_server(root, env, log, address, key)
                evidence = exercise(str(binary), root, fixture, address, key, env, vms)
                print(json.dumps(evidence), flush=True)
        except BaseException:
            save_failure(fixture)
            raise
        finally:
            cleanup(process, root, vms)


def cleanup(process, root, vms):
    smoke.cleanup(process, root, vms[0])
    for vm in vms[1:]:
        group = Path('/sys/fs/cgroup/mindos-firecracker') / vm
        if smoke.root_exists(group):
            assert not smoke.group_processes(group)
            smoke.sudo('rmdir', group)


def save_failure(fixture):
    failure = Path(tempfile.mkdtemp(prefix='fc-shared-failure-', dir='/var/tmp'))
    shutil.copytree(fixture, failure / 'fixture', dirs_exist_ok=True)
    print(f'Private diagnostic logs: {failure}', file=sys.stderr)


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--inspect':
        print(json.dumps(inspect(list(map(int, sys.argv[2:])))))
    else:
        main()
