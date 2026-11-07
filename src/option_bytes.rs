//! MCU option-byte (OB) configuration.
//!
//! The layout here is taken from Puya's own CMSIS headers
//! (`py32f003x8.h` -> `OB_TypeDef`) and `Puya.PY32F0xx_DFP.pdsc`:
//! the option bytes are a **16 byte** non-volatile block, and every
//! configuration byte is paired with its bitwise complement:
//!
//! ```text
//! 0x00 RDP        0x01 USER       0x02 nRDP       0x03 nUSER
//! 0x04 SDK_STRT   0x05 SDK_END   0x06 nSDK_STRT  0x07 nSDK_END
//! 0x08..0x0B reserved (0xFF)
//! 0x0C..0x0D WRP (u16 LE)        0x0E..0x0F nWRP (u16 LE)
//! ```
//!
//! `USER` mirrors `FLASH_OPTR[15:8]`: BOR_EN / BOR_LEV[2:0] / IWDG_SW /
//! WWDG_SW / NRST_MODE / nBOOT1. The block is **not** ordinary Flash: the
//! vendor ships a dedicated `*_OPT.FLM` algorithm for it, which is compiled
//! into the binary as `PY32_OPT_YAML` and used by `backend::write_option_bytes`.
//!
//! Everything in this module is pure data/logic so the risky parts (complement
//! generation, field encoding, change validation) stay unit-testable without
//! any hardware attached.

use std::fmt::Write as _;

/// Length of one PY32 option-byte block, as declared by the vendor pack.
pub const OB_IMAGE_LEN: usize = 16;
/// Variant name of the bundled option-byte flash algorithm description.
pub const PY32_OB_VARIANT: &str = "PY32F0xx-OB";
/// Bundled `PY32F0xx_OPT.FLM` -> probe-rs `ChipFamily` YAML. Kept in `assets/`
/// (not `targets/`) so it never appears as a selectable chip in the library.
pub const PY32_OPT_YAML: &str = include_str!("../assets/PY32F0xx_OPT.yaml");

/// A live read of a chip's option-byte block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObSnapshot {
    pub chip: String,
    pub base: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObKind {
    /// PY32F003 / PY32F030 / PY32F0xx: OB at 0x1FFF0E80, covered by the
    /// bundled `PY32F0xx_OPT.FLM`.
    Py32F0E80,
    /// PY32F002B: OB at 0x1FFF0080, needs `PY32F002Bxx_OPT.FLM`.
    Py32F002B,
    /// PY32F071 / PY32F072: OB at 0x1FFF3080, needs its own OPT.FLM.
    Py32F07x,
    /// Unknown chip: user supplies the base address; raw HEX editing only.
    Generic,
}

#[derive(Clone, Copy, Debug)]
pub struct ObProfile {
    pub kind: ObKind,
    pub label: &'static str,
    pub base: u64,
    /// True when a bundled, address-matching OPT algorithm can program it.
    pub writable: bool,
    pub note: &'static str,
}

const PROFILE_PY32_MAIN: ObProfile = ObProfile {
    kind: ObKind::Py32F0E80,
    label: "PY32F003 / F030 · 官方 SDK",
    base: 0x1FFF_0E80,
    writable: true,
    note: "Puya DFP 声明 0x1FFF0E80 / 16 字节，内置 PY32F0xx_OPT.FLM 可写入",
};
const PROFILE_PY32_002B: ObProfile = ObProfile {
    kind: ObKind::Py32F002B,
    label: "PY32F002B · 官方 SDK",
    base: 0x1FFF_0080,
    writable: false,
    note: "OB 在 0x1FFF0080，需要 PY32F002Bxx_OPT.FLM；当前仅支持读取与导出",
};
const PROFILE_PY32_07X: ObProfile = ObProfile {
    kind: ObKind::Py32F07x,
    label: "PY32F071 / F072 · 官方 SDK",
    base: 0x1FFF_3080,
    writable: false,
    note: "OB 在 0x1FFF3080，需要 PY32F071xx/F072xx_OPT.FLM；当前仅支持读取与导出",
};
const PROFILE_GENERIC: ObProfile = ObProfile {
    kind: ObKind::Generic,
    label: "通用 / 自定义基址",
    base: 0x1FFF_0E80,
    writable: false,
    note: "未内置该芯片的选项字节布局，请手填 OB 基址；仅支持读取与原始 HEX 编辑",
};

