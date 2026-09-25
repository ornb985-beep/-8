#!/usr/bin/env python3
"""Extract an ext4 image to a directory *with* its extended attributes
(security.selinux labels, security.capability), so `mke2fs -d` can rebuild
an equivalent, correctly labeled image. Needs debugfs (e2fsprogs) and root.

    ext4tree.py extract <image.img> <outdir>
    ext4tree.py elfscan <dir>...          # list non-AArch64 ELF files
    ext4tree.py label <dir> <relpath> <selinux-context>
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile


def debugfs(image, commands):
    """Run debugfs commands; returns stdout."""
    r = subprocess.run(['debugfs', '-f', '-', image], input='\n'.join(commands) + '\n',
                       capture_output=True, text=True, errors='surrogateescape')
    return r.stdout


def parse_value(raw):
    raw = raw.strip()
    if raw.startswith('"'):
        s = raw[1:raw.rindex('"')]
        out = bytearray()
        i = 0
        while i < len(s):
            c = s[i]
            if c == '\\' and i + 3 < len(s) + 0 and re.match(r'[0-7]{3}', s[i + 1:i + 4]):
                out.append(int(s[i + 1:i + 4], 8))
                i += 4
            elif c == '\\' and i + 1 < len(s):
                out.append(ord(s[i + 1]))
                i += 2
            else:
                out += c.encode('utf-8', 'surrogateescape')
                i += 1
        return bytes(out)
    return bytes(int(b, 16) for b in raw.split())


ATTR = re.compile(r'^\s+(\S+) \((\d+)\)(?: = (.*))?$')


def set_attr(out, path, name, val):
    target = out if path == '/' else os.path.join(out, path.lstrip('/'))
    os.setxattr(target, name, val, follow_symlinks=False)


def extract(image, out):
    os.makedirs(out, exist_ok=True)
    r = subprocess.run(['debugfs', '-R', f'rdump / {out}', image], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f'rdump failed: {r.stderr}')
    # rdump puts the root's children directly into <out>.
    paths = ['/']
    for root, dirs, files in os.walk(out):
        for n in dirs + files:
            paths.append('/' + os.path.relpath(os.path.join(root, n), out))
    cmds = []
    for p in paths:
        cmds.append(f'ea_list "{p}"')
    text = debugfs(image, cmds)
    cur, n = None, 0
    longs = []  # values debugfs does not print inline
    for line in text.splitlines():
        if line.startswith('debugfs: ea_list "'):
            cur = line[len('debugfs: ea_list "'):-1]
            continue
        m = ATTR.match(line)
        if cur is None or not m:
            continue
        name, size = m.group(1), int(m.group(2))
        if m.group(3) is None:
            longs.append((cur, name, size))
            continue
        val = parse_value(m.group(3))
        if len(val) != size:
            sys.exit(f'{cur}: {name}: parsed {len(val)} bytes, expected {size}')
        set_attr(out, cur, name, val)
        n += 1
    if longs:
        tmp = tempfile.mkdtemp()
        debugfs(image, [f'ea_get -f {tmp}/{i} "{p}" {name}' for i, (p, name, _) in enumerate(longs)])
        for i, (p, name, size) in enumerate(longs):
            with open(f'{tmp}/{i}', 'rb') as fh:
                val = fh.read()
            if len(val) != size:
                sys.exit(f'{p}: {name}: read {len(val)} bytes, expected {size}')
            set_attr(out, p, name, val)
            n += 1
        shutil.rmtree(tmp)
    print(f'{image}: {len(paths)} entries, {n} xattrs restored')


def elfscan(dirs):
    bad, bpf = [], []
    total = 0
    for d in dirs:
        for root, _, files in os.walk(d):
            for f in files:
                p = os.path.join(root, f)
                if os.path.islink(p) or not os.path.isfile(p):
                    continue
                with open(p, 'rb') as fh:
                    h = fh.read(20)
                if h[:4] != b'\x7fELF':
                    continue
                total += 1
                cls, machine = h[4], int.from_bytes(h[18:20], 'little')
                if cls == 2 and machine == 247:  # EM_BPF: kernel bpf programs (bpfloader)
                    bpf.append(p)
                elif cls != 2 or machine != 183:  # ELFCLASS64, EM_AARCH64
                    bad.append((p, 'ELF32' if cls == 1 else 'ELF64', machine))
    for p, c, m in bad:
        print(f'NON-AARCH64 {c} e_machine={m}: {p}')
    for p in bpf:
        print(f'eBPF object (loaded by the kernel, not executed): {p}')
    print(f'{total} ELF files: {total - len(bad) - len(bpf)} AArch64 64-bit, {len(bpf)} eBPF, {len(bad)} 32-bit/other')
    return 1 if bad else 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ''
    if cmd == 'extract' and len(sys.argv) == 4:
        extract(sys.argv[2], sys.argv[3])
    elif cmd == 'elfscan' and len(sys.argv) >= 3:
        sys.exit(elfscan(sys.argv[2:]))
    elif cmd == 'label' and len(sys.argv) == 5:
        p = os.path.join(sys.argv[2], sys.argv[3].lstrip('/'))
        os.setxattr(p, 'security.selinux', sys.argv[4].encode() + b'\0', follow_symlinks=False)
    else:
        sys.exit(__doc__)


if __name__ == '__main__':
    main()
