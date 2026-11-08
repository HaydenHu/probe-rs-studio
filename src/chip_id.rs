//! Read-only target identification (chip ID).
//!
//! This module is pure data + decoding so every risky/board-dependent part of
//! the feature stays unit-testable without a probe attached. The actual reads
//! live in `backend::collect_chip_id`.
//!
//! There is no single universal "chip ID" register on ARM MCUs, so the report
//! combines four independent sources:
//!
//! * `DPIDR` (SWD Debug Port register 0, bank 0) - identifies the *debug port*
//!   designer through its JEP106 code, not the MCU vendor.
//! * `SCB->CPUID` at `0xE000_ED00` - identifies the Cortex-M core (all
//!   Cortex-M parts implement this).
//! * A CoreSight ROM table - generic manufacturer (JEP106) + part number,
//!   available on chips for which no vendor map is bundled.
//! * A vendor `DBGMCU_IDCODE` - `DEV_ID[11:0]` + `REV_ID[31:16]`. Addresses
//!   are chip-family specific.
//! * Vendor factory `UID` (96 bit) and flash-size register, when known.
//!
//! PY32 addresses come from Puya's own CMSIS headers (`py32f003x8.h`,
//! `py32f002ax5.h`, `py32f030x8.h` all agree) and `py32f0xx_hal.c`:
//! `DBGMCU_BASE = 0x40015800`, `UID_BASE = 0x1FFF0E00`,
//! `FLASHSIZE_BASE = 0x1FFF0FFC`. STM32 entries are the commonly documented
//! addresses and are explicitly labelled as "verify against the datasheet";
//! unknown families only read the two universal ARM registers plus any custom
//! address the user supplies.

use std::fmt::Write as _;

/// `SCB->CPUID`, present on every Cortex-M core.
pub const SCB_CPUID: u64 = 0xE000_ED00;
/// Vendor factory UID is 96 bits on the families listed here.
pub const UID_LEN: usize = 12;

/// DPIDR is DP register address 0 in bank 0 (bank-independent on ADIv5).
pub const DPIDR_ADDRESS: u8 = 0x0;
pub const DPIDR_BANK: u8 = 0x0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdKind {
    /// Puya PY32F002A/B, PY32F003, PY32F030, PY32F07x ...
    Py32F0,
    Stm32F1,
    Stm32F0F3,
    Stm32F4,
    /// No vendor map bundled: universal ARM registers only.
    Generic,
}

#[derive(Clone, Copy, Debug)]
pub struct ChipIdProfile {
    pub kind: IdKind,
    pub label: &'static str,
    /// Where the addresses came from, shown verbatim in the UI.
    pub source: &'static str,
    pub idcode: Option<u64>,
    pub uid: Option<u64>,
    pub flash_size: Option<u64>,
    pub note: &'static str,
}

const PROFILE_PY32: ChipIdProfile = ChipIdProfile {
    kind: IdKind::Py32F0,
    label: "PY32F0xx · Puya 官方 SDK",
    source: "Puya PY32F0xx CMSIS 头文件 + HAL v1.5.0",
    idcode: Some(0x4001_5800),
    uid: Some(0x1FFF_0E00),
    flash_size: Some(0x1FFF_0FFC),
    note: "DBGMCU_IDCODE / 96 位 UID / Flash 容量寄存器，全部只读",
};

const PROFILE_STM32F1: ChipIdProfile = ChipIdProfile {
    kind: IdKind::Stm32F1,
    label: "STM32F1 · 常见地址",
    source: "ST 参考手册常见地址（请按具体型号核对）",
    idcode: Some(0xE004_2000),
    uid: Some(0x1FFF_F7E8),
    flash_size: Some(0x1FFF_F7E0),
    note: "不同容量/子系列的系统存储区地址可能不同",
};

const PROFILE_STM32F0F3: ChipIdProfile = ChipIdProfile {
    kind: IdKind::Stm32F0F3,
    label: "STM32F0 / F3 · 常见地址",
    source: "ST 参考手册常见地址（请按具体型号核对）",
    idcode: Some(0xE004_2000),
    uid: Some(0x1FFF_F7AC),
    flash_size: Some(0x1FFF_F7CC),
    note: "不同容量/子系列的系统存储区地址可能不同",
};

const PROFILE_STM32F4: ChipIdProfile = ChipIdProfile {
    kind: IdKind::Stm32F4,
    label: "STM32F4 · 常见地址",
    source: "ST 参考手册常见地址（请按具体型号核对）",
    idcode: Some(0xE004_2000),
    uid: Some(0x1FFF_7A10),
    flash_size: Some(0x1FFF_7A22),
    note: "F4 的 Flash 容量寄存器为 16 位，单位 KiB",
};

const PROFILE_GENERIC: ChipIdProfile = ChipIdProfile {
    kind: IdKind::Generic,
    label: "通用 ARM Cortex-M",
    source: "ARM 架构通用寄存器",
    idcode: None,
    uid: None,
    flash_size: None,
    note: "未内置该型号的厂商 ID 寄存器映射，可在下方输入自定义地址",
};

