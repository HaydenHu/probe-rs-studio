//! CMSIS-Pack -> probe-rs target description, implemented in pure Rust.
//!
//! A `.pack` is a ZIP archive holding a `.pdsc` (XML device description) and
//! one or more `.FLM` files (ELF32 flash algorithms). This module extracts the
//! same information probe-rs' `target-gen` does and emits a `ChipFamily` YAML
//! that `Registry::add_target_family_from_yaml()` accepts, so a user can add
//! any chip that is missing from the built-in registry without rebuilding the
//! application.
//!
//! Deliberately no external CLI, no Python, no `zip`/`object` crate: ZIP is
//! read through its central directory, XML through `quick-xml`, deflate through
//! `flate2`, and the small ELF32 subset is parsed directly.

use std::{
    collections::HashMap,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context as _, Result};
use base64::Engine as _;
use quick_xml::{events::Event as XmlEvent, reader::Reader};

// ---------------------------------------------------------------------------
// ZIP (central directory; stored + deflate; optional one-level nesting)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ZipEntry {
    name: String,
    data: Vec<u8>,
}

fn le16(b: &[u8], off: usize) -> Result<u16> {
    let s = b
        .get(off..off + 2)
        .ok_or_else(|| anyhow!("压缩包数据截断 (offset {off:#x})"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], off: usize) -> Result<u32> {
    let s = b
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("压缩包数据截断 (offset {off:#x})"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .context("ZIP deflate 数据解压失败")?;
    Ok(out)
}

const EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;

fn read_archive(data: &[u8]) -> Result<Vec<ZipEntry>> {
    let eocd = data
        .windows(4)
        .rposition(|w| w == EOCD)
        .ok_or_else(|| anyhow!("不是有效的 ZIP / CMSIS-Pack（缺少结束记录）"))?;
    let count = le16(data, eocd + 10)? as usize;
    let mut pos = le32(data, eocd + 16)? as usize;
    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        if le32(data, pos)? != CENTRAL {
            bail!("ZIP 中央目录损坏 (offset {pos:#x})");
        }
        let method = le16(data, pos + 10)?;
        let compressed_size = le32(data, pos + 20)? as usize;
        let name_len = le16(data, pos + 28)? as usize;
        let extra_len = le16(data, pos + 30)? as usize;
        let comment_len = le16(data, pos + 32)? as usize;
        let local_off = le32(data, pos + 42)? as usize;
        let name_bytes = data
            .get(pos + 46..pos + 46 + name_len)
            .ok_or_else(|| anyhow!("ZIP 文件名越界"))?;
        let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");

        if le32(data, local_off)? != LOCAL {
            bail!("ZIP 本地文件头损坏: {name}");
        }
        let l_name = le16(data, local_off + 26)? as usize;
        let l_extra = le16(data, local_off + 28)? as usize;
        let start = local_off + 30 + l_name + l_extra;
        let compressed = data
            .get(start..start + compressed_size)
            .ok_or_else(|| anyhow!("ZIP 成员数据越界: {name}"))?;
        let content = match method {
            0 => compressed.to_vec(),
            8 => inflate(compressed)?,
            other => bail!("不支持的 ZIP 压缩方式 {other}（{name}）"),
        };
        entries.push(ZipEntry {
            name,
            data: content,
        });
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

fn read_directory(root: &Path, base: &Path, out: &mut Vec<ZipEntry>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("无法读取目录 {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            read_directory(&path, base, out)?;
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "pdsc" && ext != "flm" {
            continue;
        }
        let name = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(ZipEntry {
            name,
            data: fs::read(&path)?,
        });
    }
    Ok(())
}

/// Unwrap an archive, following an optional outer `.pack.zip` wrapper.
fn pack_entries_from_bytes(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    let mut entries = read_archive(bytes)?;
    // Some vendors wrap the .pack in an outer .zip (e.g. Foo.1.0.0.pack.zip).
    for _ in 0..3 {
        if entries
            .iter()
            .any(|e| e.name.to_ascii_lowercase().ends_with(".pdsc"))
        {
            return Ok(entries);
        }
        let inner = entries
            .iter()
            .find(|e| {
                let n = e.name.to_ascii_lowercase();
                n.ends_with(".pack") || n.ends_with(".zip")
            })
            .cloned()
            .ok_or_else(|| anyhow!("压缩包里找不到 .pdsc 描述文件"))?;
        entries = read_archive(&inner.data)
            .with_context(|| format!("内层压缩包解析失败: {}", inner.name))?;
    }
    bail!("压缩包嵌套过深或缺少 .pdsc 描述文件")
}

/// Open a `.pack`, a `.pack.zip`, or an already unpacked directory.
fn pack_entries(path: &Path) -> Result<Vec<ZipEntry>> {
    if path.is_dir() {
        let mut out = Vec::new();
        read_directory(path, path, &mut out)?;
        if out.is_empty() {
            bail!("目录里没有 .pdsc / .FLM 文件");
        }
        return Ok(out);
    }
    let bytes = fs::read(path).with_context(|| format!("无法读取 {}", path.display()))?;
    pack_entries_from_bytes(&bytes)
        .with_context(|| format!("{} 不是有效的 CMSIS-Pack / ZIP", path.display()))
}

fn normalise(name: &str) -> String {
    name.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn find_entry<'a>(entries: &'a [ZipEntry], reference: &str) -> Option<&'a ZipEntry> {
    let wanted = normalise(reference);
    entries
        .iter()
        .find(|e| normalise(&e.name) == wanted)
        .or_else(|| {
            let base = wanted.rsplit('/').next().unwrap_or(&wanted);
            entries
                .iter()
                .find(|e| normalise(&e.name).rsplit('/').next() == Some(base))
        })
}

// ---------------------------------------------------------------------------
// PDSC (device / memory / algorithm declarations)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct CoreSpec {
    core: Option<String>,
    ap: u8,
}

impl CoreSpec {
    fn with_attrs(&self, attrs: &HashMap<String, String>) -> Self {
        let mut out = self.clone();
        if let Some(c) = attrs.get("Dcore") {
            out.core = Some(c.clone());
        }
        // Non-standard but occasionally used by vendors.
        if let Some(ap) = attrs.get("Dap").and_then(|v| parse_number(v)) {
            out.ap = ap as u8;
        }
        out
    }
}

#[derive(Clone, Debug)]
struct MemoryDef {
    id: String,
    start: u64,
    size: u64,
    startup: bool,
}

#[derive(Clone, Debug)]
struct AlgorithmRef {
    file: String,
    start: u64,
    size: u64,
}

#[derive(Clone, Debug)]
struct DeviceDef {
    name: String,
    variants: Vec<String>,
    core: CoreSpec,
    memories: Vec<MemoryDef>,
    algorithms: Vec<AlgorithmRef>,
}

#[derive(Debug)]
struct Pdsc {
    family: String,
    release: String,
    devices: Vec<DeviceDef>,
}

fn attributes(e: &quick_xml::events::BytesStart<'_>) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            (
                String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                String::from_utf8_lossy(a.value.as_ref()).into_owned(),
            )
        })
        .collect()
}

