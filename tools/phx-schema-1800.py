#!/usr/bin/env python3
"""Dump Phoenix class versions from Anno1800.exe without running the game.

Usage: python3 tools/phx-schema-1800.py GAME_DIR [OUTPUT.tsv]
Writes formats/data/anno1800/classes.tsv by default, in the format of
formats/data/anno117/classes.tsv. Only versions absent from the 117 registry
are emitted. The anchor is the shared Asset schema hash; its class block
identifies the registry thunk.
"""
import json
import argparse
from pathlib import Path
import struct
import zlib


class Image:
    def __init__(self, path):
        self.data = path.read_bytes()
        pe = self.u32(0x3C)
        if self.data[pe:pe + 4] != b"PE\0\0":
            raise ValueError("not a PE executable")
        optional = pe + 24
        if self.u16(optional) != 0x20B:
            raise ValueError("expected PE32+")
        self.base = self.u64(optional + 24)
        sections = optional + self.u16(pe + 20)
        self.sections = []
        for n in range(self.u16(pe + 6)):
            off = sections + n * 40
            virtual, rva, size, raw = struct.unpack_from("<IIII", self.data, off + 8)
            self.sections.append((self.base + rva, max(virtual, size), raw, size))

    def offset(self, va):
        for start, _, raw, size in self.sections:
            if start <= va < start + size:
                return raw + va - start
        raise ValueError(f"unmapped address {va:#x}")

    def read(self, va, length):
        for start, virtual, raw, size in self.sections:
            if start <= va and va + length <= start + virtual:
                offset = va - start
                stored = max(0, min(length, size - offset))
                # The PE loader zero-fills .bss, including Object's runtime
                # descriptor. Those zero rows have no serialized fields.
                return self.data[raw + offset:raw + offset + stored] + bytes(length - stored)
        raise ValueError(f"unmapped descriptor {va:#x}")

    def u16(self, off):
        return struct.unpack_from("<H", self.data, off)[0]

    def u32(self, off):
        return struct.unpack_from("<I", self.data, off)[0]

    def u64(self, off):
        return struct.unpack_from("<Q", self.data, off)[0]


def registry(image):
    anchor = bytes.fromhex("1102605fdf4bc6467271ea16b9b73bec")
    pos = image.data.find(anchor)
    if pos < 0 or image.u32(pos + 0x20) != 0xC36E7558:
        raise ValueError("Asset registry anchor not found")
    thunk = image.data[pos - 0x48:pos - 0x40]
    blocks = {}
    at = 0
    while (at := image.data.find(thunk, at)) >= 0:
        block = at - 0x10
        at += 8
        if block < 0 or block % 8:
            continue
        count = image.u32(block + 0x30)
        size = image.u32(block + 0x80)
        align = image.u32(block + 0x84)
        crc = image.u32(block + 0x78)
        if count > 200 or size > 0x10000 or align not in (1, 2, 4, 8, 16) or crc < 0x10000:
            continue
        rows = []
        row_va = image.u64(block + 0x28)
        for n in range(count):
            row = struct.unpack("<IIIII", image.read(row_va + n * 0x38, 20))
            if row[0] > 0xFFFF or row[4] > size:
                raise ValueError(f"invalid descriptor in class {crc:08x}")
            rows.append(row)
        parent = image.u64(block + 0x68)
        base = image.u32(image.offset(parent) + 0x78) if parent else 0
        if crc in blocks:
            raise ValueError(f"duplicate registry class {crc:08x}")
        blocks[crc] = dict(base=base, size=size, hash=image.data[block + 0x58:block + 0x68], rows=rows)
    if len(blocks) < 100:
        raise ValueError(f"only {len(blocks)} class blocks found")
    return blocks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("game_dir", type=Path)
    parser.add_argument("output", type=Path, nargs="?")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    versions = {}
    names = {}
    # Names from the 117 registry, then the ones the 1800 table already has
    # (some were recovered by hand, docs/phx-schema-tables.md section 10, and
    # are in neither the exe nor the corpus), then the corpus.
    data = root / "formats/data"
    for game, path in (("117", data / "anno117/classes.tsv"), ("1800", data / "anno1800/classes.tsv")):
        if not path.exists():
            continue
        for line in path.read_text().splitlines():
            cols = line.split("\t")
            if cols[0] not in ("class", "prop"):
                continue
            crc = int(cols[1], 16)
            if game == "117" and cols[0] == "class":
                versions[crc] = bytes.fromhex(cols[5])
            if cols[2] and zlib.crc32(cols[2].encode()) == crc:
                names.setdefault(crc, cols[2])
    corpus = root.parent / "scripts/phx_names_corpus.json"
    if corpus.exists():
        for crc, name in json.loads(corpus.read_text())["names"].items():
            value = int(crc)
            if zlib.crc32(name.encode()) == value:
                names.setdefault(value, name)
    image = Image(args.game_dir / "Bin/Win64/Anno1800.exe")
    blocks = registry(image)
    selected = {crc: b for crc, b in blocks.items() if versions.get(crc) != b["hash"]}
    out = [
        "# Anno1800.exe's Phoenix class versions the 117 registry lacks, generated by tools/phx-schema-1800.py.",
        "# Exact schema hashes select these versions; unknown versions stay opaque.",
        "# Tab-separated, kind first. Ids are 0x%08x; an empty name is unknown (every name is crc32-checked).",
        "# class\tcrc\tname\tbase\tsize\thash",
        "#   A class version; its prop rows follow in registry order.",
        "# prop\tname_crc\tname\tty\ttype_crc\tmode\toffset",
    ]
    for crc, b in sorted(selected.items()):
        out.append(f"class\t0x{crc:08x}\t{names.get(crc, '')}\t0x{b['base']:08x}\t{b['size']}\t{b['hash'].hex()}")
        for ty, type_crc, mode, name, off in b["rows"]:
            out.append(f"prop\t0x{name:08x}\t{names.get(name, '')}\t0x{ty:04x}\t0x{type_crc:08x}\t0x{mode:x}\t0x{off:x}")
    output = args.output or root / "formats/data/anno1800/classes.tsv"
    output.write_text("\n".join(out) + "\n")
    print(f"{len(blocks)} classes, {len(selected)} additional versions -> {output}")


if __name__ == "__main__":
    main()
