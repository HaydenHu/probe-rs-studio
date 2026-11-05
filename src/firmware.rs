//! Firmware file inspection (Intel HEX / ELF / raw binary).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context as _};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FirmwareFormat {
    Hex,
    Elf,
    Bin,
}

impl FirmwareFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hex => "Intel HEX",
            Self::Elf => "ELF",
            Self::Bin => "Raw Binary",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "hex" | "ihex" => Some(Self::Hex),
            "elf" | "axf" | "out" => Some(Self::Elf),
            "bin" => Some(Self::Bin),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FirmwareInfo {
    pub format: FirmwareFormat,
    pub size: u64,
    pub sha256: String,
    /// Contiguous image used for an independent post-flash readback check.
    pub image: Option<Vec<u8>>,
    pub image_base: Option<u64>,
    pub image_end: Option<u64>,
    pub records: usize,
    pub note: String,
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn inspect(path: &Path, format: FirmwareFormat, bin_base: u64) -> anyhow::Result<FirmwareInfo> {
    let data = std::fs::read(path)
        .with_context(|| format!("无法读取固件文件: {}", path.display()))?;
    let size = data.len() as u64;
    if size == 0 {
        bail!("固件文件为空");
    }

    let mut info = FirmwareInfo {
        format,
        size,
        sha256: sha256_hex(&data),
        image: None,
        image_base: None,
        image_end: None,
        records: 0,
        note: String::new(),
    };

    match format {
        FirmwareFormat::Hex => {
            let (base, image, records) = parse_hex(&data)?;
            info.image_base = Some(base);
            info.image_end = Some(base + image.len() as u64);
            info.records = records;
            info.image = Some(image);
            info.note = format!(
                "数据区间 0x{base:08X} .. 0x{:08X}，共 {} 字节",
                base + info.image.as_ref().unwrap().len() as u64 - 1,
                info.image.as_ref().unwrap().len()
            );
        }
        FirmwareFormat::Elf => {
            info.note = "可烧录段由 ELF program header 决定，probe-rs 会自动提取".to_owned();
            if data.starts_with(b"\x7fELF") {
                let class = data.get(4).copied().unwrap_or(0);
                info.note = format!(
                    "ELF{} 可执行映像，段地址由 program header 决定",
                    if class == 1 { "32" } else { "64" }
                );
            } else {
                bail!("文件扩展名为 ELF，但缺少 ELF 魔数 (0x7F 'ELF')");
            }
        }
        FirmwareFormat::Bin => {
            info.image_base = Some(bin_base);
            info.image_end = Some(bin_base + size);
            info.image = Some(data);
            info.note = format!("将写入 0x{bin_base:08X} .. 0x{:08X}", bin_base + size - 1);
        }
    }
    Ok(info)
}

/// Parse Intel HEX into one contiguous image; gaps are filled with 0xFF.
fn parse_hex(data: &[u8]) -> anyhow::Result<(u64, Vec<u8>, usize)> {
    let text = std::str::from_utf8(data).context("HEX 文件不是合法的文本")?;
    let mut bytes: BTreeMap<u64, u8> = BTreeMap::new();
    let mut ext: u64 = 0;
    let mut records = 0usize;

    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(':') || line.len() < 11 {
            bail!("HEX 第 {} 行格式错误", line_no + 1);
        }
        let raw_bytes = hex::decode(&line[1..])
            .map_err(|_| anyhow!("HEX 第 {} 行含非十六进制字符", line_no + 1))?;
        let count = raw_bytes[0] as usize;
        if raw_bytes.len() < count + 5 {
            bail!("HEX 第 {} 行长度不足", line_no + 1);
        }
        let addr = ((raw_bytes[1] as u64) << 8) | raw_bytes[2] as u64;
        let kind = raw_bytes[3];
        let payload = &raw_bytes[4..4 + count];
        records += 1;

        match kind {
            0x00 => {
                for (i, b) in payload.iter().enumerate() {
                    bytes.insert(ext + addr + i as u64, *b);
                }
            }
            0x01 => break, // EOF
            0x02 => {
                if payload.len() >= 2 {
                    ext = (((payload[0] as u64) << 8) | payload[1] as u64) << 4;
                }
            }
            0x04 => {
                if payload.len() >= 2 {
                    ext = (((payload[0] as u64) << 8) | payload[1] as u64) << 16;
                }
            }
            0x03 | 0x05 => {} // start address is informational only
            _ => {}
        }
    }

    if bytes.is_empty() {
        bail!("HEX 文件中没有数据记录");
    }
    let lo = *bytes.keys().next().unwrap();
    let hi = *bytes.keys().next_back().unwrap();
    let image: Vec<u8> = (lo..=hi).map(|a| *bytes.get(&a).unwrap_or(&0xFF)).collect();
    Ok((lo, image, records))
}

/// Minimal hex decoding without pulling in another crate.
mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        let b = s.as_bytes();
        if b.len() % 2 != 0 {
            return Err(());
        }
        let nibble = |c: u8| -> Result<u8, ()> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(()),
            }
        };
        let mut out = Vec::with_capacity(b.len() / 2);
        for i in (0..b.len()).step_by(2) {
            out.push((nibble(b[i])? << 4) | nibble(b[i + 1])?);
        }
        Ok(out)
    }
}