fn parse_number(s: &str) -> Option<u64> {
    let s = s.trim();
    let (radix, digits) = if let Some(hex) = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
    {
        (16, hex)
    } else {
        (10, s)
    };
    u64::from_str_radix(digits.trim(), radix).ok()
}

fn core_type(dcore: &str) -> Result<&'static str> {
    let key = dcore.to_ascii_lowercase().replace([' ', '_'], "");
    Ok(match key.as_str() {
        "cortex-m0" | "cortex-m0+" | "cortexm0" | "cortexm0+" => "armv6m",
        "cortex-m3" => "armv7m",
        "cortex-m4" | "cortex-m7" => "armv7em",
        "cortex-m23" | "cortex-m33" | "cortex-m55" | "cortex-m85" | "star-mc1" => "armv8m",
        _ => bail!(
            "不支持的内核 '{dcore}'（目前支持 Cortex-M0/M0+/M3/M4/M7/M23/M33/M55/M85）"
        ),
    })
}

fn parse_pdsc(text: &str) -> Result<Pdsc> {
    let mut reader = Reader::from_str(text);
    let mut family = String::new();
    let mut release = String::new();
    let mut family_core = CoreSpec::default();
    let mut scope_core = CoreSpec::default();
    let mut in_subfamily = false;
    let mut device: Option<DeviceDef> = None;
    let mut devices = Vec::new();

    fn finish(device: &mut Option<DeviceDef>, devices: &mut Vec<DeviceDef>) {
        if let Some(d) = device.take() {
            devices.push(d);
        }
    }

    loop {
        let event = reader
            .read_event()
            .map_err(|e| anyhow!("pdsc XML 解析失败: {e}"))?;
        let self_closing = matches!(event, XmlEvent::Empty(_));
        let (tag, attrs, is_end) = match event {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => (
                String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                attributes(&e),
                false,
            ),
            XmlEvent::End(e) => (
                String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                HashMap::new(),
                true,
            ),
            XmlEvent::Eof => break,
            _ => continue,
        };

        if is_end {
            match tag.as_str() {
                "device" => finish(&mut device, &mut devices),
                "subFamily" => {
                    in_subfamily = false;
                    scope_core = family_core.clone();
                }
                _ => {}
            }
            continue;
        }

        match tag.as_str() {
            "family" => {
                if let Some(name) = attrs.get("Dfamily") {
                    family = name.clone();
                }
                family_core = family_core.with_attrs(&attrs);
                scope_core = family_core.clone();
            }
            "subFamily" => {
                in_subfamily = true;
                scope_core = family_core.with_attrs(&attrs);
            }
            "device" => {
                finish(&mut device, &mut devices);
                device = Some(DeviceDef {
                    name: attrs.get("Dname").cloned().unwrap_or_default(),
                    variants: Vec::new(),
                    core: scope_core.with_attrs(&attrs),
                    memories: Vec::new(),
                    algorithms: Vec::new(),
                });
            }
            "variant" => {
                if let (Some(d), Some(v)) = (device.as_mut(), attrs.get("Dvariant")) {
                    d.variants.push(v.clone());
                }
            }
            "processor" => {
                let updated = scope_core.with_attrs(&attrs);
                if let Some(d) = device.as_mut() {
                    d.core = updated;
                } else if in_subfamily {
                    scope_core = updated;
                } else {
                    scope_core = updated.clone();
                    family_core = updated;
                }
            }
            "memory" => {
                if let Some(d) = device.as_mut() {
                    if let (Some(start), Some(size)) = (
                        attrs.get("start").and_then(|v| parse_number(v)),
                        attrs.get("size").and_then(|v| parse_number(v)),
                    ) {
                        d.memories.push(MemoryDef {
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            start,
                            size,
                            startup: attrs.get("startup").is_some_and(|v| v == "1"),
                        });
                    }
                }
            }
            "algorithm" => {
                if let Some(d) = device.as_mut() {
                    if let Some(file) = attrs.get("name") {
                        d.algorithms.push(AlgorithmRef {
                            file: file.clone(),
                            start: attrs
                                .get("start")
                                .and_then(|v| parse_number(v))
                                .unwrap_or(0),
                            size: attrs
                                .get("size")
                                .and_then(|v| parse_number(v))
                                .unwrap_or(0),
                        });
                    }
                }
            }
            "release" => {
                if release.is_empty() {
                    release = attrs.get("version").cloned().unwrap_or_default();
                }
            }
            _ => {}
        }

        if self_closing && tag == "device" {
            finish(&mut device, &mut devices);
        }
    }

    let devices: Vec<DeviceDef> = devices
        .into_iter()
        .filter(|d| !d.name.is_empty())
        .collect();
    if devices.is_empty() {
        bail!("pdsc 里没有找到任何 <device> 定义");
    }
    if family.is_empty() {
        family = format!("{} Series", devices[0].name);
    }
    Ok(Pdsc {
        family,
        release,
        devices,
    })
}

