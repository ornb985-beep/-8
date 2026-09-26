#!/usr/bin/env python3
"""Generate AIDL NDK-backend headers for *enum* declarations (what
`aidl --lang=ndk` emits for `@Backing enum`): enum class with the backing
type, toString() and ndk::enum_range. Used where the host-built aidl tool is
not available.   aidl_ndk_enum.py <out_include_dir> <file.aidl>..."""
import os, re, sys

TYPES = {'byte': 'int8_t', 'int': 'int32_t', 'long': 'int64_t'}

def parse(path):
    src = open(path).read()
    src = re.sub(r'/\*.*?\*/', '', src, flags=re.S)
    src = re.sub(r'//[^\n]*', '', src)
    pkg = re.search(r'package\s+([\w.]+)\s*;', src).group(1)
    m = re.search(r'@Backing\s*\(\s*type\s*=\s*"(\w+)"\s*\)[^{]*?enum\s+(\w+)\s*\{(.*?)\}', src, re.S)
    if not m:
        return None
    backing, name, body = TYPES[m.group(1)], m.group(2), m.group(3)
    items = []
    for part in [p.strip() for p in body.split(',') if p.strip()]:
        k, _, v = part.partition('=')
        items.append((k.strip(), v.strip()))
    return pkg, name, backing, items

def emit(out, pkg, name, backing, items):
    ns = pkg.split('.')
    d = os.path.join(out, 'aidl', *ns)
    os.makedirs(d, exist_ok=True)
    enumerators = []
    prev = None
    for k, v in items:
        if not v:
            v = '0' if prev is None else f'static_cast<{backing}>({name}::{prev}) + 1'
        enumerators.append((k, v))
        prev = k
    lines = ['#pragma once', '#include <array>', '#include <cstdint>', '#include <string>',
             '#include <android/binder_enums.h>', '']
    lines.append('namespace aidl { ' + ' '.join(f'namespace {n} {{' for n in ns))
    lines.append(f'enum class {name} : {backing} {{')
    for k, v in enumerators:
        v2 = re.sub(r'(?<![\w:])([A-Z][A-Z0-9_]+)(?![\w(])', lambda mm: f'static_cast<{backing}>({name}::{mm.group(1)})'
                    if mm.group(1) in dict(enumerators) else mm.group(1), v)
        lines.append(f'  {k} = {v2},')
    lines.append('};')
    lines.append('')
    lines.append(f'[[nodiscard]] static inline std::string toString({name} val) {{')
    # Values may alias, so no switch: the first enumerator with the value wins.
    for k, _ in enumerators:
        lines.append(f'  if (val == {name}::{k}) return "{k}";')
    lines.append(f'  return std::to_string(static_cast<{backing}>(val));')
    lines.append('}')
    lines.append(' '.join('}' for _ in ns) + ' }  // namespace aidl')
    fq = '::aidl::' + '::'.join(ns) + '::' + name
    lines.append('namespace ndk { namespace internal {')
    lines.append('#pragma clang diagnostic push')
    lines.append('#pragma clang diagnostic ignored "-Wc++17-extensions"')
    lines.append(f'template <>\nconstexpr inline std::array<{fq}, {len(enumerators)}> enum_values<{fq}> = {{')
    for k, _ in enumerators:
        lines.append(f'  {fq}::{k},')
    lines.append('};')
    lines.append('#pragma clang diagnostic pop')
    lines.append('}  // namespace internal\n}  // namespace ndk')
    open(os.path.join(d, name + '.h'), 'w').write('\n'.join(lines) + '\n')

if __name__ == '__main__':
    out = sys.argv[1]
    for p in sys.argv[2:]:
        r = parse(p)
        if r:
            emit(out, *r)
            print('enum', r[1], len(r[3]))
