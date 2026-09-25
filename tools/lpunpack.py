#!/usr/bin/env python3
"""Minimal lpunpack: extract logical partitions from an Android super image
(LP metadata v10). The input may also be a GPT disk image holding a `super`
partition (the Android Emulator's system.img).

    lpunpack.py <super.img|disk.img> <outdir> [name ...]"""
import os, struct, sys

SECTOR = 512
LP_RESERVED = 4096
GEOMETRY_SIZE = 4096


def super_offset(f):
    """Byte offset of the `super` GPT partition, or 0 for a bare super image."""
    f.seek(512)
    hdr = f.read(92)
    if hdr[:8] != b'EFI PART':
        return 0
    lba, n, sz = struct.unpack('<QII', hdr[72:88])
    f.seek(lba * SECTOR)
    for _ in range(n):
        e = f.read(sz)
        if e[56:128].decode('utf-16le').rstrip('\0') == 'super':
            return struct.unpack('<Q', e[32:40])[0] * SECTOR
    sys.exit('GPT image without a super partition')


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    src, out = sys.argv[1], sys.argv[2]
    want = set(sys.argv[3:])
    f = open(src, 'rb')
    base = super_offset(f)
    f.seek(base + LP_RESERVED)
    geo = f.read(GEOMETRY_SIZE)
    magic, struct_size = struct.unpack_from('<II', geo, 0)
    assert magic == 0x616c4467, 'not a super image (geometry magic)'
    metadata_max_size, slot_count, logical_block_size = struct.unpack_from('<III', geo, 40)
    f.seek(base + LP_RESERVED + 2 * GEOMETRY_SIZE)  # primary metadata, slot 0
    hdr = f.read(256)
    magic, major, minor, header_size = struct.unpack_from('<IHHI', hdr, 0)
    assert magic == 0x414c5030, 'bad metadata header'
    tables_size = struct.unpack_from('<I', hdr, 44)[0]
    # table descriptors: partitions, extents, groups, block_devices (offset, num, entry_size)
    descs = [struct.unpack_from('<III', hdr, 80 + 12 * i) for i in range(4)]
    f.seek(base + LP_RESERVED + 2 * GEOMETRY_SIZE + header_size)
    tables = f.read(tables_size)
    parts = []
    off, num, esz = descs[0]
    for i in range(num):
        e = tables[off + i * esz: off + (i + 1) * esz]
        name = e[:36].rstrip(b'\0').decode()
        attrs, first_ext, num_ext, group = struct.unpack_from('<IIII', e, 36)
        parts.append((name, first_ext, num_ext))
    off, num, esz = descs[1]
    exts = [struct.unpack_from('<QIQI', tables, off + i * esz) for i in range(num)]
    os.makedirs(out, exist_ok=True)
    for name, fe, ne in parts:
        size = sum(exts[fe + j][0] for j in range(ne)) * SECTOR
        print(f'{name:24} {size:>12} bytes, {ne} extent(s)')
        if want and name not in want:
            continue
        if size == 0:
            continue
        with open(os.path.join(out, name + '.img'), 'wb') as o:
            for j in range(ne):
                nsec, ttype, tdata, tdev = exts[fe + j]
                if ttype == 0:  # LINEAR
                    f.seek(base + tdata * SECTOR)
                    left = nsec * SECTOR
                    while left:
                        b = f.read(min(left, 1 << 24))
                        o.write(b)
                        left -= len(b)
                else:  # ZERO
                    o.seek(nsec * SECTOR, 1)
            o.truncate()


if __name__ == '__main__':
    main()