// ---------------------------------------------------------------------------
// ELF32 flash algorithm (FLM)
// ---------------------------------------------------------------------------

const PT_LOAD: u32 = 1;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_NOBITS: u32 = 8;

#[derive(Clone, Copy, Debug)]
struct ProgramHeader {
    typ: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
}

#[derive(Clone, Debug)]
struct Section {
    name: String,
    typ: u32,
    addr: u64,
    offset: u64,
    size: u64,
    link: usize,
    entsize: u64,
}

struct Elf32<'a> {
    buf: &'a [u8],
    programs: Vec<ProgramHeader>,
    sections: Vec<Section>,
    symbols: HashMap<String, u32>,
}

fn cstr(table: &[u8], off: usize) -> String {
    let Some(tail) = table.get(off..) else {
        return String::new();
    };
    let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..end]).into_owned()
}

impl<'a> Elf32<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < 52 || &buf[..4] != b"\x7fELF" {
            bail!("FLM 不是 ELF 文件");
        }
        if buf[4] != 1 || buf[5] != 1 {
            bail!("FLM 只支持 32 位小端 ELF");
        }
        let phoff = le32(buf, 0x1c)? as usize;
        let shoff = le32(buf, 0x20)? as usize;
        let phentsize = le16(buf, 0x2a)? as usize;
        let phnum = le16(buf, 0x2c)? as usize;
        let shentsize = le16(buf, 0x2e)? as usize;
        let shnum = le16(buf, 0x30)? as usize;
        let shstrndx = le16(buf, 0x32)? as usize;

        let mut programs = Vec::with_capacity(phnum);
        for i in 0..phnum {
            let o = phoff + i * phentsize;
            programs.push(ProgramHeader {
                typ: le32(buf, o)?,
                offset: le32(buf, o + 4)? as u64,
                vaddr: le32(buf, o + 8)? as u64,
                paddr: le32(buf, o + 12)? as u64,
                filesz: le32(buf, o + 16)? as u64,
                memsz: le32(buf, o + 20)? as u64,
            });
        }

        let mut sections = Vec::with_capacity(shnum);
        for i in 0..shnum {
            let o = shoff + i * shentsize;
            sections.push(Section {
                name: String::new(),
                typ: le32(buf, o + 4)?,
                addr: le32(buf, o + 12)? as u64,
                offset: le32(buf, o + 16)? as u64,
                size: le32(buf, o + 20)? as u64,
                link: le32(buf, o + 24)? as usize,
                entsize: le32(buf, o + 36)? as u64,
            });
        }
        let shstr = sections
            .get(shstrndx)
            .ok_or_else(|| anyhow!("FLM 的节名表索引无效"))?;
        let names = buf
            .get(shstr.offset as usize..(shstr.offset + shstr.size) as usize)
            .ok_or_else(|| anyhow!("FLM 节名表越界"))?
            .to_vec();
        // Section names live in the string table; resolve the raw name offsets.
        for (i, s) in sections.iter_mut().enumerate() {
            let o = shoff + i * shentsize;
            let name_off = le32(buf, o)? as usize;
            s.name = cstr(&names, name_off);
        }

        let mut symbols = HashMap::new();
        for s in &sections {
            if s.typ != SHT_SYMTAB {
                continue;
            }
            let Some(strtab) = sections.get(s.link) else {
                continue;
            };
            let strings = buf
                .get(strtab.offset as usize..(strtab.offset + strtab.size) as usize)
                .ok_or_else(|| anyhow!("FLM 符号字符串表越界"))?;
            let step = if s.entsize == 0 { 16 } else { s.entsize } as usize;
            let mut off = s.offset as usize;
            let end = (s.offset + s.size) as usize;
            while off + 16 <= end {
                let name_off = le32(buf, off)? as usize;
                let value = le32(buf, off + 4)?;
                let name = cstr(strings, name_off);
                if !name.is_empty() {
                    symbols.insert(name, value);
                }
                off += step;
            }
        }

        Ok(Self {
            buf,
            programs,
            sections,
            symbols,
        })
    }

    /// Mirrors `target-gen/parser.rs::read_elf_bin_data`: uses p_paddr.
    fn read_vaddr(&self, address: u64, size: usize) -> Result<Vec<u8>> {
        for ph in &self.programs {
            let seg_size = ph.memsz.min(ph.filesz);
            if address >= ph.paddr && address + size as u64 <= ph.paddr + seg_size {
                let off = ph.offset + address - ph.paddr;
                return self
                    .buf
                    .get(off as usize..off as usize + size)
                    .map(|s| s.to_vec())
                    .ok_or_else(|| anyhow!("FLM 虚拟地址 {address:#x} 越界"));
            }
        }
        bail!("FLM 无法读取虚拟地址 {address:#x} 处的 {size} 字节")
    }

}

