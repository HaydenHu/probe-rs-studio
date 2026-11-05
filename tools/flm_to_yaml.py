"""Offline CMSIS-Pack -> probe-rs target YAML helper.

Implements the same extraction rules as probe-rs' `target-gen`
(parser.rs / algorithm_binary.rs / flash_device.rs), but works on the
already-unpacked .FLM files without compiling the whole probe-rs workspace.

Used here to build Holychip HC32F4XX.yaml with one flash algorithm per
capacity; `target-gen pack` de-duplicates the 256K algorithm by mistake.
"""

import base64
import struct
import sys
from pathlib import Path

SHT_PROGBITS, SHT_SYMTAB, SHT_NOBITS, PT_LOAD = 1, 2, 8, 1
PC_KEYS = {
    "Init": "pc_init",
    "UnInit": "pc_uninit",
    "EraseChip": "pc_erase_all",
    "EraseSector": "pc_erase_sector",
    "ProgramPage": "pc_program_page",
    "Verify": "pc_verify",
    "BlankCheck": "pc_blank_check",
    "ReadFlash": "pc_read",
}


class Elf32:
    def __init__(self, buf: bytes):
        assert buf[:4] == b"\x7fELF" and buf[4] == 1, "not an ELF32 file"
        assert buf[5] == 1, "only little-endian is supported"
        self.buf = buf
        self.e = "<"
        e = self.e
        (self.phoff,) = struct.unpack_from(e + "I", buf, 0x1C)
        (self.shoff,) = struct.unpack_from(e + "I", buf, 0x20)
        self.phentsize, self.phnum, self.shentsize, self.shnum, self.shstrndx = (
            struct.unpack_from(e + "HHHHH", buf, 0x2A)
        )
        self.program_headers = []
        for i in range(self.phnum):
            off = self.phoff + i * self.phentsize
            vals = struct.unpack_from(e + "IIIIIIII", buf, off)
            self.program_headers.append(
                dict(zip("type offset vaddr paddr filesz memsz flags align".split(), vals))
            )
        self.sections = []
        for i in range(self.shnum):
            off = self.shoff + i * self.shentsize
            vals = struct.unpack_from(e + "IIIIIIIIII", buf, off)
            self.sections.append(
                dict(
                    zip(
                        "name type flags addr offset size link info addralign entsize".split(),
                        vals,
                    )
                )
            )
        shstr = self.sections[self.shstrndx]
        self.shstrtab = buf[shstr["offset"] : shstr["offset"] + shstr["size"]]
        for s in self.sections:
            s["name_str"] = self._cstr(self.shstrtab, s["name"])

        self.symbols = {}
        for s in self.sections:
            if s["type"] != SHT_SYMTAB:
                continue
            strtab = self.sections[s["link"]]
            strings = buf[strtab["offset"] : strtab["offset"] + strtab["size"]]
            for off in range(s["offset"], s["offset"] + s["size"], s["entsize"] or 16):
                st_name, st_value, st_size, st_info, st_other, st_shndx = struct.unpack_from(
                    e + "IIIBBH", buf, off
                )
                name = self._cstr(strings, st_name)
                if name:
                    self.symbols[name] = st_value

    @staticmethod
    def _cstr(table: bytes, off: int) -> str:
        end = table.find(b"\0", off)
        return table[off:end].decode("utf-8", "replace")

    def section(self, name: str, sh_type: int):
        for s in self.sections:
            if s["name_str"] == name and s["type"] == sh_type:
                return s
        return None

    def read_vaddr(self, address: int, size: int) -> bytes:
        """Mirror parser.rs::read_elf_bin_data (uses p_paddr)."""
        for ph in self.program_headers:
            seg_addr = ph["paddr"]
            seg_size = min(ph["memsz"], ph["filesz"])
            if seg_addr <= address and address + size <= seg_addr + seg_size:
                off = ph["offset"] + address - seg_addr
                return self.buf[off : off + size]
        raise ValueError(f"cannot read {size} bytes at {address:#x}")


def parse_flash_device(elf: Elf32) -> dict:
    addr = elf.symbols["FlashDevice"]
    data = elf.read_vaddr(addr, 160)
    version = struct.unpack_from("<H", data, 0)[0]
    name = data[2:130].split(b"\0", 1)[0].decode("utf-8", "replace")
    typ = struct.unpack_from("<H", data, 130)[0]
    start, size, page, _reserved = struct.unpack_from("<IIII", data, 132)
    erased = data[148]
    program_timeout, erase_timeout = struct.unpack_from("<II", data, 152)
    sectors = []
    off = 160
    while True:
        raw = elf.read_vaddr(addr + off, 8)
        sector_size, sector_addr = struct.unpack_from("<II", raw, 0)
        if sector_size == 0xFFFF_FFFF or sector_addr == 0xFFFF_FFFF:
            break
        sectors.append((sector_size, sector_addr))
        off += 8
        if len(sectors) > 64:
            break
    return dict(
        version=version,
        name=name,
        typ=typ,
        start=start,
        size=size,
        page_size=page,
        erased=erased,
        program_timeout=program_timeout,
        erase_timeout=erase_timeout,
        sectors=sectors,
    )


