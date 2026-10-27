#!/usr/bin/env python3
"""Build a small newc archive from a static BusyBox and the overlay bootstrap."""
import gzip
from pathlib import Path
import stat
import struct
import sys


def entry(output, number, name, mode, data=b''):
    encoded = name.encode() + b'\0'
    fields = (number, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(encoded), 0)
    header = b'070701' + b''.join(f'{field:08x}'.encode() for field in fields)
    output.extend(header + encoded)
    output.extend(b'\0' * (-len(output) % 4))
    output.extend(data)
    output.extend(b'\0' * (-len(output) % 4))


def build(busybox, init, target):
    # Reject dynamic executables: the archive deliberately contains no dynamic loader.
    binary = busybox.read_bytes()
    if binary[:5] != b'\x7fELF\x02':
        raise ValueError('BusyBox must be a static ELF64 executable')
    phoff = struct.unpack_from('<Q', binary, 32)[0]
    entsize, count = struct.unpack_from('<HH', binary, 54)
    if any(struct.unpack_from('<I', binary, phoff + i * entsize)[0] == 3 for i in range(count)):
        raise ValueError('BusyBox must be statically linked (PT_INTERP found)')
    archive = bytearray()
    entry(archive, 1, 'bin', stat.S_IFDIR | 0o755)
    entry(archive, 2, 'bin/busybox', stat.S_IFREG | 0o755, binary)
    entry(archive, 3, 'init', stat.S_IFREG | 0o755, init.read_bytes())
    entry(archive, 4, 'TRAILER!!!', 0)
    target.write_bytes(gzip.compress(archive, mtime=0))


if __name__ == '__main__':
    build(*map(Path, sys.argv[1:4]))