/// Pick the register map for a chip-library name. Matching is deliberately
/// prefix-based: `PY32F003x8`, `PY32F003X8` and `PY32F030x6` all share one map.
pub fn profile_for_chip(chip: &str) -> ChipIdProfile {
    let name: String = chip
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_uppercase)
        .collect();
    if name.starts_with("PY32F0") {
        PROFILE_PY32
    } else if name.starts_with("STM32F1") {
        PROFILE_STM32F1
    } else if name.starts_with("STM32F0") || name.starts_with("STM32F3") {
        PROFILE_STM32F0F3
    } else if name.starts_with("STM32F4") {
        PROFILE_STM32F4
    } else {
        PROFILE_GENERIC
    }
}

/// One row in the report. `address` is `None` for DP registers (which are not
/// memory-mapped) and `raw` is always printable, even for multi-byte values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipIdField {
    pub label: String,
    pub address: Option<u64>,
    pub raw: String,
    pub detail: String,
}

impl ChipIdField {
    pub fn new(
        label: impl Into<String>,
        address: Option<u64>,
        raw: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            address,
            raw: raw.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipIdSnapshot {
    pub chip: String,
    pub profile: String,
    pub source: String,
    pub fields: Vec<ChipIdField>,
    /// Non-fatal failures (a vendor register that does not exist on this die).
    pub warnings: Vec<String>,
}

impl ChipIdSnapshot {
    pub fn new(chip: &str, profile: ChipIdProfile) -> Self {
        Self {
            chip: chip.to_owned(),
            profile: profile.label.to_owned(),
            source: profile.source.to_owned(),
            fields: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn push(&mut self, field: ChipIdField) {
        self.fields.push(field);
    }

    /// One-line identity used in the log panel and the status bar.
    pub fn identity(&self) -> String {
        let interesting = [
            "CPUID",
            "DBGMCU_IDCODE",
            "DPIDR",
            "UID",
            "Flash 容量",
            "ROM Table",
        ];
        let parts: Vec<String> = self
            .fields
            .iter()
            .filter(|f| interesting.iter().any(|n| f.label.contains(n)))
            .map(|f| format!("{}={}", f.label, f.raw))
            .collect();
        if parts.is_empty() {
            "未读到任何 ID 寄存器".to_owned()
        } else {
            parts.join(" · ")
        }
    }

    /// Plain-text report for `--chip-id`, also used by "复制全部".
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "目标芯片: {}", self.chip);
        let _ = writeln!(out, "寄存器映射: {} ({})", self.profile, self.source);
        let _ = writeln!(out, "{}", "─".repeat(72));
        for field in &self.fields {
            let addr = field
                .address
                .map(|a| format!("0x{a:08X}"))
                .unwrap_or_else(|| "DP 寄存器".to_owned());
            let _ = writeln!(out, "{:<18} {:<12} {}", field.label, addr, field.raw);
            if !field.detail.is_empty() {
                let _ = writeln!(out, "    {}", field.detail);
            }
        }
        for warning in &self.warnings {
            let _ = writeln!(out, "[WARN] {warning}");
        }
        out
    }
}

/// `DPIDR`: revision / part number / MINDP / DP version / JEP106 designer.
/// The designer name is resolved by probe-rs' JEP106 table in the backend and
/// passed in; `None` keeps the raw code so a pure unit test stays meaningful.
pub fn dpidr_detail(raw: u32, designer: Option<&str>) -> String {
    let revision = (raw >> 28) & 0xF;
    let part_no = (raw >> 20) & 0xFF;
    let min_dp = (raw >> 16) & 0x1;
    let version = (raw >> 12) & 0xF;
    let jep_cc = (raw >> 8) & 0xF;
    let jep_id = (raw >> 1) & 0x7F;
    let designer = designer
        .map(str::to_owned)
        .unwrap_or_else(|| format!("JEP106 cc={jep_cc} id=0x{jep_id:02X}"));
    format!(
        "DPv{version} · 设计者 {designer} · 部件号 0x{part_no:02X} · 修订 {revision} · MINDP={min_dp}"
    )
}

/// `SCB->CPUID`: implementer / variant / architecture / part number / revision.
pub fn cpuid_detail(raw: u32) -> String {
    let implementer = (raw >> 24) & 0xFF;
    let variant = (raw >> 20) & 0xF;
    let arch = (raw >> 16) & 0xF;
    let part = (raw >> 4) & 0xFFF;
    let revision = raw & 0xF;
    let core = match part {
        0xC20 => "Cortex-M0",
        0xC60 => "Cortex-M0+",
        0xC21 => "Cortex-M1",
        0xC23 => "Cortex-M3",
        0xC24 => "Cortex-M4",
        0xC27 => "Cortex-M7",
        0xD20 => "Cortex-M23",
        0xD21 => "Cortex-M33",
        0xD22 => "Cortex-M55",
        0xD23 => "Cortex-M85",
        _ => "未知内核（部件号未收录）",
    };
    let maker = if implementer == 0x41 {
        "ARM"
    } else {
        "非 ARM 实现者"
    };
    let arch_label = if arch == 0xF {
        "ARMv7-M 及以上"
    } else {
        "ARMv6-M"
    };
    format!(
        "{maker} {core} · 架构 {arch_label} (0x{arch:X}) · 部件号 0x{part:03X} · 版本 r{variant}p{revision}"
    )
}

/// Vendor `DBGMCU_IDCODE`: DEV_ID[11:0], REV_ID[31:16].
pub fn idcode_detail(raw: u32) -> String {
    let dev_id = raw & 0xFFF;
    let rev_id = (raw >> 16) & 0xFFFF;
    format!("DEV_ID=0x{dev_id:03X} · REV_ID=0x{rev_id:04X}（芯片型号 / 硅版本，以厂商手册比对）")
}

/// Compact uppercase hex used as the `raw` column for a UID row.
pub fn uid_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// 96-bit factory UID. Shown both as three little-endian words and as bytes.
pub fn uid_detail(bytes: &[u8]) -> String {
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let hex = uid_hex(bytes);
    let words_text: String = words
        .iter()
        .map(|w| format!("0x{w:08X}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("96 位唯一 ID · {hex} · 小端字 {words_text}")
}

/// Flash-size register is a 16-bit value in KiB.
pub fn flash_size_detail(raw: u32) -> String {
    let kib = raw & 0xFFFF;
    if kib == 0 || kib == 0xFFFF {
        "读数无效（全 0 / 全 F），该型号可能没有此寄存器".to_owned()
    } else {
        format!("{kib} KiB · {:.0} 字节", kib as f32 * 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py32_profile_uses_the_official_sdk_addresses() {
        let p = profile_for_chip("PY32F003x8");
        assert_eq!(p.kind, IdKind::Py32F0);
        assert_eq!(p.idcode, Some(0x4001_5800));
        assert_eq!(p.uid, Some(0x1FFF_0E00));
        assert_eq!(p.flash_size, Some(0x1FFF_0FFC));
        assert_eq!(profile_for_chip("py32f030x6").kind, IdKind::Py32F0);
        assert_eq!(profile_for_chip("PY32F002B").kind, IdKind::Py32F0);
    }

    #[test]
    fn unknown_chip_only_uses_universal_arm_registers() {
        let p = profile_for_chip("HC32F460");
        assert_eq!(p.kind, IdKind::Generic);
        assert!(p.idcode.is_none() && p.uid.is_none() && p.flash_size.is_none());
        assert!(p.note.contains("自定义"));
    }

    #[test]
    fn cpuid_decodes_a_cortex_m0_plus() {
        let detail = cpuid_detail(0x410C_C601);
        assert!(detail.contains("Cortex-M0+"), "{detail}");
        assert!(detail.contains("ARMv6-M"), "{detail}");
        assert!(detail.contains("0xC60"), "{detail}");
        assert!(cpuid_detail(0x410F_C241).contains("Cortex-M4"));
    }

    #[test]
    fn dpidr_decodes_version_and_designer() {
        // 0x2BA01477 is the classic ARM SW-DP: DPv1, part 0xBA, JEP106 4/0x3B.
        let detail = dpidr_detail(0x2BA0_1477, Some("ARM Ltd"));
        assert!(detail.contains("DPv1"), "{detail}");
        assert!(detail.contains("ARM Ltd"), "{detail}");
        assert!(detail.contains("0xBA"), "{detail}");
        assert!(dpidr_detail(0, None).contains("JEP106"));
    }

    #[test]
    fn idcode_uid_and_flash_size_decode() {
        assert!(idcode_detail(0x1000_0442).contains("DEV_ID=0x442"));
        assert!(idcode_detail(0x1000_0442).contains("REV_ID=0x1000"));
        let uid = [0x11, 0x22, 0x33, 0x44, 0xAA, 0xBB, 0xCC, 0xDD, 0x01, 0x02, 0x03, 0x04];
        let detail = uid_detail(&uid);
        assert!(detail.contains("11223344AABBCCDD01020304"), "{detail}");
        assert!(detail.contains("0x44332211"), "{detail}");
        assert!(flash_size_detail(64).contains("64 KiB"));
        assert!(flash_size_detail(0).contains("无效"));
    }

    #[test]
    fn report_contains_rows_warnings_and_identity() {
        let profile = profile_for_chip("PY32F003x8");
        let mut snap = ChipIdSnapshot::new("PY32F003x8", profile);
        snap.push(ChipIdField::new(
            "CPUID",
            Some(SCB_CPUID),
            "0x410CC601",
            cpuid_detail(0x410C_C601),
        ));
        snap.warnings.push("UID 读取失败".to_owned());
        let text = snap.text();
        assert!(text.contains("0xE000ED00"), "{text}");
        assert!(text.contains("Cortex-M0+"), "{text}");
        assert!(text.contains("[WARN] UID"));
        assert!(snap.identity().contains("CPUID=0x410CC601"));
    }
}