/// Pick a layout from the selected chip name. The PY32 sub-families really do
/// use different OB addresses, so they must not share one profile.
pub fn profile_for_chip(chip: &str) -> &'static ObProfile {
    let c = chip.trim().to_ascii_lowercase();
    if c.starts_with("py32f002b") {
        &PROFILE_PY32_002B
    } else if c.starts_with("py32f07") {
        &PROFILE_PY32_07X
    } else if c.starts_with("py32f0") {
        &PROFILE_PY32_MAIN
    } else {
        &PROFILE_GENERIC
    }
}

/// Accept `0x1FFF0E80`, `1FFF0E80` (hex when >= 8 digits), `536870912` or
/// underscore-separated numbers.
pub fn parse_base(text: &str) -> Option<u64> {
    let s = text.trim().replace('_', "");
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).ok();
    }
    s.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// 16-byte image
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObImage {
    pub bytes: [u8; OB_IMAGE_LEN],
}

impl Default for ObImage {
    /// Erased / unread state: all 0xFF. Not a valid programmed image (its
    /// complements are wrong); `factory_default()` is the valid baseline.
    fn default() -> Self {
        Self {
            bytes: [0xFF; OB_IMAGE_LEN],
        }
    }
}

impl ObImage {
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != OB_IMAGE_LEN {
            return None;
        }
        let mut out = [0u8; OB_IMAGE_LEN];
        out.copy_from_slice(bytes);
        Some(Self { bytes: out })
    }

    /// A conservative, valid "no protection" image: RDP level 0, all user
    /// bits 0 (hardware watchdog, reset pin as reset), no SDK / WRP protection.
    pub fn factory_default() -> Self {
        let mut image = Self {
            bytes: [0xFF; OB_IMAGE_LEN],
        };
        image.bytes[0] = 0xAA; // RDP level 0
        image.bytes[1] = 0x00; // USER
        image.bytes[4] = 0x00; // SDK_STRT
        image.bytes[5] = 0x00; // SDK_END
        image.bytes[12] = 0xFF; // WRP (low = protected)
        image.bytes[13] = 0xFF;
        image.refresh_complements();
        image
    }

    pub fn rdp(&self) -> u8 {
        self.bytes[0]
    }

    /// Recompute every complemented byte and force the reserved words to
    /// 0xFF. Called after any field edit, so the UI can never emit a block
    /// that the FLASH controller would reject with OPTVERR.
    pub fn refresh_complements(&mut self) {
        self.bytes[2] = !self.bytes[0];
        self.bytes[3] = !self.bytes[1];
        self.bytes[6] = !self.bytes[4];
        self.bytes[7] = !self.bytes[5];
        self.bytes[8..12].fill(0xFF);
        self.bytes[14] = !self.bytes[12];
        self.bytes[15] = !self.bytes[13];
    }

    pub fn complements_valid(&self) -> bool {
        self.bytes[2] == !self.bytes[0]
            && self.bytes[3] == !self.bytes[1]
            && self.bytes[6] == !self.bytes[4]
            && self.bytes[7] == !self.bytes[5]
            && self.bytes[8..12].iter().all(|b| *b == 0xFF)
            && self.bytes[14] == !self.bytes[12]
            && self.bytes[15] == !self.bytes[13]
    }

    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(OB_IMAGE_LEN * 3);
        for (i, b) in self.bytes.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let _ = write!(s, "{b:02X}");
        }
        s
    }

    /// Human-readable annotation of the 16 raw bytes.
    pub fn byte_labels() -> [&'static str; OB_IMAGE_LEN] {
        [
            "RDP", "USER", "nRDP", "nUSER", "SDK_STRT", "SDK_END", "nSDK_STRT",
            "nSDK_END", "保留", "保留", "保留", "保留", "WRP[7:0]", "WRP[15:8]",
            "nWRP[7:0]", "nWRP[15:8]",
        ]
    }
}

pub fn parse_hex_image(text: &str) -> Result<ObImage, String> {
    let bytes = crate::serial::parse_hex_text(text)?;
    ObImage::from_slice(&bytes)
        .ok_or_else(|| format!("选项字节必须正好 {OB_IMAGE_LEN} 字节，实际 {}", bytes.len()))
}