def parse_algorithm(path: Path) -> dict:
    buf = path.read_bytes()
    elf = Elf32(buf)

    # algorithm_binary.rs: only sections fully inside a PT_LOAD file range.
    found = {}
    for ph in elf.program_headers:
        if ph["type"] != PT_LOAD or ph["memsz"] == 0:
            continue
        sector = range(ph["offset"], ph["offset"] + ph["memsz"])
        for sh in elf.sections:
            if sh["offset"] not in sector or sh["offset"] + sh["size"] > sector.stop:
                continue
            data = (
                b""
                if sh["type"] == SHT_NOBITS
                else buf[sh["offset"] : sh["offset"] + sh["size"]]
            )
            section = dict(
                start=sh["addr"],
                length=sh["size"],
                data=data,
                load_address=ph["vaddr"] + sh["offset"] - ph["offset"],
            )
            if sh["name_str"] == "PrgCode" and sh["type"] == SHT_PROGBITS:
                found["code"] = section
            elif sh["name_str"] == "PrgData" and sh["type"] == SHT_PROGBITS:
                found["data"] = section
            elif sh["name_str"] == "PrgData" and sh["type"] == SHT_NOBITS:
                found["bss"] = section

    code = found.get("code")
    if code is None:
        raise ValueError(f"{path.name}: no PrgCode section")
    data_section = found.get("data") or dict(
        start=code["start"] + code["length"], length=0, data=b""
    )
    bss = found.get("bss") or dict(length=0)
    blob = code["data"] + data_section["data"] + bytes(bss["length"])

    pcs = {}
    for sym, key in PC_KEYS.items():
        if sym in elf.symbols:
            pcs[key] = elf.symbols[sym] - code["start"]

    dev = parse_flash_device(elf)
    return dict(
        name=path.stem.lower(),
        description=dev["name"],
        blob=blob,
        pcs=pcs,
        data_section_offset=data_section["start"],
        device=dev,
    )


def hexv(v: int) -> str:
    return f"0x{v:x}"


def algo_yaml(a: dict) -> str:
    d = a["device"]
    lines = [
        f"- name: {a['name']}",
        f"  description: {a['description']}",
        "  default: true",
        "  instructions: " + base64.b64encode(a["blob"]).decode("ascii"),
    ]
    for key in (
        "pc_init",
        "pc_uninit",
        "pc_program_page",
        "pc_erase_sector",
        "pc_erase_all",
        "pc_verify",
        "pc_blank_check",
        "pc_read",
    ):
        if key in a["pcs"]:
            lines.append(f"  {key}: {hexv(a['pcs'][key])}")
    lines += [
        f"  data_section_offset: {hexv(a['data_section_offset'])}",
        "  flash_properties:",
        "    address_range:",
        f"      start: {hexv(d['start'])}",
        f"      end: {hexv(d['start'] + d['size'])}",
        f"    page_size: {hexv(d['page_size'])}",
        f"    erased_byte_value: {hexv(d['erased'])}",
        f"    program_page_timeout: {d['program_timeout']}",
        f"    erase_sector_timeout: {d['erase_timeout']}",
        "    sectors:",
    ]
    for size, address in d["sectors"]:
        lines.append(f"    - size: {hexv(size)}")
        lines.append(f"      address: {hexv(address)}")
    return "\n".join(lines)


def main():
    flash_dir = Path(sys.argv[1])
    out = Path(sys.argv[2])
    capacities = [
        ("HC32F401BRCX", 0x0004_0000, "HC32F4xx_256.FLM"),
        ("HC32F401BREX", 0x0008_0000, "HC32F4xx_512.FLM"),
        ("HC32F401BRGX", 0x0010_0000, "HC32F4xx_1024.FLM"),
    ]
    algos = []
    for _, _, flm in capacities:
        a = parse_algorithm(flash_dir / flm)
        d = a["device"]
        print(
            f"{flm}: name={a['name']!r} desc={d['name']!r} "
            f"flash=0x{d['start']:x}..0x{d['start']+d['size']:x} "
            f"page=0x{d['page_size']:x} blob={len(a['blob'])}B "
            f"pcs={ {k: hex(v) for k, v in a['pcs'].items()} }"
        )
        algos.append(a)

    lines = [
        "name: HC32F4 Series",
        "generated_from_pack: true",
        "pack_file_release: 1.0.7",
        "variants:",
    ]
    for (chip, size, _), a in zip(capacities, algos):
        lines += [
            f"- name: {chip}",
            "  cores:",
            "  - name: main",
            "    type: armv7em",
            "    core_access_options: !Arm",
            "      ap: !v1 0",
            "  memory_map:",
            "  - !Nvm",
            "    name: IROM1",
            "    range:",
            f"      start: 0x8000000",
            f"      end: {hexv(0x0800_0000 + size)}",
            "    cores:",
            "    - main",
            "    access:",
            "      write: false",
            "      boot: true",
            "  - !Ram",
            "    name: IRAM1",
            "    range:",
            "      start: 0x20000000",
            "      end: 0x20030000",
            "    cores:",
            "    - main",
            "  flash_algorithms:",
            f"  - {a['name']}",
        ]
    lines.append("flash_algorithms:")
    for a in algos:
        lines.append(algo_yaml(a))
    out.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {out} ({out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