#[derive(Clone, Debug)]
struct FlashDevice {
    description: String,
    start: u64,
    size: u64,
    page_size: u64,
    erased: u8,
    program_timeout: u64,
    erase_timeout: u64,
    sectors: Vec<(u64, u64)>,
}

fn parse_flash_device(elf: &Elf32<'_>) -> Result<FlashDevice> {
    let addr = *elf
        .symbols
        .get("FlashDevice")
        .ok_or_else(|| anyhow!("FLM 缺少 FlashDevice 符号"))? as u64;
    let head = elf.read_vaddr(addr, 160)?;
    let description = cstr(&head[2..130], 0);
    let start = u32::from_le_bytes(head[132..136].try_into().unwrap()) as u64;
    let size = u32::from_le_bytes(head[136..140].try_into().unwrap()) as u64;
    let page_size = u32::from_le_bytes(head[140..144].try_into().unwrap()) as u64;
    let erased = head[148];
    let program_timeout = u32::from_le_bytes(head[152..156].try_into().unwrap()) as u64;
    let erase_timeout = u32::from_le_bytes(head[156..160].try_into().unwrap()) as u64;

    let mut sectors = Vec::new();
    for i in 0..64 {
        let raw = elf.read_vaddr(addr + 160 + i * 8, 8)?;
        let sector_size = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as u64;
        let sector_addr = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as u64;
        if sector_size == 0xffff_ffff || sector_addr == 0xffff_ffff {
            break;
        }
        sectors.push((sector_size, sector_addr));
    }
    Ok(FlashDevice {
        description,
        start,
        size,
        page_size,
        erased,
        program_timeout,
        erase_timeout,
        sectors,
    })
}