// ---------------------------------------------------------------------------
// Field model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct ObChoice {
    pub value: u32,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct ObFieldDef {
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// Index of the first (least significant) byte in the OB image.
    pub byte: usize,
    pub shift: u8,
    pub width: u8,
    pub choices: &'static [ObChoice],
    pub danger: bool,
}

const RDP_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0xAA, label: "Level 0 · 无读保护" },
    ObChoice { value: 0x55, label: "Level 1 · 读保护（会擦除 Flash）" },
];
const BOR_EN_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0, label: "禁用（使用默认阈值）" },
    ObChoice { value: 1, label: "启用（用选项字节阈值）" },
];
const BOR_LEV_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0, label: "1.7 – 1.8 V" },
    ObChoice { value: 1, label: "1.9 – 2.0 V" },
    ObChoice { value: 2, label: "2.1 – 2.2 V" },
    ObChoice { value: 3, label: "2.3 – 2.4 V" },
    ObChoice { value: 4, label: "2.5 – 2.6 V" },
    ObChoice { value: 5, label: "2.7 – 2.8 V" },
    ObChoice { value: 6, label: "2.9 – 3.0 V" },
    ObChoice { value: 7, label: "3.1 – 3.2 V" },
];
const SW_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0, label: "硬件看门狗" },
    ObChoice { value: 1, label: "软件看门狗" },
];
const RESET_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0, label: "NRST 作复位输入" },
    ObChoice { value: 1, label: "NRST 作 GPIO（PF2）" },
];
const BOOT1_CHOICES: &[ObChoice] = &[
    ObChoice { value: 0, label: "BOOT1 = 0 · SRAM 启动" },
    ObChoice { value: 1, label: "BOOT1 = 1 · 系统存储器" },
];
const NO_CHOICES: &[ObChoice] = &[];

/// `USER` mirrors FLASH_OPTR[15:8], so BOR_EN is bit 0 of the USER byte,
/// BOR_LEV bits 3:1, IWDG_SW bit 4, WWDG_SW bit 5, NRST_MODE bit 6,
/// nBOOT1 bit 7 (see py32f003x8.h).
const PY32_FIELDS: &[ObFieldDef] = &[
    ObFieldDef {
        id: "rdp",
        label: "读保护 RDP",
        help: "0xAA = Level 0；0x55 = Level 1。降级会触发整片擦除，升级会锁定调试访问。",
        byte: 0,
        shift: 0,
        width: 8,
        choices: RDP_CHOICES,
        danger: true,
    },
    ObFieldDef {
        id: "bor_en",
        label: "BOR 使能",
        help: "是否使用选项字节中配置的欠压复位阈值。",
        byte: 1,
        shift: 0,
        width: 1,
        choices: BOR_EN_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "bor_lev",
        label: "BOR 阈值",
        help: "欠压复位电压档位（仅 BOR 使能时有效）。",
        byte: 1,
        shift: 1,
        width: 3,
        choices: BOR_LEV_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "iwdg_sw",
        label: "独立看门狗",
        help: "硬件（上电即启动）或软件（由程序启动）。",
        byte: 1,
        shift: 4,
        width: 1,
        choices: SW_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "wwdg_sw",
        label: "窗口看门狗",
        help: "硬件或软件启动窗口看门狗。",
        byte: 1,
        shift: 5,
        width: 1,
        choices: SW_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "nrst_mode",
        label: "NRST 引脚",
        help: "设为 GPIO 后复位脚不再可用，务必确认板上还有其它复位手段。",
        byte: 1,
        shift: 6,
        width: 1,
        choices: RESET_CHOICES,
        danger: true,
    },
    ObFieldDef {
        id: "nboot1",
        label: "BOOT1",
        help: "启动模式选择；改错可能导致芯片不再运行用户程序。",
        byte: 1,
        shift: 7,
        width: 1,
        choices: BOOT1_CHOICES,
        danger: true,
    },
    ObFieldDef {
        id: "sdk_strt",
        label: "SDK 起始块",
        help: "安全区起始块号（0 – 31，2 KiB 粒度）。",
        byte: 4,
        shift: 0,
        width: 8,
        choices: NO_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "sdk_end",
        label: "SDK 结束块",
        help: "安全区结束块号（0 – 31）。",
        byte: 5,
        shift: 0,
        width: 8,
        choices: NO_CHOICES,
        danger: false,
    },
    ObFieldDef {
        id: "wrp",
        label: "写保护 WRP",
        help: "16 位掩码，某位为 0 表示对应扇区被写保护；0xFFFF = 不保护。",
        byte: 12,
        shift: 0,
        width: 16,
        choices: NO_CHOICES,
        danger: true,
    },
];

/// `Generic` chips have no verified bit layout, so only raw HEX editing is
/// offered instead of inventing field meanings.
pub fn fields_for(kind: ObKind) -> &'static [ObFieldDef] {
    match kind {
        ObKind::Generic => &[],
        _ => PY32_FIELDS,
    }
}

fn field_mask(width: u8) -> u32 {
    if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    }
}

fn byte_count(width: u8) -> usize {
    ((width + 7) / 8) as usize
}

/// Read a bit field, little-endian for fields wider than one byte.
pub fn read_field(image: &ObImage, def: &ObFieldDef) -> u32 {
    let mut raw = 0u32;
    for i in 0..byte_count(def.width) {
        let idx = def.byte + i;
        if let Some(b) = image.bytes.get(idx) {
            raw |= (*b as u32) << (8 * i);
        }
    }
    (raw >> def.shift) & field_mask(def.width)
}

/// Write a bit field and immediately refresh every complement byte.
pub fn write_field(image: &mut ObImage, def: &ObFieldDef, value: u32) -> Result<(), String> {
    let mask = field_mask(def.width);
    if value > mask {
        return Err(format!(
            "{} 取值范围 0x00 – 0x{mask:X}",
            def.label
        ));
    }
    let mut raw = 0u32;
    for i in 0..byte_count(def.width) {
        if let Some(b) = image.bytes.get(def.byte + i) {
            raw |= (*b as u32) << (8 * i);
        }
    }
    raw = (raw & !(mask << def.shift)) | (value << def.shift);
    for i in 0..byte_count(def.width) {
        let idx = def.byte + i;
        if idx < OB_IMAGE_LEN {
            image.bytes[idx] = ((raw >> (8 * i)) & 0xFF) as u8;
        }
    }
    image.refresh_complements();
    Ok(())
}