#[derive(Clone, Debug)]
struct FlashAlgorithm {
    name: String,
    description: String,
    blob: Vec<u8>,
    entry_points: Vec<(&'static str, u64)>,
    data_section_offset: u64,
    device: FlashDevice,
}

const ENTRY_SYMBOLS: &[(&str, &str)] = &[
    ("Init", "pc_init"),
    ("UnInit", "pc_uninit"),
    ("ProgramPage", "pc_program_page"),
    ("EraseSector", "pc_erase_sector"),
    ("EraseChip", "pc_erase_all"),
    ("Verify", "pc_verify"),
    ("BlankCheck", "pc_blank_check"),
    ("ReadFlash", "pc_read"),
];

fn parse_flm(bytes: &[u8], name: &str) -> Result<FlashAlgorithm> {
    let elf = Elf32::parse(bytes).with_context(|| format!("解析 FLM 失败: {name}"))?;

    // Collect PrgCode / PrgData / PrgData(BSS) sections that are fully covered
    // by a PT_LOAD segment (algorithm_binary.rs rule).
    let mut code: Option<(&Section, Vec<u8>, u64)> = None; // (section, data, load addr)
    let mut data_sec: Option<(&Section, Vec<u8>)> = None;
    let mut bss_len = 0u64;
    for ph in &elf.programs {
        if ph.typ != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        let begin = ph.offset;
        let end = ph.offset + ph.memsz;
        for sh in &elf.sections {
            if sh.offset < begin || sh.offset + sh.size > end {
                continue;
            }
            let content = if sh.typ == SHT_NOBITS {
                vec![]
            } else {
                elf.buf
                    .get(sh.offset as usize..(sh.offset + sh.size) as usize)
                    .unwrap_or(&[])
                    .to_vec()
            };
            let load_address = ph.vaddr + sh.offset - ph.offset;
            match (sh.name.as_str(), sh.typ) {
                ("PrgCode", SHT_PROGBITS) => {
                    code = Some((sh, content, load_address));
                }
                ("PrgData", SHT_PROGBITS) => {
                    data_sec = Some((sh, content));
                }
                ("PrgData", SHT_NOBITS) => {
                    bss_len = sh.size;
                }
                _ => {}
            }
        }
    }
    let (code_sec, code_data, _load_address) =
        code.ok_or_else(|| anyhow!("{name}: 缺少 PrgCode 段"))?;
    let (data_start, data_bytes) = match data_sec {
        Some((sh, bytes)) => (sh.addr, bytes),
        None => (code_sec.addr + code_sec.size, vec![]),
    };
    let mut blob = code_data;
    blob.extend_from_slice(&data_bytes);
    blob.resize(blob.len() + bss_len as usize, 0);

    let mut entry_points = Vec::new();
    for (symbol, key) in ENTRY_SYMBOLS {
        if let Some(value) = elf.symbols.get(*symbol) {
            let offset = value.saturating_sub(code_sec.addr as u32) as u64;
            entry_points.push((*key, offset));
        }
    }
    let device = parse_flash_device(&elf)?;
    Ok(FlashAlgorithm {
        name: name.to_owned(),
        description: device.description.clone(),
        blob,
        entry_points,
        data_section_offset: data_start,
        device,
    })
}

// ---------------------------------------------------------------------------
// YAML emission
// ---------------------------------------------------------------------------

fn yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn hex(value: u64) -> String {
    format!("0x{value:x}")
}

fn safe_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_').to_owned();
    if cleaned.is_empty() {
        "flash_algo".to_owned()
    } else {
        cleaned
    }
}

#[derive(Debug)]
struct AlgorithmInstance {
    name: String,
    parsed: FlashAlgorithm,
    /// Flash range end taken from the device's <algorithm> declaration.
    address_end: u64,
}

fn emit_algorithm(out: &mut String, algo: &AlgorithmInstance) {
    let a = &algo.parsed;
    let d = &a.device;
    out.push_str(&format!("- name: {}\n", yaml_string(&algo.name)));
    out.push_str(&format!(
        "  description: {}\n",
        yaml_string(&a.description)
    ));
    out.push_str("  default: true\n");
    out.push_str(&format!(
        "  instructions: {}\n",
        base64::engine::general_purpose::STANDARD.encode(&a.blob)
    ));
    for (key, value) in &a.entry_points {
        out.push_str(&format!("  {key}: {}\n", hex(*value)));
    }
    out.push_str(&format!(
        "  data_section_offset: {}\n",
        hex(a.data_section_offset)
    ));
    out.push_str("  flash_properties:\n");
    out.push_str("    address_range:\n");
    out.push_str(&format!("      start: {}\n", hex(d.start)));
    out.push_str(&format!("      end: {}\n", hex(algo.address_end.max(d.start))));
    out.push_str(&format!("    page_size: {}\n", hex(d.page_size)));
    out.push_str(&format!("    erased_byte_value: {}\n", hex(d.erased as u64)));
    out.push_str(&format!(
        "    program_page_timeout: {}\n",
        d.program_timeout
    ));
    out.push_str(&format!(
        "    erase_sector_timeout: {}\n",
        d.erase_timeout
    ));
    let limit = algo.address_end.saturating_sub(d.start);
    let sectors: Vec<(u64, u64)> = d
        .sectors
        .iter()
        .copied()
        .filter(|(_, address)| *address < limit)
        .collect();
    if sectors.is_empty() {
        out.push_str("    sectors: []\n");
    } else {
        out.push_str("    sectors:\n");
        for (size, address) in sectors {
            out.push_str(&format!("    - size: {}\n", hex(size)));
            out.push_str(&format!("      address: {}\n", hex(address)));
        }
    }
}

fn is_nvm(mem: &MemoryDef, algos: &[AlgorithmRef]) -> bool {
    let id = mem.id.to_ascii_uppercase();
    mem.startup
        || id.starts_with("IROM")
        || id.starts_with("ROM")
        || id.contains("FLASH")
        || algos.iter().any(|a| a.start == mem.start && a.size > 0)
}

fn emit_variant(
    out: &mut String,
    name: &str,
    device: &DeviceDef,
    algorithm_names: &[String],
) -> Result<()> {
    let dcore = device
        .core
        .core
        .as_deref()
        .ok_or_else(|| anyhow!("芯片 {name} 的 pdsc 未声明 Dcore 内核类型"))?;
    let core = core_type(dcore).with_context(|| format!("芯片 {name}"))?;
    out.push_str(&format!("- name: {}\n", yaml_string(name)));
    out.push_str("  cores:\n");
    out.push_str("  - name: main\n");
    out.push_str(&format!("    type: {core}\n"));
    out.push_str("    core_access_options: !Arm\n");
    out.push_str(&format!("      ap: !v1 {}\n", device.core.ap));
    out.push_str("  memory_map:\n");

    let mut memories = device.memories.clone();
    if memories.is_empty() {
        // Fall back to the algorithm range and a conventional SRAM window.
        if let Some(a) = device.algorithms.first() {
            memories.push(MemoryDef {
                id: "IROM1".to_owned(),
                start: a.start,
                size: a.size,
                startup: true,
            });
        }
        memories.push(MemoryDef {
            id: "IRAM1".to_owned(),
            start: 0x2000_0000,
            size: 0x0001_0000,
            startup: false,
        });
    }
    let mut boot_assigned = false;
    for mem in &memories {
        let end = mem.start.saturating_add(mem.size);
        if is_nvm(mem, &device.algorithms) {
            out.push_str("  - !Nvm\n");
            out.push_str(&format!("    name: {}\n", yaml_string(&mem.id)));
            out.push_str("    range:\n");
            out.push_str(&format!("      start: {}\n", hex(mem.start)));
            out.push_str(&format!("      end: {}\n", hex(end)));
            out.push_str("    cores:\n    - main\n");
            out.push_str("    access:\n");
            out.push_str("      write: false\n");
            if !boot_assigned {
                out.push_str("      boot: true\n");
                boot_assigned = true;
            }
        } else {
            out.push_str("  - !Ram\n");
            out.push_str(&format!("    name: {}\n", yaml_string(&mem.id)));
            out.push_str("    range:\n");
            out.push_str(&format!("      start: {}\n", hex(mem.start)));
            out.push_str(&format!("      end: {}\n", hex(end)));
            out.push_str("    cores:\n    - main\n");
        }
    }
    if algorithm_names.is_empty() {
        out.push_str("  flash_algorithms: []\n");
    } else {
        out.push_str("  flash_algorithms:\n");
        for name in algorithm_names {
            out.push_str(&format!("  - {name}\n"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ConvertedPack {
    pub family: String,
    pub release: String,
    pub variants: Vec<String>,
    pub algorithms: Vec<String>,
    pub yaml: String,
}

/// Convert a CMSIS-Pack (`.pack`, `.pack.zip` or an unpacked directory) into a
/// probe-rs `ChipFamily` YAML document.
pub fn convert(path: &Path) -> Result<ConvertedPack> {
    let entries = pack_entries(path)?;
    convert_entries(&entries)
}

fn convert_entries(entries: &[ZipEntry]) -> Result<ConvertedPack> {
    let pdsc_entry = entries
        .iter()
        .find(|e| e.name.to_ascii_lowercase().ends_with(".pdsc"))
        .ok_or_else(|| anyhow!("pack 里没有 .pdsc 文件"))?;
    let text = String::from_utf8_lossy(&pdsc_entry.data);
    let pdsc = parse_pdsc(&text).with_context(|| format!("解析 {} 失败", pdsc_entry.name))?;

    // One algorithm instance per (FLM, declared flash range). Deliberately no
    // content de-duplication: target-gen drops algorithms whose *code* is
    // identical even when their FlashDevice size differs (e.g. HC32F4XX).
    let mut instances: Vec<AlgorithmInstance> = Vec::new();
    let mut cache: HashMap<String, FlashAlgorithm> = HashMap::new();
    let mut used_names: HashMap<String, usize> = HashMap::new();

    // variant name -> algorithm instance indices
    let mut variants: Vec<(String, DeviceDef, Vec<usize>)> = Vec::new();

    for device in &pdsc.devices {
        let names: Vec<String> = if device.variants.is_empty() {
            vec![device.name.clone()]
        } else {
            device.variants.clone()
        };
        for variant in names {
            let mut indices = Vec::new();
            for algo_ref in &device.algorithms {
                let entry = find_entry(&entries, &algo_ref.file).ok_or_else(|| {
                    anyhow!(
                        "找不到 Flash 算法 {}（芯片 {}）",
                        algo_ref.file,
                        variant
                    )
                })?;
                let stem = Path::new(&entry.name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&entry.name);
                let base_name = safe_name(stem);

                let cache_key = normalise(&entry.name);
                let parsed = match cache.get(&cache_key) {
                    Some(cached) => cached.clone(),
                    None => {
                        let parsed = parse_flm(&entry.data, &cache_key)
                            .with_context(|| format!("解析 {} 失败", entry.name))?;
                        cache.insert(cache_key, parsed.clone());
                        parsed
                    }
                };

                let end = if algo_ref.size > 0 {
                    algo_ref.start.saturating_add(algo_ref.size)
                } else {
                    parsed.device.start.saturating_add(parsed.device.size)
                };
                // Reuse an instance only when both the FLM and the declared
                // flash range match; otherwise create a separate algorithm.
                let existing = instances.iter().position(|a| {
                    a.parsed.name == parsed.name && a.address_end == end
                });
                let idx = match existing {
                    Some(i) => i,
                    None => {
                        let count = used_names.entry(base_name.clone()).or_insert(0);
                        *count += 1;
                        let name = if *count == 1 {
                            base_name.clone()
                        } else {
                            format!("{base_name}_{count}")
                        };
                        instances.push(AlgorithmInstance {
                            name,
                            parsed,
                            address_end: end,
                        });
                        instances.len() - 1
                    }
                };
                if !indices.contains(&idx) {
                    indices.push(idx);
                }
            }
            variants.push((variant, device.clone(), indices));
        }
    }

    if instances.is_empty() {
        bail!("pack 里没有可用的 Flash 算法（.FLM），无法生成可烧录的芯片描述");
    }

    let mut yaml = String::new();
    yaml.push_str(&format!("name: {}\n", yaml_string(&pdsc.family)));
    yaml.push_str("generated_from_pack: true\n");
    if !pdsc.release.is_empty() {
        yaml.push_str(&format!(
            "pack_file_release: {}\n",
            yaml_string(&pdsc.release)
        ));
    }
    yaml.push_str("variants:\n");
    for (variant, device, indices) in &variants {
        let names: Vec<String> = indices
            .iter()
            .map(|i| instances[*i].name.clone())
            .collect();
        emit_variant(&mut yaml, variant, device, &names)
            .with_context(|| format!("生成芯片 {variant} 描述失败"))?;
    }
    yaml.push_str("flash_algorithms:\n");
    for algo in &instances {
        emit_algorithm(&mut yaml, algo);
    }

    Ok(ConvertedPack {
        family: pdsc.family,
        release: pdsc.release,
        variants: variants.into_iter().map(|(v, _, _)| v).collect(),
        algorithms: instances.iter().map(|a| a.name.clone()).collect(),
        yaml,
    })
}

/// Directory scanned by `backend::build_registry()` on every chip lookup.
pub fn targets_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(mut exe) = std::env::current_exe() {
        exe.pop();
        candidates.push(exe.join("targets"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("targets");
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    for dir in &candidates {
        if fs::create_dir_all(dir).is_err() {
            continue;
        }
        let probe = dir.join(".write_probe");
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(&probe);
            return Ok(dir.clone());
        }
    }
    bail!("无法写入 targets/ 目录（请检查程序目录权限）")
}

fn output_stem(path: &Path) -> String {
    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmsis_pack")
        .to_owned();
    loop {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".pack") || lower.ends_with(".zip") {
            name.truncate(name.len() - 5);
        } else {
            break;
        }
    }
    safe_name(&name)
}

/// Write a converted description into `targets/` so it is picked up on the
/// next registry build and on every subsequent application start.
pub fn install(path: &Path, yaml: &str) -> Result<PathBuf> {
    let dest = targets_dir()?.join(format!("{}.yaml", output_stem(path)));
    fs::write(&dest, yaml).with_context(|| format!("写入 {} 失败", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Self-contained samples built from the Holychip HC32F4XX pack: the inner
    // .pack holds the real .pdsc and the three real .FLM files, the outer file
    // reproduces the `.pack.zip` wrapper vendors often publish.
    const MINI_PACK: &[u8] = include_bytes!("../tests/fixtures/HC32F4XX-mini.pack");
    const MINI_PACK_ZIP: &[u8] = include_bytes!("../tests/fixtures/HC32F4XX-mini.pack.zip");
    const BUNDLED: &str = include_str!("../targets/HC32F4XX.yaml");

    fn mini() -> ConvertedPack {
        convert_entries(&pack_entries_from_bytes(MINI_PACK).expect("mini pack parses")).unwrap()
    }

    fn instructions(yaml: &str) -> Vec<String> {
        yaml.lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("instructions: ")
                    .map(|v| v.trim().to_owned())
            })
            .collect()
    }

    #[test]
    fn numeric_string_and_core_helpers() {
        assert_eq!(parse_number("0x00040000"), Some(0x40000));
        assert_eq!(parse_number("4096"), Some(4096));
        assert_eq!(parse_number("not a number"), None);
        assert_eq!(core_type("Cortex-M0+").unwrap(), "armv6m");
        assert_eq!(core_type("Cortex-M3").unwrap(), "armv7m");
        assert_eq!(core_type("Cortex-M4").unwrap(), "armv7em");
        assert_eq!(core_type("Cortex-M33").unwrap(), "armv8m");
        assert!(core_type("Cortex-A5").is_err());
        assert_eq!(yaml_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(safe_name("HC32F4xx_256.FLM"), "hc32f4xx_256.flm");
    }

    #[test]
    fn converted_pack_matches_the_verified_bundled_description() {
        let c = mini();
        assert_eq!(c.family, "HC32F4 Series");
        assert_eq!(c.release, "1.0.7");
        assert_eq!(
            c.variants,
            vec!["HC32F401BRCX", "HC32F401BREX", "HC32F401BRGX"]
        );
        assert_eq!(
            c.algorithms,
            vec!["hc32f4xx_256", "hc32f4xx_512", "hc32f4xx_1024"]
        );
        // Byte-for-byte identical algorithm blobs, plus the same entry points.
        assert_eq!(instructions(&c.yaml), instructions(BUNDLED));
        for expected in [
            "pc_init: 0x1d",
            "pc_uninit: 0x4f",
            "pc_program_page: 0xed",
            "pc_erase_sector: 0x9b",
            "pc_erase_all: 0x5d",
            "data_section_offset: 0x168",
            "type: armv7em",
            "end: 0x8040000",
            "end: 0x8080000",
            "end: 0x8100000",
        ] {
            assert!(c.yaml.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn nested_pack_zip_produces_the_same_yaml() {
        let direct = mini();
        let nested =
            convert_entries(&pack_entries_from_bytes(MINI_PACK_ZIP).unwrap()).unwrap();
        assert_eq!(direct.variants, nested.variants);
        assert_eq!(direct.algorithms, nested.algorithms);
        assert_eq!(direct.yaml, nested.yaml);
    }

    #[test]
    fn generated_yaml_is_accepted_by_probe_rs_registry() {
        let c = mini();
        let mut registry = probe_rs::config::Registry::from_builtin_families();
        registry.add_target_family_from_yaml(&c.yaml).unwrap();
        for (name, size) in c.variants.iter().zip([0x4_0000u64, 0x8_0000, 0x10_0000]) {
            let target = registry.get_target_by_name(name).unwrap();
            assert_eq!(target.flash_algorithms.len(), 1, "{name}");
            let nvm: Vec<_> = target
                .memory_map
                .iter()
                .filter(|r| r.as_nvm_region().is_some())
                .collect();
            assert_eq!(nvm.len(), 1, "{name}");
            let range = nvm[0].address_range();
            assert_eq!(range.end - range.start, size, "{name}");
        }
    }

    #[test]
    fn flm_elf_parser_extracts_flash_device_and_entry_points() {
        let entries = pack_entries_from_bytes(MINI_PACK).unwrap();
        let entry = find_entry(&entries, "Flash/HC32F4xx_256.FLM").unwrap();
        let algo = parse_flm(&entry.data, "test").unwrap();
        assert_eq!(algo.blob.len(), 364);
        assert_eq!(algo.data_section_offset, 0x168);
        assert_eq!(algo.device.start, 0x0800_0000);
        assert_eq!(algo.device.size, 0x4_0000);
        assert_eq!(algo.device.page_size, 0x400);
        assert_eq!(algo.device.erased, 0xff);
        assert_eq!(algo.device.sectors.len(), 3);
        assert_eq!(
            algo.entry_points,
            vec![
                ("pc_init", 0x1d),
                ("pc_uninit", 0x4f),
                ("pc_program_page", 0xed),
                ("pc_erase_sector", 0x9b),
                ("pc_erase_all", 0x5d),
            ]
        );
    }

    #[test]
    fn unpacked_pack_directory_is_supported() {
        let dir = std::env::temp_dir().join(format!(
            "probe-rs-studio-pack-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        for entry in read_archive(MINI_PACK).unwrap() {
            let path = dir.join(&entry.name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, &entry.data).unwrap();
        }
        let converted = convert(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(converted.variants.len(), 3);
        assert_eq!(converted.algorithms.len(), 3);
    }
}