pub fn format_value(def: &ObFieldDef, value: u32) -> String {
    if let Some(c) = def.choices.iter().find(|c| c.value == value) {
        return c.label.to_owned();
    }
    if def.width > 8 {
        format!("0x{value:04X}")
    } else if def.width == 8 {
        format!("0x{value:02X}")
    } else {
        value.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObChange {
    pub label: String,
    pub from: String,
    pub to: String,
    pub danger: bool,
}

pub fn diff(kind: ObKind, old: &ObImage, new: &ObImage) -> Vec<ObChange> {
    fields_for(kind)
        .iter()
        .filter_map(|def| {
            let a = read_field(old, def);
            let b = read_field(new, def);
            (a != b).then(|| ObChange {
                label: def.label.to_owned(),
                from: format_value(def, a),
                to: format_value(def, b),
                danger: def.danger,
            })
        })
        .collect()
}

/// Guard rails before any hardware write. `acknowledged` is the explicit
/// "I understand the risk" checkbox from the UI, and is deliberately also
/// enforced here so a stray job cannot bypass it.
pub fn validate(old: &ObImage, new: &ObImage, acknowledged: bool) -> Result<(), String> {
    if old == new {
        return Err("没有需要写入的变更".to_owned());
    }
    // Check the all-zero case first: it is also complement-invalid, but the
    // specific "chip would never boot" message is far more useful than a
    // generic complement error.
    if new.bytes.iter().all(|b| *b == 0) {
        return Err("全 0x00 的选项字节块会让芯片无法启动，拒绝写入".to_owned());
    }
    if !new.complements_valid() {
        return Err("选项字节反码校验失败，拒绝写入".to_owned());
    }
    let rdp = new.rdp();
    if rdp != 0xAA && rdp != 0x55 {
        return Err(format!("非法的 RDP 值 0x{rdp:02X}（只允许 0xAA / 0x55）"));
    }
    if old.rdp() != rdp && !acknowledged {
        return Err("改变读保护等级需要勾选「我已了解风险」".to_owned());
    }
    Ok(())
}

/// Text report used by the headless `--option-bytes` command.
pub fn describe(snapshot: &ObSnapshot) -> String {
    let Some(image) = ObImage::from_slice(&snapshot.bytes) else {
        return format!("读取到 {} 字节，无法按 16 字节布局解析", snapshot.bytes.len());
    };
    let profile = profile_for_chip(&snapshot.chip);
    let mut lines = vec![
        format!("芯片 {} · {}", snapshot.chip, profile.label),
        format!("OB 区 0x{:08X} · {} 字节", snapshot.base, snapshot.bytes.len()),
        format!("原始: {}", image.hex()),
        format!("标注: {}", ObImage::byte_labels().join(" | ")),
        format!(
            "反码: {}",
            if image.complements_valid() {
                "校验通过"
            } else {
                "校验失败（保留字节或反码不匹配）"
            }
        ),
        String::new(),
    ];
    for def in fields_for(profile.kind) {
        lines.push(format!(
            "  {:<12} = {:<28} [{}]",
            def.label,
            format_value(def, read_field(&image, def)),
            def.help
        ));
    }
    if fields_for(profile.kind).is_empty() {
        lines.push("  该芯片无已核对的字段布局，仅显示原始字节".to_owned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py32_subfamilies_resolve_their_own_ob_address() {
        assert_eq!(profile_for_chip("PY32F003x8").base, 0x1FFF_0E80);
        assert_eq!(profile_for_chip("py32f030x6").base, 0x1FFF_0E80);
        assert!(profile_for_chip("PY32F003x8").writable);
        assert_eq!(profile_for_chip("PY32F002Bxx").base, 0x1FFF_0080);
        assert!(!profile_for_chip("PY32F002Bxx").writable);
        assert_eq!(profile_for_chip("PY32F072xB").base, 0x1FFF_3080);
        assert_eq!(profile_for_chip("STM32F103C8").kind, ObKind::Generic);
        assert!(!profile_for_chip("STM32F103C8").writable);
    }

    #[test]
    fn parse_base_accepts_prefixed_hex_and_decimal() {
        assert_eq!(parse_base("0x1FFF0E80"), Some(0x1FFF_0E80));
        assert_eq!(parse_base("0X1FFF_0E80"), Some(0x1FFF_0E80));
        assert_eq!(parse_base("536870912"), Some(0x2000_0000));
        assert_eq!(parse_base("nope"), None);
        assert_eq!(parse_base(""), None);
    }

    #[test]
    fn factory_default_has_valid_complements() {
        let image = ObImage::factory_default();
        assert!(image.complements_valid());
        assert_eq!(image.rdp(), 0xAA);
        assert_eq!(image.bytes[2], 0x55);
        assert_eq!(image.bytes[3], 0xFF);
        assert_eq!(&image.bytes[8..12], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(image.bytes[14..16], [0x00, 0x00]);
        // The all-0xFF "unread" default must NOT be considered valid.
        assert!(!ObImage::default().complements_valid());
    }

    #[test]
    fn editing_a_user_field_refreshes_all_complements() {
        let mut image = ObImage::factory_default();
        let iwdg = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "iwdg_sw")
            .unwrap();
        write_field(&mut image, iwdg, 1).unwrap();
        assert_eq!(image.bytes[1], 0x10);
        assert_eq!(image.bytes[3], 0xEF);
        assert!(image.complements_valid());
        assert_eq!(read_field(&image, iwdg), 1);
    }

    #[test]
    fn bor_level_occupies_bits_3_to_1_of_user_byte() {
        let mut image = ObImage::factory_default();
        let bor = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "bor_lev")
            .unwrap();
        write_field(&mut image, bor, 5).unwrap();
        assert_eq!(image.bytes[1], 0b0000_1010);
        assert_eq!(read_field(&image, bor), 5);
        // An adjacent field must survive the edit.
        let iwdg = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "iwdg_sw")
            .unwrap();
        write_field(&mut image, iwdg, 1).unwrap();
        assert_eq!(read_field(&image, bor), 5);
    }

    #[test]
    fn wrp_is_a_little_endian_sixteen_bit_field() {
        let mut image = ObImage::factory_default();
        let wrp = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "wrp")
            .unwrap();
        write_field(&mut image, wrp, 0x1234).unwrap();
        assert_eq!(image.bytes[12], 0x34);
        assert_eq!(image.bytes[13], 0x12);
        assert_eq!(image.bytes[14], 0xCB);
        assert_eq!(image.bytes[15], 0xED);
        assert!(image.complements_valid());
        assert_eq!(read_field(&image, wrp), 0x1234);
    }

    #[test]
    fn out_of_range_field_values_are_rejected() {
        let mut image = ObImage::factory_default();
        let bor = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "bor_lev")
            .unwrap();
        assert!(write_field(&mut image, bor, 9).is_err());
        assert_eq!(read_field(&image, bor), 0);
    }

    #[test]
    fn hex_image_round_trips_and_rejects_wrong_length() {
        let image = ObImage::factory_default();
        let parsed = parse_hex_image(&image.hex()).unwrap();
        assert_eq!(parsed, image);
        assert!(parse_hex_image("AA 55").is_err());
        assert!(parse_hex_image("ZZ").is_err());
        assert!(ObImage::from_slice(&[0u8; 15]).is_none());
    }

    #[test]
    fn complement_validation_catches_a_corrupted_block() {
        let mut image = ObImage::factory_default();
        image.bytes[2] = 0x00;
        assert!(!image.complements_valid());
        image.refresh_complements();
        assert!(image.complements_valid());
        image.bytes[9] = 0x00;
        assert!(!image.complements_valid());
    }

    #[test]
    fn diff_reports_only_changed_fields() {
        let old = ObImage::factory_default();
        let mut new = old;
        new.bytes[1] = 0x80;
        new.refresh_complements();
        let changes = diff(ObKind::Py32F0E80, &old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].label, "BOOT1");
        assert!(changes[0].danger);
        assert!(diff(ObKind::Py32F0E80, &old, &old).is_empty());
        assert!(diff(ObKind::Generic, &old, &new).is_empty());
    }

    #[test]
    fn rdp_change_requires_an_explicit_acknowledgement() {
        let old = ObImage::factory_default();
        let mut new = old;
        let rdp = fields_for(ObKind::Py32F0E80)
            .iter()
            .find(|f| f.id == "rdp")
            .unwrap();
        write_field(&mut new, rdp, 0x55).unwrap();
        assert!(validate(&old, &new, false).unwrap_err().contains("了解风险"));
        assert!(validate(&old, &new, true).is_ok());
        assert!(validate(&old, &old, true).unwrap_err().contains("没有需要写入"));
    }

    #[test]
    fn illegal_rdp_and_all_zero_images_are_rejected() {
        let old = ObImage::factory_default();
        let mut illegal = old;
        illegal.bytes[0] = 0x33;
        illegal.refresh_complements();
        assert!(validate(&old, &illegal, true).unwrap_err().contains("非法的 RDP"));
        let zeros = ObImage { bytes: [0; OB_IMAGE_LEN] };
        assert!(validate(&old, &zeros, true).unwrap_err().contains("无法启动"));
    }

    #[test]
    fn generic_chips_expose_no_invented_fields() {
        assert!(fields_for(ObKind::Generic).is_empty());
        assert_eq!(fields_for(ObKind::Py32F0E80).len(), 10);
        assert!(fields_for(ObKind::Py32F002B).len() > 0);
    }

    #[test]
    fn built_in_opt_algorithm_matches_the_ob_block() {
        assert!(PY32_OPT_YAML.contains("PY32F0xx-OB"));
        assert!(PY32_OPT_YAML.contains("0x1fff0e80"));
        assert!(PY32_OPT_YAML.contains("0x1fff0e90"));
        assert!(PY32_OPT_YAML.contains("page_size: 0x10"));
        assert!(PY32_OPT_YAML.contains("instructions: "));
        assert_eq!(PY32_OB_VARIANT, "PY32F0xx-OB");
    }

    #[test]
    fn describe_reports_raw_bytes_and_complement_status() {
        let snap = ObSnapshot {
            chip: "PY32F003x8".to_owned(),
            base: 0x1FFF_0E80,
            bytes: ObImage::factory_default().bytes.to_vec(),
        };
        let text = describe(&snap);
        assert!(text.contains("PY32F003x8"));
        assert!(text.contains("校验通过"));
        assert!(text.contains("读保护 RDP"));
        assert!(text.contains("标注: RDP | USER | nRDP"));
        // factory_default(): RDP=0xAA, USER=0x00, nRDP=0x55.
        assert!(text.contains("AA 00 55"));
    }
}
