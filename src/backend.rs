//! Hardware operations, executed on worker threads.
//!
//! Everything talks to the probe-rs *library* directly - no subprocess, no CLI,
//! no RPC. The UI thread only receives `Event`s over a channel.

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context as _, Result};
use probe_rs::{
    config::{Registry, TargetSelector},
    flashing::{
        self, BinLoader, BinOptions, DownloadOptions, ElfLoader, ElfOptions, FlashProgress,
        HexLoader, ProgressEvent,
    },
    probe::{list::Lister, DebugProbeInfo, DebugProbeSelector, Probe, WireProtocol},
    MemoryInterface, Permissions, Session,
};
use sha2::{Digest, Sha256};

use crate::{
    chip_id::{self, ChipIdField, ChipIdProfile, ChipIdSnapshot},
    firmware::{self, FirmwareFormat, FirmwareInfo},
    option_bytes::{self, ObImage, ObSnapshot, OB_IMAGE_LEN},
    pack::FlmSpec,
};

/// Older settings files have no `read_chip_id_before_flash` key. Keep it off:
/// the standalone chip-ID window is the primary entry point, and an opt-in
/// pre-flash check must never change the behaviour of an existing flash flow
/// (extra SWD traffic matters on easily upset DAPLink probes).
fn default_read_chip_id() -> bool {
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Protocol {
    Swd,
    Jtag,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FlashConfig {
    pub probe_index: usize,
    /// Stable identity of the selected probe. Survives re-enumeration, so a
    /// hot-unplug/replug cannot make us silently attach to a different device.
    #[serde(default)]
    pub probe_id: Option<String>,
    /// Address of a network CMSIS-DAP probe, e.g. `dap.local` for an
    /// elaphureLink wireless debugger. Empty means USB probes only.
    /// `probe_id` decides whether it is actually the selected probe.
    /// Defaults to the same address as OpenOCD's `elaphurelink.cfg`, so the
    /// network probe is visible in the dropdown without any setup.
    #[serde(default = "default_network_probe")]
    pub network_probe: String,
    pub chip: String,
    pub protocol: Protocol,
    pub speed_khz: u32,
    pub connect_under_reset: bool,
    pub chip_description: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
    pub format: FirmwareFormat,
    pub bin_base: u64,
    pub verify: bool,
    pub restore_unwritten: bool,
    pub chip_erase: bool,
    pub preverify: bool,
    pub reset_after: bool,
    pub verify_readback: bool,
    /// Read the target's ID registers and log its identity before programming,
    /// so a mismatched chip selection is visible in the log before any write.
    #[serde(default = "default_read_chip_id")]
    pub read_chip_id_before_flash: bool,
    pub readback_address: u64,
    pub readback_size: u64,
    pub backup_path: PathBuf,
}

/// OpenOCD's `elaphurelink.cfg` uses `dap.local` as the default device address.
pub fn default_network_probe() -> String {
    "dap.local".to_owned()
}

impl Default for FlashConfig {
    fn default() -> Self {
        Self {
            probe_index: 0,
            probe_id: None,
            network_probe: default_network_probe(),
            chip: "PY32F003x8".to_owned(),
            protocol: Protocol::Swd,
            speed_khz: 1000,
            connect_under_reset: false,
            chip_description: None,
            firmware: None,
            format: FirmwareFormat::Hex,
            bin_base: 0x0800_0000,
            verify: true,
            restore_unwritten: true,
            chip_erase: false,
            preverify: false,
            reset_after: true,
            verify_readback: true,
            read_chip_id_before_flash: false,
            readback_address: 0x0800_0000,
            readback_size: 4096,
            backup_path: PathBuf::from("backup_before.bin"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProbeItem {
    /// Stable identity: VID:PID:serial:interface:transport.
    pub id: String,
    pub label: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ChipItem {
    pub family: String,
    pub name: String,
    /// True when the family comes from a bundled/user CMSIS-Pack description
    /// rather than from probe-rs' built-in target registry.
    pub external: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
    Debug,
}

#[derive(Clone, Debug)]
pub enum Job {
    RefreshProbes,
    LoadChips,
    ChipInfo {
        chip: String,
        cfg: Box<FlashConfig>,
    },
    /// Import a user-supplied CMSIS-Pack into `targets/`.
    LoadPack(PathBuf),
    /// Turn a standalone `.FLM` into a chip family with user-supplied
    /// chip name / core / RAM layout.
    LoadFlm {
        path: PathBuf,
        spec: Box<FlmSpec>,
    },
    Connect(Box<FlashConfig>),
    /// Read-only network reachability / handshake diagnostic.
    NetworkCheck(String),
    ReadBack(Box<FlashConfig>),
    Flash(Box<FlashConfig>),
    VerifyFirmware(Box<FlashConfig>),
    EraseAll(Box<FlashConfig>),
    Reset(Box<FlashConfig>),
    /// Read-only chip identification: DPIDR / CPUID / vendor IDCODE / UID /
    /// flash-size register, plus an optional user-supplied address.
    ReadChipId {
        cfg: Box<FlashConfig>,
        custom: Option<u64>,
    },
    /// Read the 16-byte option-byte block (read-only, safe on any chip).
    ReadOptionBytes {
        cfg: Box<FlashConfig>,
        base: u64,
    },
    /// Program the option-byte block with the bundled OPT.FLM algorithm.
    /// `acknowledged` mirrors the UI's "I understand the risk" checkbox and is
    /// re-validated here so a stray job cannot bypass it.
    WriteOptionBytes {
        cfg: Box<FlashConfig>,
        base: u64,
        image: ObImage,
        acknowledged: bool,
    },
}

pub enum Event {
    Log(Level, String),
    Probes(Vec<ProbeItem>),
    /// Live reachability of the configured network probe. Emitted only when
    /// the online/offline state changes, so the UI does not repaint per tick.
    NetworkProbeStatus {
        address: String,
        online: bool,
        detail: String,
    },
    /// A confirmed hot-plug / hot-unplug transition from the watcher.
    ProbeHotplug {
        items: Vec<ProbeItem>,
        added: Vec<String>,
        removed: Vec<String>,
    },
    Chips(Vec<ChipItem>),
    ChipDetails(String),
    /// Fresh list of currently attached serial ports (see `spawn_serial_watcher`).
    SerialPorts(Vec<crate::serial::SerialPort>),
    /// Bytes received by the open serial session.
    SerialRx(Vec<u8>),
    /// Number of bytes actually written by the serial session.
    SerialTxCount(usize),
    /// Session ended; the string is empty for a user-initiated close.
    SerialClosed(String),
    /// A fresh, read-only identification report for the attached target.
    ChipId(ChipIdSnapshot),
    /// A fresh read of the chip's option-byte block.
    OptionBytes(ObSnapshot),
    Connected(String),
    Progress { value: f32, label: String },
    Finished { ok: bool, summary: String, elapsed: f32 },
}

fn log(tx: &Sender<Event>, level: Level, msg: impl Into<String>) {
    let _ = tx.send(Event::Log(level, msg.into()));
}

fn progress(tx: &Sender<Event>, value: f32, label: impl Into<String>) {
    let _ = tx.send(Event::Progress {
        value: value.clamp(0.0, 1.0),
        label: label.into(),
    });
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn spawn(job: Job, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = run(job, &tx);
        let elapsed = started.elapsed().as_secs_f32();
        match result {
            Ok(summary) => {
                log(&tx, Level::Ok, format!("完成 · {summary}"));
                let _ = tx.send(Event::Finished {
                    ok: true,
                    summary,
                    elapsed,
                });
            }
            Err(err) => {
                log(&tx, Level::Error, format!("{err:#}"));
                let _ = tx.send(Event::Finished {
                    ok: false,
                    summary: format!("{err}"),
                    elapsed,
                });
            }
        }
    });
}

/// Headless end-to-end check of the very same code path the GUI uses.
/// Invoked with `--selftest <firmware> [chip]`.
pub fn selftest(cfg: &FlashConfig, write_flash: bool) -> Result<()> {
    use std::sync::mpsc::Receiver;

    fn drain(rx: &Receiver<Event>) {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Log(level, msg) => {
                    let tag = match level {
                        Level::Info => "INFO",
                        Level::Ok => "OK  ",
                        Level::Warn => "WARN",
                        Level::Error => "ERR ",
                        Level::Debug => "DBG ",
                    };
                    println!("    [{tag}] {msg}");
                }
                Event::Progress { value, label } => {
                    println!("    [{:3.0}%] {label}", value * 100.0);
                }
                Event::Finished { ok, summary, elapsed } => {
                    println!(
                        "    [{}] {} ({:.2}s)",
                        if ok { "DONE" } else { "FAIL" },
                        summary,
                        elapsed
                    );
                }
                _ => {}
            }
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let mut steps: Vec<(&str, Job)> = vec![
        ("探针枚举", Job::RefreshProbes),
        ("芯片描述库", Job::LoadChips),
        ("连接测试", Job::Connect(Box::new(cfg.clone()))),
        ("读回备份", Job::ReadBack(Box::new(cfg.clone()))),
    ];
    if write_flash {
        steps.push(("烧录固件", Job::Flash(Box::new(cfg.clone()))));
        steps.push(("独立读回校验", Job::VerifyFirmware(Box::new(cfg.clone()))));
    } else {
        steps.push(("独立读回校验", Job::VerifyFirmware(Box::new(cfg.clone()))));
    }

    for (name, job) in steps {
        println!("\n=== {name} ===");
        let result = run(job, &tx);
        drain(&rx);
        match result {
            Ok(summary) => println!("[PASS] {name}: {summary}"),
            Err(e) => return Err(e.context(format!("{name} 失败"))),
        }
    }
    println!("\n=== 自检完成 ===");
    Ok(())
}

/// How many times a probe I/O job is attempted before giving up. Cheap
/// CMSIS-DAP v1 adapters (DAPLink clones among them) occasionally drop a HID
/// report or answer a command with a stale one; a whole retry of an idempotent
/// operation recovers without the user having to press the button again.
const PROBE_JOB_ATTEMPTS: usize = 3;
const PROBE_RETRY_DELAY: Duration = Duration::from_millis(300);

/// Whether a failed job may safely be run again from the beginning.
///
/// Flash programming rewrites the same image, read-back/verify/connect are
/// read-only, and erase/reset are idempotent. Option-byte *writes* are excluded:
/// repeating a programming cycle on the configuration NVM is not worth the
/// risk of leaving the chip in a locked state.
fn job_is_retryable(job: &Job) -> bool {
    matches!(
        job,
        Job::Connect(_)
            | Job::ReadBack(_)
            | Job::Flash(_)
            | Job::VerifyFirmware(_)
            | Job::EraseAll(_)
            | Job::Reset(_)
            | Job::ReadChipId { .. }
            | Job::ReadOptionBytes { .. }
    )
}

/// A probe whose HID endpoint stopped answering altogether. Replaying the
/// whole job cannot fix this (the device needs a power cycle / re-enumeration),
/// so it is deliberately excluded from `is_transient_probe_error`.
#[derive(Debug)]
struct ProbeUnresponsive(String);

impl std::fmt::Display for ProbeUnresponsive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "探针固件无响应（HID 超时）· 请重新插拔 DAPLink USB 让它重新上电；\
             若刚用过 pyOCD / OpenOCD / 其它调试工具，请确认它们已退出。\
             这种状态只靠重试命令无法恢复，需要探针重新枚举\n原始错误：{}",
            self.0
        )
    }
}

impl std::error::Error for ProbeUnresponsive {}

/// Only transport-level hiccups justify a retry. A missing firmware file, a
/// failed verification or a rejected option-byte image must surface at once.
fn is_transient_probe_error(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<ProbeUnresponsive>().is_some() {
        return false;
    }
    let text = format!("{err:#}").to_lowercase();
    const HINTS: &[&str] = &[
        "command id in response",
        "communication with",
        "arm specific error",
        "no target responded",
        "did not respond to request",
        "debug probe encountered an error",
        "timeout",
        "timedout",
        "usb error",
    ];
    HINTS.iter().any(|hint| text.contains(hint))
}

fn run(job: Job, tx: &Sender<Event>) -> Result<String> {
    let attempts = if job_is_retryable(&job) {
        PROBE_JOB_ATTEMPTS
    } else {
        1
    };
    for attempt in 1..=attempts {
        let result = run_once(job.clone(), tx);
        let Err(err) = result else {
            return result;
        };
        if attempt == attempts || !is_transient_probe_error(&err) {
            return Err(err);
        }
        // Back off between attempts: a DAPLink that just dropped its HID
        // queue needs progressively longer to become responsive again.
        let delay = PROBE_RETRY_DELAY * attempt as u32;
        log(
            tx,
            Level::Warn,
            format!(
                "探针通信中断（第 {attempt}/{attempts} 次失败），{} ms 后自动重试：{err:#}",
                delay.as_millis()
            ),
        );
        std::thread::sleep(delay);
    }
    unreachable!("retry loop always returns")
}

fn run_once(job: Job, tx: &Sender<Event>) -> Result<String> {
    match job {
        Job::RefreshProbes => refresh_probes(tx),
        Job::LoadChips => load_chips(tx),
        Job::ChipInfo { chip, cfg } => chip_info(&chip, Some(cfg.as_ref()), tx),
        Job::LoadPack(path) => load_pack(&path, tx),
        Job::LoadFlm { path, spec } => load_flm(&path, &spec, tx),
        Job::Connect(cfg) => {
            let mut session = connect(&cfg, tx)?;
            let summary = describe_session(&mut session)?;
            Ok(summary)
        }
        Job::NetworkCheck(address) => {
            for line in network_check(&address).lines() {
                log(tx, Level::Info, line.to_owned());
            }
            Ok("网络探针诊断完成".to_owned())
        }
        Job::ReadBack(cfg) => read_back(&cfg, tx),
        Job::Flash(cfg) => flash(&cfg, tx),
        Job::VerifyFirmware(cfg) => {
            let (expected, base) = expected_image(&cfg)?;
            let mut session = connect(&cfg, tx)?;
            verify_readback(&mut session, base, &expected, tx)
        }
        Job::EraseAll(cfg) => {
            let mut session = connect(&cfg, tx)?;
            log(tx, Level::Warn, "执行整片擦除（不可逆）…");
            let mut p = FlashProgress::empty();
            flashing::erase_all(&mut session, &mut p, false).context("整片擦除失败")?;
            Ok("整片擦除完成".to_owned())
        }
        Job::Reset(cfg) => {
            let mut session = connect(&cfg, tx)?;
            reset_target(&mut session, tx)?;
            Ok("目标已复位并运行".to_owned())
        }
        Job::ReadChipId { cfg, custom } => read_chip_id(&cfg, custom, tx),
        Job::ReadOptionBytes { cfg, base } => read_option_bytes(&cfg, base, tx),
        Job::WriteOptionBytes {
            cfg,
            base,
            image,
            acknowledged,
        } => write_option_bytes(&cfg, base, image, acknowledged, tx),
    }
}

/// A stable identity for a physical probe, independent of USB enumeration
/// order. Re-plugging the same DAPLink produces the same string.
fn probe_identity(p: &DebugProbeInfo) -> String {
    format!(
        "{:04x}:{:04x}:{}:{}:{}",
        p.vendor_id,
        p.product_id,
        p.serial_number.as_deref().unwrap_or(""),
        p.interface.map(|i| i.to_string()).unwrap_or_default(),
        if p.is_hid_interface { "hid" } else { "usb" },
    )
}

fn probe_item(p: &DebugProbeInfo) -> ProbeItem {
    ProbeItem {
        id: probe_identity(p),
        label: format!("{}  [{:04x}:{:04x}]", p.identifier, p.vendor_id, p.product_id),
        detail: format!(
            "{} · {}",
            p.serial_number.as_deref().unwrap_or("无序列号"),
            if p.is_hid_interface {
                "CMSIS-DAP v1 (HID)"
            } else {
                "USB"
            }
        ),
    }
}

/// Placeholder USB ids probe-rs uses for network CMSIS-DAP backends.
const NETWORK_TCP_PID: u16 = 0x0001;
const NETWORK_ELAPHURELINK_PID: u16 = 0x0002;

/// Parse a network probe entry into a probe-rs selector.
///
/// A bare address (`dap.local`, `192.168.1.50:3240`) means elaphureLink;
/// `elaphurelink:<addr>` and `tcp:<addr>` select the backend explicitly.
/// probe-rs carries the address in the selector's serial number, which is why
/// the vendor/product ids are placeholders rather than real USB ids.
pub fn parse_network_probe(text: &str) -> Option<DebugProbeSelector> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (product_id, address) = match text.split_once(':') {
        Some(("elaphurelink", addr)) if !addr.is_empty() => (NETWORK_ELAPHURELINK_PID, addr),
        Some(("tcp", addr)) if !addr.is_empty() => (NETWORK_TCP_PID, addr),
        // An explicit backend with no address is a typo, not a host name.
        Some(("elaphurelink" | "tcp", _)) => return None,
        // A bare IPv4 address with a port also splits on ':', so anything
        // that is not an explicit prefix is used verbatim.
        _ => (NETWORK_ELAPHURELINK_PID, text),
    };
    Some(DebugProbeSelector {
        vendor_id: 0x0000,
        product_id,
        serial_number: Some(address.to_string()),
        interface: None,
        usb_location: None,
    })
}

/// Stable list identity of a network probe, in the same format as
/// [`probe_identity`] produces for a physical probe.
pub fn network_probe_identity(selector: &DebugProbeSelector) -> String {
    format!(
        "{:04x}:{:04x}:{}::usb",
        selector.vendor_id,
        selector.product_id,
        selector.serial_number.as_deref().unwrap_or(""),
    )
}

/// A dropdown entry for the configured network probe, so it can be selected
/// alongside USB probes. Network probes cannot be enumerated by USB polling.
pub fn network_probe_entry(text: &str) -> Option<ProbeItem> {
    let selector = parse_network_probe(text)?;
    let address = selector.serial_number.clone()?;
    let name = if selector.product_id == NETWORK_TCP_PID {
        "CMSIS-DAP TCP"
    } else {
        "elaphureLink"
    };
    Some(ProbeItem {
        id: network_probe_identity(&selector),
        label: format!("{name}  [0000:{:04x}]", selector.product_id),
        detail: format!("{address} · 网络 CMSIS-DAP 探针（不参与 USB 热插拔检测）"),
    })
}

/// Diagnose a configured network probe without opening it through probe-rs.
///
/// "connection timed out" can come from mDNS resolution, the TCP handshake or
/// the elaphureLink application handshake. Reporting each stage separately,
/// plus processes already holding a connection to that port, turns a generic
/// timeout into an actionable message.
pub fn network_check(address: &str) -> String {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let mut out = String::new();
    let Some(selector) = parse_network_probe(address) else {
        return format!("网络探针诊断: 地址「{address}」为空或格式无效");
    };
    let host = selector.serial_number.as_deref().unwrap_or("");
    let elaphure = selector.product_id == NETWORK_ELAPHURELINK_PID;
    let port = if elaphure { 3240u16 } else { 4441u16 };
    out.push_str(&format!(
        "网络探针诊断: {address}（{} · TCP {port}）\n",
        if elaphure { "elaphureLink" } else { "CMSIS-DAP TCP" }
    ));

    // Stage 1: DNS / mDNS lookup. Windows .local resolution can take seconds,
    // which is part of why a failed GUI connection felt like a hang.
    let started = Instant::now();
    let resolved: Vec<_> = match host.to_socket_addrs() {
        Ok(list) => list.collect(),
        Err(_) => match (host, port).to_socket_addrs() {
            Ok(list) => list.collect(),
            Err(err) => {
                out.push_str(&format!("[1/3] 地址解析失败: {err}\n"));
                out.push_str(
                    "结论: mDNS `.local` 未发现设备或 DNS 不通；确认设备已上电并连上同一局域网，或直接填设备 IP。\n",
                );
                return out;
            }
        },
    };
    if resolved.is_empty() {
        out.push_str("[1/3] 地址解析结果为空\n结论: 未解析到任何 IP，请改用设备 IP。\n");
        return out;
    }
    let list = resolved
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "[1/3] 解析 {host} → {list}（{:.2} s）\n",
        started.elapsed().as_secs_f32()
    ));

    // Stage 2: TCP connect to each resolved address.
    let mut stream = None;
    for addr in &resolved {
        let t = Instant::now();
        match TcpStream::connect_timeout(addr, Duration::from_secs(2)) {
            Ok(s) => {
                out.push_str(&format!(
                    "[2/3] TCP 连接 {addr} → 成功（{:.2} s）\n",
                    t.elapsed().as_secs_f32()
                ));
                stream = Some(s);
                break;
            }
            Err(err) => out.push_str(&format!(
                "[2/3] TCP 连接 {addr} → {err}（{:.2} s）\n",
                t.elapsed().as_secs_f32()
            )),
        }
    }
    let Some(mut stream) = stream else {
        let owners = port_owners(port);
        if !owners.is_empty() {
            out.push_str(&format!(
                "[占用] 本机已有进程连着 TCP {port}: {}\n",
                owners.join("、")
            ));
        }
        if let Some(listener) = local_listener(port) {
            out.push_str(&format!(
                "[注意] 本机 TCP {port} 正由 {listener} 监听；填 127.0.0.1 / localhost 会连到它而不是无线 DAP\n"
            ));
        }
        out.push_str(
            "结论: probe-rs 还没发出握手就失败了。elaphureLink 代理通常只接受一个客户端——若上方列出已连接进程，先退出它；若设备刚被异常断开，请给无线 DAP 断电重启（Web 配置页能打开不代表 DAP 服务已启动）。\n",
        );
        return out;
    };

    if !elaphure {
        out.push_str(
            "[3/3] 端口可达；CMSIS-DAP TCP backend 使用 DAP 帧，可直接尝试连接测试。\n",
        );
        return out;
    }

    // Stage 3: application-level handshake. This connection is closed as soon
    // as the report is done, so it never blocks a later real connection.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let request = [0x8a, 0x65, 0x6c, 0x70, 0, 0, 0, 0, 0, 1, 0, 0];
    let mut reply = [0u8; 12];
    match stream
        .write_all(&request)
        .and_then(|_| stream.read_exact(&mut reply))
    {
        Ok(_) if reply[..8] == [0x8a, 0x65, 0x6c, 0x70, 0, 0, 0, 0] => {
            let version = u32::from_be_bytes(reply[8..12].try_into().unwrap());
            out.push_str(&format!(
                "[3/3] elaphureLink 握手成功（代理版本 {}.{}）\n",
                (version >> 16) & 0xffff,
                (version >> 8) & 0xff
            ));
            let mut scope = [0u8; 4];
            match stream
                .write_all(&[0x88, 0x02, 0, 0])
                .and_then(|_| stream.read_exact(&mut scope))
            {
                Ok(_) => out.push_str(&format!(
                    "vendor scope 回复 {:02x?}（status {}）\n",
                    scope, scope[1]
                )),
                Err(err) => out.push_str(&format!("vendor scope 无响应: {err}\n")),
            }
            out.push_str("结论: 设备协议正常，可用「连接测试」连接该探针。\n");
        }
        Ok(_) => out.push_str(&format!(
            "[3/3] 端口可达但回执不是 elaphureLink 握手: {:02x?}\n结论: TCP {port} 上是别的服务。\n",
            &reply[..8]
        )),
        Err(err) => {
            out.push_str(&format!("[3/3] TCP 已连通，但没有收到 12 字节握手: {err}\n"));
            out.push_str(
                "结论: 端口上的服务不是可用的 elaphureLink 代理（usbipd-win 也默认占用 3240），或设备端 DAP 服务卡死，需要给设备断电重启。\n",
            );
        }
    }
    out
}

/// Lines from `netstat -ano`; IP/state/PID fields are ASCII on every locale.
fn netstat_lines() -> Vec<String> {
    std::process::Command::new("netstat")
        .args(["-ano"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|line| line.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn pid_name(pid: u32) -> String {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            let first = text.lines().next()?;
            Some(
                first
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_owned(),
            )
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "未知进程".to_owned())
}

/// Processes holding an ESTABLISHED connection to `port` (the usual reason a
/// single-client elaphureLink proxy refuses a second connection).
fn port_owners(port: u16) -> Vec<String> {
    let needle = format!(":{port}");
    let mut pids = Vec::new();
    for line in netstat_lines() {
        let upper = line.to_ascii_uppercase();
        if upper.contains("ESTABLISHED") && line.contains(&needle) {
            if let Some(pid) = line
                .split_whitespace()
                .last()
                .and_then(|s| s.parse::<u32>().ok())
            {
                if !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids.into_iter()
        .map(|pid| format!("{} (PID {pid})", pid_name(pid)))
        .collect()
}

/// Process listening locally on `port`, if any. usbipd-win also uses 3240,
/// so pointing the address at localhost silently reaches the wrong service.
fn local_listener(port: u16) -> Option<String> {
    let needle = format!(":{port}");
    for line in netstat_lines() {
        let upper = line.to_ascii_uppercase();
        if upper.contains("LISTENING") && line.contains(&needle) {
            if let Some(pid) = line
                .split_whitespace()
                .last()
                .and_then(|s| s.parse::<u32>().ok())
            {
                return Some(format!("{} (PID {pid})", pid_name(pid)));
            }
        }
    }
    None
}

/// Enumerate probes without opening them. Safe to call repeatedly from a
/// polling thread; it never claims the USB interface.
fn snapshot_probes() -> Vec<ProbeItem> {
    Lister::new()
        .list_all()
        .iter()
        .map(probe_item)
        .collect()
}

/// Compare attached probe sets (order-insensitive: USB enumeration order can
/// change without any physical plug/unplug).
fn same_probes(a: &[ProbeItem], b: &[ProbeItem]) -> bool {
    fn ids(items: &[ProbeItem]) -> Vec<&str> {
        let mut v: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        v.sort_unstable();
        v
    }
    ids(a) == ids(b)
}

/// Labels of probes that appeared (plugged in) and disappeared (unplugged).
fn probe_diff(previous: &[ProbeItem], current: &[ProbeItem]) -> (Vec<String>, Vec<String>) {
    let added = current
        .iter()
        .filter(|n| !previous.iter().any(|o| o.id == n.id))
        .map(|n| n.label.clone())
        .collect();
    let removed = previous
        .iter()
        .filter(|o| !current.iter().any(|n| n.id == o.id))
        .map(|o| o.label.clone())
        .collect();
    (added, removed)
}

/// Shared switches for the background watcher. Polling is suspended while a
/// flashing job owns the USB device, and can be turned off from the settings UI.
#[derive(Default)]
pub struct WatcherControl {
    pub enabled: AtomicBool,
    pub busy: AtomicBool,
}

impl WatcherControl {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            busy: AtomicBool::new(false),
        }
    }
}

/// Poll the OS serial-port list and push an event whenever a COM port appears,
/// disappears or changes state (e.g. a DAPLink virtual COM port being opened
/// by another program). Registry + SetupAPI reads never touch the debug probe,
/// so this keeps running while a flashing job owns the debugger.
pub fn spawn_serial_watcher(
    tx: Sender<Event>,
    control: Arc<WatcherControl>,
    interval: Duration,
    notify: impl Fn() + Send + Sync + 'static,
) {
    let _ = std::thread::Builder::new()
        .name("serial-port-watch".to_owned())
        .spawn(move || {
            // (COM names, full signatures): names drive the log, signatures
            // also catch state changes such as "port now busy".
            let mut baseline: Option<(Vec<String>, Vec<String>)> = None;
            loop {
                if !control.enabled.load(Ordering::Relaxed) {
                    std::thread::sleep(interval);
                    continue;
                }
                let ports = crate::serial::enumerate();
                let names: Vec<String> = ports.iter().map(|p| p.name.clone()).collect();
                let signatures: Vec<String> =
                    ports.iter().map(|p| p.signature()).collect();
                let changed = baseline
                    .as_ref()
                    .map_or(true, |(_, sigs)| *sigs != signatures);
                if changed {
                    if let Some((previous, _)) = &baseline {
                        for added in names.iter().filter(|n| !previous.contains(n)) {
                            log(&tx, Level::Ok, format!("检测到串口插入: {added}"));
                        }
                        for removed in previous.iter().filter(|n| !names.contains(n)) {
                            log(&tx, Level::Warn, format!("检测到串口拔出: {removed}"));
                        }
                    }
                    baseline = Some((names, signatures));
                    let _ = tx.send(Event::SerialPorts(ports));
                    notify();
                }
                std::thread::sleep(interval);
            }
        });
}

/// Commands sent from the UI to the serial-session worker thread.
pub enum SerialCommand {
    Send(Vec<u8>),
    Close,
}

/// Own an open COM port in a background thread: poll the input queue, write
/// outgoing bytes and report both directions as events. `Port`'s `Drop`
/// closes the handle when this loop ends.
pub fn spawn_serial_session(
    name: String,
    cfg: crate::serial::SerialSettings,
    tx: Sender<Event>,
    rx: Receiver<SerialCommand>,
) {
    let _ = std::thread::Builder::new()
        .name("serial-session".to_owned())
        .spawn(move || {
            let port = match crate::serial::open_port(&name, cfg) {
                Ok(port) => port,
                Err(err) => {
                    log(&tx, Level::Error, format!("打开 {name} 失败: {err:#}"));
                    let _ = tx.send(Event::SerialClosed(format!("打开失败: {err:#}")));
                    return;
                }
            };
            log(&tx, Level::Ok, format!("已打开 {name} · {}", cfg.label()));
            let mut reason = String::new();
            loop {
                match rx.try_recv() {
                    Ok(SerialCommand::Close) => break,
                    Ok(SerialCommand::Send(bytes)) => match port.write(&bytes) {
                        Ok(n) => {
                            let _ = tx.send(Event::SerialTxCount(n));
                        }
                        Err(err) => {
                            log(&tx, Level::Error, format!("串口写入失败: {err:#}"));
                            reason = format!("写入失败: {err:#}");
                            break;
                        }
                    },
                    Err(mpsc::TryRecvError::Disconnected) => {
                        reason = "会话通道已断开".to_owned();
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                match port.read() {
                    Ok(bytes) => {
                        if !bytes.is_empty() {
                            let _ = tx.send(Event::SerialRx(bytes));
                        }
                    }
                    Err(err) => {
                        log(&tx, Level::Error, format!("串口读取失败: {err:#}"));
                        reason = format!("读取失败: {err:#}");
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            log(
                &tx,
                Level::Info,
                format!("{name} 串口会话已结束"),
            );
            let _ = tx.send(Event::SerialClosed(reason));
        });
}

/// Address shared between the UI and the network-probe watcher. A plain
/// mutex is enough: it is written once per frame and read every few seconds.
pub type SharedNetworkAddress = Arc<std::sync::Mutex<String>>;

/// Resolve a configured network probe to concrete socket addresses. The
/// placeholder backend decides the default port (elaphureLink 3240, the
/// generic CMSIS-DAP TCP backend 4441).
pub fn resolve_network_probe_address(text: &str) -> Vec<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;

    let Some(selector) = parse_network_probe(text) else {
        return Vec::new();
    };
    let host = selector.serial_number.as_deref().unwrap_or("");
    let port = if selector.product_id == NETWORK_ELAPHURELINK_PID {
        3240u16
    } else {
        4441u16
    };
    match host.to_socket_addrs() {
        Ok(list) => list.collect(),
        Err(_) => (host, port)
            .to_socket_addrs()
            .map(|list| list.collect())
            .unwrap_or_default(),
    }
}

/// A bare TCP connect, deliberately WITHOUT the elaphureLink handshake.
/// The proxy serves exactly one client, so starting a DAP session just to
/// ask "are you there?" could steal the slot from a real flashing job.
pub fn network_probe_reachable(addresses: &[std::net::SocketAddr]) -> (bool, String) {
    use std::net::TcpStream;

    let mut last = "未解析到地址".to_owned();
    for address in addresses {
        match TcpStream::connect_timeout(address, Duration::from_millis(600)) {
            Ok(stream) => {
                drop(stream);
                return (true, format!("TCP {address} 可达"));
            }
            Err(err) => last = format!("TCP {address}: {err}"),
        }
    }
    (false, last)
}

/// Poll the configured network probe and report online/offline transitions.
///
/// The watcher shares `WatcherControl` with the USB hot-plug watcher, so it is
/// suspended while a job owns the debugger and when the user turns off
/// automatic device detection. Polling backs off from 3 s (online) to 8 s
/// (offline), and a failed lookup forces mDNS re-resolution on the next tick
/// so a device that changed IP after a reboot is found again.
pub fn spawn_network_probe_watcher(
    tx: Sender<Event>,
    control: Arc<WatcherControl>,
    address: SharedNetworkAddress,
    notify: impl Fn() + Send + Sync + 'static,
) {
    const ONLINE_POLL: Duration = Duration::from_secs(3);
    const OFFLINE_POLL: Duration = Duration::from_secs(8);
    let _ = std::thread::Builder::new()
        .name("network-probe-watch".to_owned())
        .spawn(move || {
            log(
                &tx,
                Level::Debug,
                "网络探针在线检测已启动 · 在线 3s / 离线 8s 轮询，任务执行期间暂停",
            );
            let mut last: Option<bool> = None;
            let mut cached_text = String::new();
            let mut cached_addrs: Vec<std::net::SocketAddr> = Vec::new();
            loop {
                if !control.enabled.load(Ordering::Relaxed)
                    || control.busy.load(Ordering::Relaxed)
                {
                    std::thread::sleep(ONLINE_POLL);
                    continue;
                }
                let text = address
                    .lock()
                    .map(|guard| guard.trim().to_owned())
                    .unwrap_or_default();
                if text.is_empty() {
                    if last.is_some() {
                        let _ = tx.send(Event::NetworkProbeStatus {
                            address: String::new(),
                            online: false,
                            detail: "未配置网络探针地址".to_owned(),
                        });
                        last = None;
                        notify();
                    }
                    cached_text.clear();
                    cached_addrs.clear();
                    std::thread::sleep(ONLINE_POLL);
                    continue;
                }
                if text != cached_text || cached_addrs.is_empty() {
                    cached_text = text.clone();
                    cached_addrs = resolve_network_probe_address(&text);
                }
                let (online, detail) = network_probe_reachable(&cached_addrs);
                if !online {
                    cached_addrs.clear();
                }
                if last != Some(online) {
                    log(
                        &tx,
                        if online { Level::Ok } else { Level::Warn },
                        format!(
                            "网络探针 {text} {} · {detail}",
                            if online { "已上线" } else { "已离线" }
                        ),
                    );
                    let _ = tx.send(Event::NetworkProbeStatus {
                        address: text.clone(),
                        online,
                        detail,
                    });
                    notify();
                    last = Some(online);
                }
                std::thread::sleep(if online { ONLINE_POLL } else { OFFLINE_POLL });
            }
        });
}

/// Headless live monitor: probe-rs-studio.exe --network-watch [seconds].
pub fn network_watch_cli(address: &str, duration: Duration) {
    let (tx, rx) = mpsc::channel();
    let control = Arc::new(WatcherControl::new(true));
    let shared: SharedNetworkAddress =
        Arc::new(std::sync::Mutex::new(address.to_owned()));
    spawn_network_probe_watcher(tx, control, shared, || {});
    println!(
        "网络探针在线检测: {address}（{} 秒，可插拔/断电设备观察状态变化）…",
        duration.as_secs()
    );
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::NetworkProbeStatus {
                address,
                online,
                detail,
            }) => println!(
                "[{}] {} · {}",
                if online { "在线" } else { "离线" },
                address,
                detail
            ),
            Ok(Event::Log(_, msg)) => println!("  {msg}"),
            Ok(_) | Err(_) => {}
        }
    }
}

/// Watch USB debug probes and push a fresh list whenever a probe is plugged in
/// or unplugged. The first sample is taken immediately; later samples only
/// produce an event after the probe set actually changes and the change is
/// confirmed by a second (debounced) sample.
pub fn spawn_probe_watcher(
    tx: Sender<Event>,
    control: Arc<WatcherControl>,
    interval: Duration,
    notify: impl Fn() + Send + Sync + 'static,
) {
    let debounce = Duration::from_millis(350);
    let _ = std::thread::Builder::new()
        .name("probe-hotplug-watch".to_owned())
        .spawn(move || {
            log(
                &tx,
                Level::Debug,
                format!(
                    "设备热插拔监视已启动 · 每 {:.1}s 检测一次",
                    interval.as_secs_f32()
                ),
            );
            let mut baseline: Option<Vec<ProbeItem>> = None;
            loop {
                if control.enabled.load(Ordering::Relaxed) && !control.busy.load(Ordering::Relaxed) {
                    let current = snapshot_probes();
                    let changed = baseline
                        .as_ref()
                        .map_or(true, |b| !same_probes(b, &current));
                    if changed {
                        // USB enumeration is transient right after a plug or
                        // unplug; confirm before notifying the UI.
                        std::thread::sleep(debounce);
                        let confirmed = snapshot_probes();
                        let still_changed = baseline
                            .as_ref()
                            .map_or(true, |b| !same_probes(b, &confirmed));
                        if still_changed {
                            let first = baseline.is_none();
                            let previous = baseline.take().unwrap_or_default();
                            let (added, removed) = probe_diff(&previous, &confirmed);
                            if first {
                                log(
                                    &tx,
                                    Level::Info,
                                    format!("发现 {} 个调试探针", confirmed.len()),
                                );
                                let _ = tx.send(Event::Probes(confirmed.clone()));
                                notify();
                            } else {
                                for a in &added {
                                    log(&tx, Level::Ok, format!("检测到调试探针插入: {a}"));
                                }
                                for r in &removed {
                                    log(&tx, Level::Warn, format!("检测到调试探针拔出: {r}"));
                                }
                                let _ = tx.send(Event::ProbeHotplug {
                                    items: confirmed.clone(),
                                    added,
                                    removed,
                                });
                                notify();
                            }
                            baseline = Some(confirmed);
                        }
                    }
                }
                std::thread::sleep(interval);
            }
        });
}

/// Headless diagnostic for the hot-plug watcher. Prints the initial probe list
/// and every plug/unplug transition for `duration` seconds.
pub fn watch_cli(duration: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    let control = Arc::new(WatcherControl::new(true));
    spawn_probe_watcher(tx, control, Duration::from_millis(1500), || {});
    println!(
        "设备热插拔监视 {} 秒（可在此期间插拔调试探针）…",
        duration.as_secs()
    );
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Log(level, msg)) => println!("[{level:?}] {msg}"),
            Ok(Event::Probes(items)) => println!(
                "当前探针 {} 个: {}",
                items.len(),
                items.iter()
                    .map(|i| i.label.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            Ok(Event::ProbeHotplug {
                items,
                added,
                removed,
            }) => println!(
                "热插拔: 插入 {} 个, 拔出 {} 个, 现有 {} 个 | + {:?} | - {:?}",
                added.len(),
                removed.len(),
                items.len(),
                added,
                removed
            ),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    println!("监视结束");
}

fn refresh_probes(tx: &Sender<Event>) -> Result<String> {
    log(tx, Level::Info, "枚举调试探针…");
    let items = snapshot_probes();
    log(tx, Level::Info, format!("发现 {} 个探针", items.len()));
    let count = items.len();
    let _ = tx.send(Event::Probes(items));
    Ok(format!("发现 {count} 个探针"))
}

/// Chip families generated from CMSIS-Packs with probe-rs' `target-gen` and
/// compiled into the binary, so a single executable works without extra files.
/// `Registry::add_target_family` replaces a family with the same name, which
/// makes these entries override any built-in family of the same name.
const BUNDLED_FAMILIES: &[(&str, &str)] = &[(
    "HC32F4XX.yaml",
    include_str!("../targets/HC32F4XX.yaml"),
)];

/// Extra user-supplied chip descriptions: any `*.yaml` / `*.yml` inside a
/// `targets/` folder next to the executable (or in the current directory).
/// Dropping in a new pack description is enough - no rebuild required.
fn user_chip_yamls() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(mut exe) = std::env::current_exe() {
        exe.pop();
        dirs.push(exe.join("targets"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("targets");
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }

    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !(ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")) {
                continue;
            }
            // The bundled file is already compiled in; avoid a duplicate load.
            let is_bundled = BUNDLED_FAMILIES.iter().any(|(name, _)| {
                path.file_name().and_then(|n| n.to_str()) == Some(*name)
            });
            if !is_bundled {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// The single source of truth for every chip lookup. Layer order:
/// built-in families -> bundled CMSIS-Packs -> `targets/*.yaml` ->
/// the explicitly selected `chip_description` (highest priority).
fn build_registry(cfg: Option<&FlashConfig>, tx: &Sender<Event>) -> Result<Registry> {
    let mut registry = Registry::from_builtin_families();

    for (name, yaml) in BUNDLED_FAMILIES {
        registry
            .add_target_family_from_yaml(yaml)
            .with_context(|| format!("内置芯片包 {name} 解析失败"))?;
    }

    for path in user_chip_yamls() {
        let yaml = fs::read_to_string(&path)
            .with_context(|| format!("无法读取芯片描述文件: {}", path.display()))?;
        registry
            .add_target_family_from_yaml(&yaml)
            .with_context(|| format!("芯片描述文件解析失败: {}", path.display()))?;
        log(
            tx,
            Level::Info,
            format!("已加载扩展芯片包: {}", path.display()),
        );
    }

    if let Some(path) = cfg.and_then(|c| c.chip_description.as_ref()) {
        let yaml = fs::read_to_string(path)
            .with_context(|| format!("无法读取芯片描述文件: {}", path.display()))?;
        registry
            .add_target_family_from_yaml(&yaml)
            .context("芯片描述文件解析失败")?;
        log(
            tx,
            Level::Info,
            format!("已加载外部芯片描述: {}", path.display()),
        );
    }
    Ok(registry)
}

fn load_chips(tx: &Sender<Event>) -> Result<String> {
    log(tx, Level::Info, "加载芯片描述库…");
    let builtin: Vec<String> = Registry::from_builtin_families()
        .families()
        .iter()
        .map(|f| f.name.to_ascii_lowercase())
        .collect();
    let registry = build_registry(None, tx)?;

    let mut chips = Vec::new();
    for family in registry.families() {
        let external = !builtin.contains(&family.name.to_ascii_lowercase());
        for variant in &family.variants {
            chips.push(ChipItem {
                family: family.name.clone(),
                name: variant.name.clone(),
                external,
            });
        }
    }
    let extra = chips.iter().filter(|c| c.external).count();
    let count = chips.len();
    log(
        tx,
        Level::Info,
        format!("共 {count} 个型号（含 {extra} 个扩展包型号）"),
    );
    let _ = tx.send(Event::Chips(chips));
    Ok(format!("已加载 {count} 个芯片型号"))
}

/// Convert a CMSIS-Pack into a probe-rs chip family, validate it, persist it
/// under `targets/` and refresh the chip library shown in the UI.
fn load_pack(path: &std::path::Path, tx: &Sender<Event>) -> Result<String> {
    log(
        tx,
        Level::Info,
        format!("解析 CMSIS-Pack: {}", path.display()),
    );
    let converted = crate::pack::convert(path)?;

    // Never leave a broken description behind: probe-rs must accept the YAML
    // before it is written to the auto-loaded directory.
    let mut check = Registry::from_builtin_families();
    check
        .add_target_family_from_yaml(&converted.yaml)
        .context("生成的芯片描述无法被 probe-rs 解析")?;

    let dest = crate::pack::install(path, &converted.yaml)?;
    log(
        tx,
        Level::Ok,
        format!(
            "已生成 {} 个型号 / {} 个 Flash 算法",
            converted.variants.len(),
            converted.algorithms.len()
        ),
    );
    for name in &converted.variants {
        log(tx, Level::Info, format!("新增芯片: {name}"));
    }
    log(
        tx,
        Level::Info,
        format!("芯片描述: {}", dest.display()),
    );
    load_chips(tx)?;
    Ok(format!(
        "已加载 {} 个型号 → {}",
        converted.variants.len(),
        dest.display()
    ))
}

/// Convert one standalone `.FLM` into a chip family, validate it, persist it
/// under `targets/` and refresh the chip library shown in the UI.
fn load_flm(path: &std::path::Path, spec: &FlmSpec, tx: &Sender<Event>) -> Result<String> {
    log(
        tx,
        Level::Info,
        format!("解析 Flash 算法: {}", path.display()),
    );
    let converted = crate::pack::convert_flm(path, spec)?;
    let info = &converted.info;
    log(tx, Level::Info, format!("FLM 自带参数: {}", info.summary()));
    log(tx, Level::Debug, format!("FLM 细节: {}", info.details()));
    log(
        tx,
        Level::Info,
        format!(
            "芯片描述: 系列 {} · 型号 {} · 内核 {} · Flash {:#x}+{} KiB · RAM {:#x}+{} KiB",
            converted.family,
            converted.variants.join(", "),
            crate::pack::resolve_core(&spec.core).unwrap_or("?"),
            spec.flash_base.unwrap_or(info.flash_base),
            spec.flash_size.unwrap_or(info.flash_size) / 1024,
            spec.ram_base,
            spec.ram_size / 1024
        ),
    );
    if spec.load_address.is_none()
        && info.code_load_address != 0
        && !(spec.ram_base..spec.ram_base.saturating_add(spec.ram_size))
            .contains(&info.code_load_address)
    {
        log(
            tx,
            Level::Warn,
            format!(
                "算法链接地址 {:#x} 不在声明的 RAM 内；若该 FLM 不是位置无关代码，请指定固定加载地址",
                info.code_load_address
            ),
        );
    }

    // Never leave a broken description behind.
    let mut check = Registry::from_builtin_families();
    check
        .add_target_family_from_yaml(&converted.yaml)
        .context("生成的芯片描述无法被 probe-rs 解析")?;
    for chip in &converted.variants {
        check
            .get_target_by_name(chip)
            .with_context(|| format!("生成的芯片描述里找不到型号 {chip}"))?;
    }

    let dest = crate::pack::install_as(
        &format!("flm_{}", converted.family),
        &converted.yaml,
    )?;
    for chip in &converted.variants {
        log(tx, Level::Info, format!("新增芯片: {chip}"));
    }
    log(
        tx,
        Level::Info,
        format!("芯片描述: {}", dest.display()),
    );
    load_chips(tx)?;
    Ok(format!(
        "已加载 {} 个型号 / 算法 {} → {}",
        converted.variants.len(),
        converted.algorithm,
        dest.display()
    ))
}

/// Mirror background log events to stdout for the headless CLI commands.
fn print_log_events(rx: Receiver<Event>) {
    while let Ok(event) = rx.try_recv() {
        if let Event::Log(level, msg) = event {
            let tag = match level {
                Level::Info => "INFO",
                Level::Ok => "OK  ",
                Level::Warn => "WARN",
                Level::Error => "ERR ",
                Level::Debug => "DBG ",
            };
            println!("    [{tag}] {msg}");
        }
    }
}

/// Headless chip-library check (`--chips`): no hardware needed, prints the
/// same layered registry the GUI builds.
pub fn chip_library() -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let result = load_chips(&tx);
    drop(tx);
    print_log_events(rx);
    result
}

/// Headless counterpart of the GUI's "load CMSIS-Pack" button: same
/// conversion, validation, persistence and chip-library refresh, with log
/// events mirrored to stdout. Used by `--pack <file>` without `--out`.
pub fn import_pack(path: &std::path::Path) -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let result = load_pack(path, &tx);
    drop(tx);
    print_log_events(rx);
    result
}

/// Headless counterpart of the GUI's "load .FLM" button (`--flm`).
pub fn import_flm(path: &std::path::Path, spec: &FlmSpec) -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let result = load_flm(path, spec, &tx);
    drop(tx);
    print_log_events(rx);
    result
}

fn chip_info(chip: &str, cfg: Option<&FlashConfig>, tx: &Sender<Event>) -> Result<String> {
    let registry = build_registry(cfg, tx)?;
    let target = registry
        .get_target_by_name(chip)
        .with_context(|| format!("芯片 {chip} 不在芯片库中"))?;
    let mut lines = vec![format!("型号: {}", target.name)];
    for core in &target.cores {
        lines.push(format!("内核: {} ({:?})", core.name, core.core_type));
    }
    for region in &target.memory_map {
        let range = region.address_range();
        let kind = if region.as_nvm_region().is_some() {
            "Flash / NVM"
        } else if region.as_ram_region().is_some() {
            "RAM"
        } else {
            "其它"
        };
        lines.push(format!(
            "{kind:<12} 0x{:08X} .. 0x{:08X}  ({:.1} KiB)",
            range.start,
            range.end,
            (range.end - range.start) as f64 / 1024.0
        ));
    }
    lines.push(format!("Flash 算法: {} 个", target.flash_algorithms.len()));
    let text = lines.join("\n");
    let _ = tx.send(Event::ChipDetails(text));
    Ok(format!("已读取 {} 的描述", target.name))
}

fn connect(cfg: &FlashConfig, tx: &Sender<Event>) -> Result<Session> {
    connect_with(cfg, tx, false)
}

/// `attach_ob` appends the bundled option-byte flash algorithm and its 16-byte
/// NVM region to the selected target. probe-rs' `FlashLoader` is built from
/// `session.target()`, so this is what makes an OB address programmable: the
/// main Flash algorithm does not cover 0x1FFF0E80, but the vendor's dedicated
/// `*_OPT.FLM` does. The extra algorithm lives only for this one session.
/// Whether a failed `Lister::open()` is a transient probe-level hiccup worth
/// re-enumerating for. A genuinely missing probe or a bad USB descriptor is not.
fn is_probe_open_transient(err: &str) -> bool {
    is_transient_probe_error(&anyhow!("{err}"))
}

/// Turn the raw probe error into an actionable message. In particular, a
/// `WinError: WaitTimedOut` means the DAPLink firmware stopped answering, NOT
/// that another program holds the device; telling the user to close other
/// programs sends them down the wrong path.
fn probe_open_hint(err: &str) -> &'static str {
    let lower = err.to_ascii_lowercase();
    if lower.contains("waittimedout") || lower.contains("timed out") || lower.contains("timeout") {
        "探针固件无响应（HID 超时）· 请重新插拔 DAPLink USB 让它重新上电；\
         若刚用过 pyOCD / OpenOCD / 其它调试工具，请确认它们已退出。\
         这种状态只靠重试命令无法恢复，需要探针重新枚举"
    } else if lower.contains("access is denied")
        || lower.contains("getlasterror=5")
        || lower.contains("sharing violation")
    {
        "探针被其它程序独占 · 请关闭 pyOCD / OpenOCD / 其它 probe-rs 实例后重试"
    } else {
        "打开探针失败"
    }
}

/// Open the selected probe, re-enumerating between attempts.
///
/// A DAPLink that dropped its HID queue often becomes usable again after the
/// host re-enumerates it (or after the user replugs it). Retrying on the same
/// `Lister` snapshot cannot see that, so each attempt calls `list_all()` again
/// and resolves the probe by its stable identity rather than by index.
fn open_probe(cfg: &FlashConfig, tx: &Sender<Event>) -> Result<Probe> {
    const ATTEMPTS: usize = 3;

    // A network probe has no USB enumeration, so it is opened directly from
    // the configured address when its identity is the selected one. A failed
    // connection is a configuration/availability problem, not a transient USB
    // error, so it is reported immediately instead of being retried.
    if let Some(selector) = parse_network_probe(&cfg.network_probe) {
        let identity = network_probe_identity(&selector);
        if cfg.probe_id.as_deref() == Some(identity.as_str()) {
            log(
                tx,
                Level::Info,
                format!(
                    "打开网络探针: {} [0000:{:04x}]",
                    cfg.network_probe, selector.product_id
                ),
            );
            return Lister::new().open(selector).map_err(|err| {
                let text = format!("{:#}", anyhow::Error::from(err));
                anyhow!(
                    "无法连接网络探针 {}。请在「连接配置」点「诊断连接…」看失败发生在解析 / TCP / 握手哪一步；\
                     常见原因是 elaphureLink 代理只接受一个客户端（被 OpenOCD 等程序占用）、\
                     设备未上电或 DAP 服务未启动。若要使用本机 USB 调试器，请在上方下拉框切换。\n原始错误：{text}",
                    cfg.network_probe
                )
            });
        }
    }

    let lister = Lister::new();
    for attempt in 1..=ATTEMPTS {
        let probes = lister.list_all();
        // Prefer the stable device identity: enumeration indices shift whenever
        // a probe is unplugged, so an index alone could attach to a wrong device.
        // After a replug the identity may change; if exactly one probe is left,
        // it is unambiguous and safe to use it.
        let found = match cfg.probe_id.as_deref() {
            Some(id) if !id.is_empty() => probes.iter().find(|p| probe_identity(p) == id),
            _ => probes.get(cfg.probe_index),
        };
        let info = match found {
            Some(info) => info,
            None if probes.len() == 1 => {
                log(
                    tx,
                    Level::Warn,
                    "保存的探针身份已变化（可能刚重新插拔），当前仅发现一个探针，自动改用它",
                );
                &probes[0]
            }
            None => bail!("所选调试探针已断开，请在「设备」页重新选择（当前发现 {} 个探针）", probes.len()),
        };
        log(
            tx,
            Level::Info,
            format!(
                "打开探针: {} [{:04x}:{:04x}]{}",
                info.identifier,
                info.vendor_id,
                info.product_id,
                if attempt > 1 { format!(" · 第 {attempt} 次尝试") } else { String::new() }
            ),
        );
        match lister.open(info) {
            Ok(probe) => return Ok(probe),
            Err(err) => {
                // DebugProbeError's Display is only the top-level message;
                // wrapping in anyhow and using `{:#}` prints the whole cause
                // chain (including hidapi's WinError), which is what the hint
                // and retry classifier need to see.
                let wrapped = anyhow::Error::from(err);
                let text = format!("{wrapped:#}");
                if attempt == ATTEMPTS {
                    return Err(ProbeUnresponsive(text).into());
                }
                if !is_probe_open_transient(&text) {
                    bail!("{}\n原始错误：{text}", probe_open_hint(&text));
                }
                log(
                    tx,
                    Level::Warn,
                    format!(
                        "探针打开失败（第 {attempt}/{ATTEMPTS} 次），{} ms 后重新枚举重试：{text}",
                        400 * attempt
                    ),
                );
                std::thread::sleep(Duration::from_millis(400 * attempt as u64));
            }
        }
    }
    unreachable!("open_probe always returns or bails")
}

fn connect_with(cfg: &FlashConfig, tx: &Sender<Event>, attach_ob: bool) -> Result<Session> {
    let mut probe = open_probe(cfg, tx)?;

    let protocol = match cfg.protocol {
        Protocol::Swd => WireProtocol::Swd,
        Protocol::Jtag => WireProtocol::Jtag,
    };
    probe.select_protocol(protocol).context("选择调试协议失败")?;
    let actual = probe.set_speed(cfg.speed_khz).context("设置时钟失败")?;
    log(
        tx,
        Level::Debug,
        format!("协议 {} @ {} kHz (请求 {} kHz)", protocol, actual, cfg.speed_khz),
    );

    let registry = build_registry(Some(cfg), tx)?;
    let mut target = registry
        .get_target_by_name(&cfg.chip)
        .with_context(|| format!("芯片 {} 不在描述库中", cfg.chip))?;
    if attach_ob {
        // Keep the OPT family out of the general chip registry: resolve it in a
        // private registry, then lift just the algorithm + NVM region over.
        let mut ob_registry = Registry::from_builtin_families();
        ob_registry
            .add_target_family_from_yaml(option_bytes::PY32_OPT_YAML)
            .context("内置 PY32 选项字节算法解析失败")?;
        let ob_target = ob_registry
            .get_target_by_name(option_bytes::PY32_OB_VARIANT)
            .context("内置 PY32 选项字节算法缺少目标型号")?;
        let algorithm = ob_target
            .flash_algorithms
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("内置 PY32 选项字节算法为空"))?;
        let region = ob_target
            .memory_map
            .into_iter()
            .find(|r| r.as_nvm_region().is_some())
            .ok_or_else(|| anyhow!("内置 PY32 选项字节算法缺少 NVM 区域"))?;
        target.flash_algorithms.push(algorithm);
        target.memory_map.push(region);
        log(tx, Level::Info, "已挂载 PY32F0xx_OPT.FLM 选项字节算法");
    }
    let selector = TargetSelector::Specified(Box::new(target));
    let permissions = Permissions::default();
    let session = if cfg.connect_under_reset {
        probe.attach_under_reset_with_registry(selector, permissions, &registry)
    } else {
        probe.attach_with_registry(selector, permissions, &registry)
    }
    .context("连接目标芯片失败")?;
    log(tx, Level::Ok, format!("已连接 {}", cfg.chip));
    let _ = tx.send(Event::Connected(format!(
        "{} · {} · {} kHz",
        cfg.chip, protocol, actual
    )));
    Ok(session)
}

fn describe_session(session: &mut Session) -> Result<String> {
    let target = session.target().clone();
    let mut parts = vec![target.name.clone()];
    for core in &target.cores {
        parts.push(format!("{:?}", core.core_type));
    }
    let nvm_start = target
        .memory_map
        .iter()
        .find(|r| r.as_nvm_region().is_some())
        .map(|r| r.address_range().start)
        .unwrap_or(0x0800_0000);
    let mut core = session.core(0).context("无法访问核心 0")?;
    core.halt(Duration::from_millis(500)).ok();
    let mut head = [0u8; 16];
    core.read_8(nvm_start, &mut head).context("读取 Flash 失败")?;
    core.run().ok();
    let sp = u32::from_le_bytes(head[0..4].try_into().unwrap());
    let pc = u32::from_le_bytes(head[4..8].try_into().unwrap());
    parts.push(format!(
        "向量表 SP=0x{sp:08X} PC=0x{pc:08X}, Flash 头 {}",
        head.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
    ));
    Ok(parts.join(" · "))
}

fn read_flash(session: &mut Session, address: u64, size: usize, tx: &Sender<Event>) -> Result<Vec<u8>> {
    let mut core = session.core(0).context("无法访问核心 0")?;
    core.halt(Duration::from_millis(500)).ok();
    let mut out = Vec::with_capacity(size);
    let mut done = 0usize;
    while done < size {
        let n = (size - done).min(1024);
        let mut chunk = vec![0u8; n];
        core.read_8(address + done as u64, &mut chunk)
            .with_context(|| format!("读取 0x{:08X} 处 {} 字节失败", address + done as u64, n))?;
        out.extend_from_slice(&chunk);
        done += n;
        if size > 4096 {
            progress(tx, done as f32 / size as f32, format!("读回 {done}/{size} 字节"));
        }
    }
    core.run().ok();
    Ok(out)
}

fn read_back(cfg: &FlashConfig, tx: &Sender<Event>) -> Result<String> {
    let mut session = connect(cfg, tx)?;
    log(
        tx,
        Level::Info,
        format!(
            "读回 0x{:08X} 起 {} 字节 → {}",
            cfg.readback_address,
            cfg.readback_size,
            cfg.backup_path.display()
        ),
    );
    let data = read_flash(&mut session, cfg.readback_address, cfg.readback_size as usize, tx)?;
    fs::write(&cfg.backup_path, &data)
        .with_context(|| format!("写入 {} 失败", cfg.backup_path.display()))?;
    progress(tx, 1.0, "读回完成");
    Ok(format!(
        "备份 {} 字节 · SHA256 {}",
        data.len(),
        sha256_hex(&data)
    ))
}

fn expected_image(cfg: &FlashConfig) -> Result<(Vec<u8>, u64)> {
    let path = cfg.firmware.as_ref().ok_or_else(|| anyhow!("请先选择固件文件"))?;
    let info = firmware::inspect(path, cfg.format, cfg.bin_base)?;
    let image = info.image.ok_or_else(|| anyhow!("该格式不支持独立读回比对（ELF 请使用库内校验）"))?;
    Ok((image, info.image_base.unwrap_or(cfg.bin_base)))
}

fn verify_readback(session: &mut Session, base: u64, expected: &[u8], tx: &Sender<Event>) -> Result<String> {
    log(
        tx,
        Level::Info,
        format!("独立读回校验: 0x{base:08X} 起 {} 字节", expected.len()),
    );
    let actual = read_flash(session, base, expected.len(), tx)?;
    let expected_hash = sha256_hex(expected);
    let actual_hash = sha256_hex(&actual);
    if actual == expected {
        progress(tx, 1.0, "校验通过");
        Ok(format!("读回完全一致 · SHA256 {actual_hash}"))
    } else {
        let diffs: Vec<usize> = actual
            .iter()
            .zip(expected.iter())
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .take(8)
            .collect();
        bail!(
            "校验失败: 期望 {expected_hash}, 实际 {actual_hash}, 首个差异偏移 {diffs:?}"
        )
    }
}

fn reset_target(session: &mut Session, tx: &Sender<Event>) -> Result<()> {
    log(tx, Level::Debug, "复位目标并恢复运行");
    let mut core = session.core(0).context("无法访问核心 0")?;
    core.reset().context("复位失败")?;
    core.run().context("恢复运行失败")?;
    Ok(())
}

fn flash(cfg: &FlashConfig, tx: &Sender<Event>) -> Result<String> {
    let path = cfg.firmware.clone().ok_or_else(|| anyhow!("请先选择固件文件"))?;
    let info: FirmwareInfo = firmware::inspect(&path, cfg.format, cfg.bin_base)?;
    log(
        tx,
        Level::Info,
        format!(
            "固件 {} · {} · {} 字节 · SHA256 {}",
            path.display(),
            info.format.label(),
            info.size,
            info.sha256
        ),
    );
    log(tx, Level::Debug, info.note.clone());

    let mut session = connect(cfg, tx)?;

    // Identify the *physical* silicon before the first write. Reuse this
    // session (no extra attach), keep the check read-only and never let an
    // unsupported vendor register abort a perfectly good flash run.
    if cfg.read_chip_id_before_flash {
        let snapshot = collect_chip_id(&mut session, cfg, None);
        if snapshot.fields.is_empty() {
            log(
                tx,
                Level::Warn,
                "烧录前芯片 ID 读取失败（只读步骤，不中断烧录）",
            );
        } else {
            log(
                tx,
                Level::Info,
                format!("烧录前芯片身份: {}", snapshot.identity()),
            );
            for warning in &snapshot.warnings {
                log(tx, Level::Debug, warning.clone());
            }
            let _ = tx.send(Event::ChipId(snapshot));
        }
    }

    let progress_tx = tx.clone();
    let mut total: u64 = 0;
    let mut done: u64 = 0;
    let mut options = DownloadOptions::default();
    options.verify = cfg.verify;
    options.keep_unwritten_bytes = cfg.restore_unwritten;
    options.do_chip_erase = cfg.chip_erase;
    options.preverify = cfg.preverify;
    options.progress = FlashProgress::new(move |event| match event {
        ProgressEvent::AddProgressBar { operation, total: t } => {
            if let Some(t) = t {
                total += t;
                let _ = progress_tx.send(Event::Log(
                    Level::Debug,
                    format!("{operation:?}: 共 {t} 字节"),
                ));
            }
        }
        ProgressEvent::Started(operation) => {
            let _ = progress_tx.send(Event::Log(
                Level::Info,
                format!("开始 {operation:?}"),
            ));
        }
        ProgressEvent::Progress { operation, size, .. } => {
            done += size;
            let _ = progress_tx.send(Event::Progress {
                value: if total > 0 { done as f32 / total as f32 } else { 0.0 },
                label: format!("{operation:?} · {done} 字节"),
            });
        }
        ProgressEvent::Finished(operation) => {
            let _ = progress_tx.send(Event::Log(
                Level::Ok,
                format!("{operation:?} 完成"),
            ));
        }
        ProgressEvent::Failed(operation) => {
            let _ = progress_tx.send(Event::Log(
                Level::Error,
                format!("{operation:?} 失败"),
            ));
        }
        ProgressEvent::DiagnosticMessage { message } => {
            let _ = progress_tx.send(Event::Log(Level::Debug, message));
        }
        ProgressEvent::FlashLayoutReady { .. } => {
            let _ = progress_tx.send(Event::Log(
                Level::Debug,
                "Flash 布局已生成".to_owned(),
            ));
        }
    });

    let started = Instant::now();
    let result = match cfg.format {
        FirmwareFormat::Hex => {
            flashing::download_file_with_options(&mut session, &path, HexLoader, options)
        }
        FirmwareFormat::Elf => flashing::download_file_with_options(
            &mut session,
            &path,
            ElfLoader(ElfOptions::default()),
            options,
        ),
        FirmwareFormat::Bin => flashing::download_file_with_options(
            &mut session,
            &path,
            BinLoader(BinOptions {
                base_address: Some(cfg.bin_base),
                skip: 0,
            }),
            options,
        ),
    };
    result.context("烧录失败")?;
    progress(tx, 1.0, "烧录完成");
    log(
        tx,
        Level::Ok,
        format!("写入完成，耗时 {:.2}s", started.elapsed().as_secs_f32()),
    );

    if cfg.verify_readback {
        if let Some(expected) = &info.image {
            let base = info.image_base.unwrap_or(cfg.bin_base);
            verify_readback(&mut session, base, expected, tx)?;
        } else {
            log(tx, Level::Warn, "ELF 跳过独立读回比对，已由库内 verify 校验");
        }
    }

    if cfg.reset_after {
        reset_target(&mut session, tx)?;
    }
    Ok(format!(
        "烧录成功 · {} 字节 · {:.2}s",
        info.size,
        started.elapsed().as_secs_f32()
    ))
}

// ---------------------------------------------------------------------------
// Option bytes
// ---------------------------------------------------------------------------

/// Read the SWD `DPIDR` through probe-rs' debug-port interface. Returns the
/// raw register plus the manufacturer name resolved from its JEP106 code.
fn read_dpidr(session: &mut Session) -> Option<(u32, Option<String>)> {
    // `DapAccess` is a supertrait of `ArmDebugInterface`, so its methods are
    // available on the trait object without importing the trait here.
    use probe_rs::architecture::arm::dp::{DPIDR, DebugPortId, DpAddress, DpRegisterAddress};
    let interface = session.get_arm_interface().ok()?;
    let raw = interface
        .read_raw_dp_register(
            DpAddress::Default,
            DpRegisterAddress {
                address: chip_id::DPIDR_ADDRESS,
                bank: Some(chip_id::DPIDR_BANK),
            },
        )
        .ok()?;
    let designer = DebugPortId::from(DPIDR(raw))
        .designer
        .get()
        .map(|name| name.to_string());
    Some((raw, designer))
}

/// Generic ARM identity from the CoreSight ROM table: manufacturer (JEP106)
/// + part number + debug base address. Works on chips with no bundled vendor
/// register map (MM32, HC32, GD32, ...), where only a ROM table is discoverable.
/// Returns `None` on M0/M0+ parts that have no ROM table; that is not an error.
fn read_rom_table_identity(session: &mut Session) -> Option<(String, u16, u64)> {
    use probe_rs::architecture::arm::{dp::DpAddress, memory::romtable::Component};

    let interface = session.get_arm_interface().ok()?;
    let dp = DpAddress::Default;
    for ap in interface.access_ports(dp).ok()? {
        let Ok(mut memory) = interface.memory_interface(&ap) else {
            continue;
        };
        let Ok(base) = memory.base_address() else {
            continue;
        };
        if base == 0 {
            continue;
        }
        let Ok(Component::Class1RomTable(cid, _)) = Component::try_parse(&mut *memory, base)
        else {
            continue;
        };
        let Some(jep) = cid.peripheral_id().jep106() else {
            continue;
        };
        let designer = jep
            .get()
            .map(|name| name.to_string())
            .unwrap_or_else(|| format!("JEP106 cc={} id=0x{:02X}", jep.cc, jep.id));
        return Some((designer, cid.peripheral_id().part(), base));
    }
    None
}

/// Assemble a full read-only identity report from an already attached session.
///
/// Every vendor register is optional: a die that simply does not implement it
/// produces a warning, never an error. That makes the same routine safe to call
/// automatically before a flash run on any chip in the library.
fn collect_chip_id(
    session: &mut Session,
    cfg: &FlashConfig,
    custom: Option<u64>,
) -> ChipIdSnapshot {
    let profile: ChipIdProfile = chip_id::profile_for_chip(&cfg.chip);
    let mut snap = ChipIdSnapshot::new(&cfg.chip, profile);

    match read_dpidr(session) {
        Some((raw, designer)) => snap.push(ChipIdField::new(
            "DPIDR",
            None,
            format!("0x{raw:08X}"),
            chip_id::dpidr_detail(raw, designer.as_deref()),
        )),
        None => snap
            .warnings
            .push("DPIDR 读取失败（目标可能不是 ARM 或探针不支持）".to_owned()),
    }

    // A ROM-table walk costs dozens of SWD transactions. Only pay for it when
    // there is no vendor map: PY32F003 (M0+) has no ROM table anyway, and the
    // pre-flash check must stay cheap on an easily upset DAPLink.
    if profile.kind == chip_id::IdKind::Generic {
        if let Some((designer, part, base)) = read_rom_table_identity(session) {
            snap.push(ChipIdField::new(
                "ROM Table",
                Some(base),
                format!("0x{part:04X}"),
                format!(
                    "CoreSight ROM Table · 制造商 {designer} · 部件号 0x{part:04X} · 调试基址 0x{base:08X}"
                ),
            ));
        }
    }

    // The selected chip description declares a core type (Armv6m / Armv7m /
    // ...). Compare it with the silicon's CPUID so a wrong model in the chip
    // library is called out instead of silently producing a confusing flash
    // failure. Read this before `session.core(0)` takes a mutable borrow.
    let expected_core = session
        .target()
        .cores
        .first()
        .map(|c| format!("{:?}", c.core_type));

    let mut core = match session.core(0) {
        Ok(core) => core,
        Err(err) => {
            snap.warnings.push(format!("无法访问核心 0: {err}"));
            return snap;
        }
    };
    core.halt(Duration::from_millis(500)).ok();

    match core.read_word_32(chip_id::SCB_CPUID) {
        Ok(raw) => {
            snap.push(ChipIdField::new(
                "CPUID",
                Some(chip_id::SCB_CPUID),
                format!("0x{raw:08X}"),
                chip_id::cpuid_detail(raw),
            ));
            if let Some(note) = chip_id::core_mismatch_note(expected_core.as_deref(), raw) {
                snap.warnings.push(note);
            }
        }
        Err(err) => snap.warnings.push(format!(
            "CPUID 0x{:08X} 读取失败: {err}",
            chip_id::SCB_CPUID
        )),
    }

    if let Some(addr) = profile.idcode {
        match core.read_word_32(addr) {
            Ok(raw) => snap.push(ChipIdField::new(
                "DBGMCU_IDCODE",
                Some(addr),
                format!("0x{raw:08X}"),
                chip_id::idcode_detail(raw),
            )),
            Err(err) => snap
                .warnings
                .push(format!("DBGMCU_IDCODE 0x{addr:08X} 读取失败: {err}")),
        }
    }

    if let Some(addr) = profile.uid {
        let mut bytes = [0u8; chip_id::UID_LEN];
        match core.read_8(addr, &mut bytes) {
            Ok(()) => snap.push(ChipIdField::new(
                "UID",
                Some(addr),
                chip_id::uid_hex(&bytes),
                chip_id::uid_detail(&bytes),
            )),
            Err(err) => snap
                .warnings
                .push(format!("UID 0x{addr:08X} 读取失败: {err}")),
        }
    }

    // The flash-size register is a 16-bit value and is not always word aligned
    // (e.g. STM32F4 @ 0x1FFF7A22), so read bytes rather than a 32-bit word.
    if let Some(addr) = profile.flash_size {
        let mut bytes = [0u8; 2];
        match core.read_8(addr, &mut bytes) {
            Ok(()) => {
                let raw = u16::from_le_bytes(bytes) as u32;
                snap.push(ChipIdField::new(
                    "Flash 容量",
                    Some(addr),
                    format!("0x{raw:04X}"),
                    chip_id::flash_size_detail(raw),
                ));
            }
            Err(err) => snap
                .warnings
                .push(format!("Flash 容量 0x{addr:08X} 读取失败: {err}")),
        }
    }

    if let Some(addr) = custom {
        let mut bytes = [0u8; 4];
        match core.read_8(addr, &mut bytes) {
            Ok(()) => {
                let raw = u32::from_le_bytes(bytes);
                snap.push(ChipIdField::new(
                    "自定义寄存器",
                    Some(addr),
                    format!("0x{raw:08X}"),
                    "小端 32 位原值，未做型号相关解析".to_owned(),
                ));
            }
            Err(err) => snap
                .warnings
                .push(format!("自定义地址 0x{addr:08X} 读取失败: {err}")),
        }
    }

    core.run().ok();
    snap
}

fn read_chip_id_snapshot(
    cfg: &FlashConfig,
    custom: Option<u64>,
    tx: &Sender<Event>,
) -> Result<ChipIdSnapshot> {
    let mut session = connect(cfg, tx)?;
    let snapshot = collect_chip_id(&mut session, cfg, custom);
    if snapshot.fields.is_empty() {
        bail!(
            "未能读取任何芯片 ID 寄存器：{}",
            snapshot.warnings.join("；")
        );
    }
    Ok(snapshot)
}

fn read_chip_id(cfg: &FlashConfig, custom: Option<u64>, tx: &Sender<Event>) -> Result<String> {
    let snapshot = read_chip_id_snapshot(cfg, custom, tx)?;
    for warning in &snapshot.warnings {
        log(tx, Level::Warn, warning.clone());
    }
    log(
        tx,
        Level::Ok,
        format!("芯片身份: {}", snapshot.identity()),
    );
    let count = snapshot.fields.len();
    let _ = tx.send(Event::ChipId(snapshot));
    Ok(format!("已读取 {count} 项芯片 ID 信息"))
}

/// Headless equivalent of the GUI's「连接测试」button. Uses `run()` so it also
/// exercises the job-level probe retry policy, not just the raw library call.
pub fn connect_cli(cfg: &FlashConfig) {
    let (tx, rx) = mpsc::channel();
    println!("连接测试: {} · {} kHz", cfg.chip, cfg.speed_khz);
    let result = run(Job::Connect(Box::new(cfg.clone())), &tx);
    while let Ok(ev) = rx.try_recv() {
        if let Event::Log(level, msg) = ev {
            println!("    [{level:?}] {msg}");
        }
    }
    match result {
        Ok(summary) => println!("[PASS] {summary}"),
        Err(err) => println!("[FAIL] {err:#}"),
    }
}

/// Headless, read-only identification: `--chip-id [chip] [--id-addr 0x…]`.
pub fn chip_id_cli(cfg: &FlashConfig, custom: Option<u64>) {
    let (tx, rx) = mpsc::channel();
    println!("读取芯片 ID（只读，不写入任何数据）…");
    match read_chip_id_snapshot(cfg, custom, &tx) {
        Ok(snapshot) => {
            while let Ok(ev) = rx.try_recv() {
                if let Event::Log(level, msg) = ev {
                    println!("    [{level:?}] {msg}");
                }
            }
            println!();
            // `ChipIdSnapshot::text()` already renders every `[WARN]` row;
            // printing `snapshot.warnings` again would duplicate them.
            print!("{}", snapshot.text());
            println!("[PASS] 芯片 ID 读取完成");
        }
        Err(err) => {
            while let Ok(ev) = rx.try_recv() {
                if let Event::Log(level, msg) = ev {
                    println!("    [{level:?}] {msg}");
                }
            }
            println!("[FAIL] {err:#}");
        }
    }
}

/// Read the option-byte block through plain memory access. Safe on every chip:
/// no FLASH registers are touched and nothing is written.
fn read_ob_snapshot(cfg: &FlashConfig, base: u64, tx: &Sender<Event>) -> Result<ObSnapshot> {
    let mut session = connect(cfg, tx)?;
    let mut bytes = vec![0u8; OB_IMAGE_LEN];
    {
        let mut core = session.core(0).context("无法访问核心 0")?;
        core.halt(Duration::from_millis(500)).ok();
        core.read_8(base, &mut bytes)
            .with_context(|| format!("读取选项字节 0x{base:08X} 失败"))?;
        core.run().ok();
    }
    Ok(ObSnapshot {
        chip: cfg.chip.clone(),
        base,
        bytes,
    })
}

fn read_option_bytes(cfg: &FlashConfig, base: u64, tx: &Sender<Event>) -> Result<String> {
    let snapshot = read_ob_snapshot(cfg, base, tx)?;
    let image = ObImage::from_slice(&snapshot.bytes)
        .ok_or_else(|| anyhow!("选项字节长度不是 {OB_IMAGE_LEN} 字节"))?;
    log(
        tx,
        Level::Ok,
        format!("读取 0x{base:08X}: {}", image.hex()),
    );
    if !image.complements_valid() {
        log(
            tx,
            Level::Warn,
            "选项字节反码/保留字节校验未通过，请以原始字节为准",
        );
    }
    let _ = tx.send(Event::OptionBytes(snapshot));
    Ok(format!("已读取 {OB_IMAGE_LEN} 字节选项字节"))
}

/// Program the 16-byte OB block with the vendor's dedicated OPT.FLM algorithm.
/// The block is staged as a temporary BIN so probe-rs' own loader performs the
/// erase/program/verify sequence; no FLASH controller registers are poked by
/// hand. The current block is read first and every guard in
/// `option_bytes::validate` is enforced before anything is written.
fn write_option_bytes(
    cfg: &FlashConfig,
    base: u64,
    image: ObImage,
    acknowledged: bool,
    tx: &Sender<Event>,
) -> Result<String> {
    let profile = option_bytes::profile_for_chip(&cfg.chip);
    if !profile.writable {
        bail!("{}\n该芯片未内置地址匹配的 OPT.FLM，暂不提供写入（只能读取与导出）", profile.note);
    }
    if base != profile.base {
        bail!(
            "OB 基址 0x{base:08X} 与内置算法地址 0x{:08X} 不一致，拒绝写入",
            profile.base
        );
    }

    // One session for read -> validate -> program -> read back. Attaching the
    // OB algorithm does not affect ordinary memory reads.
    let mut session = connect_with(cfg, tx, true)?;
    let mut current = vec![0u8; OB_IMAGE_LEN];
    {
        let mut core = session.core(0).context("无法访问核心 0")?;
        core.halt(Duration::from_millis(500)).ok();
        core.read_8(base, &mut current)
            .with_context(|| format!("读取当前选项字节 0x{base:08X} 失败"))?;
        core.run().ok();
    }
    let old = ObImage::from_slice(&current)
        .ok_or_else(|| anyhow!("当前选项字节长度不是 {OB_IMAGE_LEN} 字节"))?;
    option_bytes::validate(&old, &image, acknowledged).map_err(anyhow::Error::msg)?;

    for change in option_bytes::diff(profile.kind, &old, &image) {
        let flag = if change.danger { " [高风险]" } else { "" };
        log(
            tx,
            if change.danger { Level::Warn } else { Level::Info },
            format!("{}: {} → {}{}", change.label, change.from, change.to, flag),
        );
    }
    log(tx, Level::Warn, "正在写入选项字节，请勿断开调试器或断电…");
    log(tx, Level::Info, format!("当前: {}", old.hex()));
    log(tx, Level::Info, format!("目标: {}", image.hex()));

    let staging = std::env::temp_dir().join(format!(
        "probe-rs-studio-ob-{}.bin",
        std::process::id()
    ));
    fs::write(&staging, &image.bytes).context("暂存选项字节镜像失败")?;
    let mut options = DownloadOptions::default();
    options.verify = true;
    options.keep_unwritten_bytes = false;
    options.do_chip_erase = false;
    options.preverify = false;
    let result = flashing::download_file_with_options(
        &mut session,
        &staging,
        BinLoader(BinOptions {
            base_address: Some(base),
            skip: 0,
        }),
        options,
    );
    let _ = fs::remove_file(&staging);
    result.context("写入选项字节失败（OPT.FLM 算法执行出错）")?;

    // Independent read-back, not just the loader's internal verify.
    let mut readback = vec![0u8; OB_IMAGE_LEN];
    {
        let mut core = session.core(0).context("回读时无法访问核心 0")?;
        core.read_8(base, &mut readback)
            .context("回读选项字节失败")?;
    }
    if readback != image.bytes.as_slice() {
        bail!(
            "选项字节回读不一致\n期望 {}\n实际 {}",
            image.hex(),
            readback
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    log(tx, Level::Ok, "选项字节写入并回读校验通过");
    reset_target(&mut session, tx)?;
    log(
        tx,
        Level::Warn,
        "选项字节通常在重新上电后完全生效；如未生效请给目标板断电重启",
    );
    Ok("选项字节写入成功 · 16 字节 · 回读一致".to_owned())
}

/// Headless, read-only inspection: `--option-bytes [base]`. Never writes.
pub fn option_bytes_cli(cfg: &FlashConfig, base: u64) {
    let (tx, rx) = mpsc::channel();
    println!("读取选项字节 0x{base:08X} …");
    match read_ob_snapshot(cfg, base, &tx) {
        Ok(snapshot) => {
            while let Ok(ev) = rx.try_recv() {
                if let Event::Log(level, msg) = ev {
                    println!("    [{level:?}] {msg}");
                }
            }
            println!();
            println!("{}", option_bytes::describe(&snapshot));
            println!("\n[PASS] 选项字节读取完成（只读，未写入任何数据）");
        }
        Err(err) => {
            while let Ok(ev) = rx.try_recv() {
                if let Event::Log(level, msg) = ev {
                    println!("    [{level:?}] {msg}");
                }
            }
            println!("[FAIL] {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> ProbeItem {
        ProbeItem {
            id: id.to_owned(),
            label: id.to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn probe_set_comparison_ignores_enumeration_order_but_detects_changes() {
        let a = vec![item("a"), item("b")];
        // USB enumeration order may shuffle without any physical change.
        assert!(same_probes(&a, &[item("b"), item("a")]));
        assert!(same_probes(&[], &[]));
        // Unplug, plug and swap (unplug A + plug B) must all be detected.
        assert!(!same_probes(&a, &[item("a")]));
        assert!(!same_probes(&a, &[item("a"), item("c")]));
        assert!(!same_probes(&a, &[item("a"), item("b"), item("c")]));
    }

    #[test]
    fn probe_diff_reports_plug_unplug_and_device_swaps() {
        let a = item("a");
        let b = item("b");
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<String>>();

        let (added, removed) = probe_diff(&[], &[a.clone(), b.clone()]);
        assert_eq!(added, s(&["a", "b"]));
        assert!(removed.is_empty(), "plug-in must not report a removal");

        let (added, removed) = probe_diff(&[a.clone(), b.clone()], &[b.clone()]);
        assert!(added.is_empty(), "unplug must not report an addition");
        assert_eq!(removed, s(&["a"]));

        // Unplug A and plug B in the same polling window (count stays 1).
        let (added, removed) = probe_diff(&[a.clone()], &[b.clone()]);
        assert_eq!(added, s(&["b"]));
        assert_eq!(removed, s(&["a"]));

        // Pure enumeration reorder produces no events at all.
        let (added, removed) = probe_diff(&[a.clone(), b.clone()], &[b, a]);
        assert!(added.is_empty() && removed.is_empty());
    }

    #[test]
    fn bundled_hc32f4xx_pack_is_registered_and_flashable() {
        let mut registry = Registry::from_builtin_families();
        let family = registry
            .add_target_family_from_yaml(BUNDLED_FAMILIES[0].1)
            .expect("bundled HC32F4XX YAML must parse");
        assert!(
            family.to_ascii_uppercase().contains("HC32F4"),
            "unexpected family name: {family}"
        );

        // 256 / 512 / 1024 KiB variants from the Holychip CMSIS-Pack.
        let expected = [
            ("HC32F401BRCX", 0x0004_0000_u64),
            ("HC32F401BREX", 0x0008_0000_u64),
            ("HC32F401BRGX", 0x0010_0000_u64),
        ];
        for (name, flash_size) in expected {
            let target = registry
                .get_target_by_name(name)
                .unwrap_or_else(|e| panic!("{name} must be in the chip library: {e}"));
            assert!(
                !target.flash_algorithms.is_empty(),
                "{name} needs a flash algorithm to be flashable"
            );
            let nvm: u64 = target
                .memory_map
                .iter()
                .filter(|r| r.as_nvm_region().is_some())
                .map(|r| {
                    let range = r.address_range();
                    range.end - range.start
                })
                .sum();
            assert!(nvm >= flash_size, "{name}: NVM {nvm:#x} < {flash_size:#x}");
            assert!(
                target.memory_map.iter().any(|r| r.as_ram_region().is_some()),
                "{name} needs a RAM region for the flash algorithm"
            );
        }
    }

    /// The bundled OPT.FLM description must actually be accepted by probe-rs
    /// and cover exactly the 16-byte OB block. This catches a corrupted or
    /// mis-addressed algorithm long before any hardware is touched.
    #[test]
    fn bundled_py32_option_byte_algorithm_is_loadable() {
        let mut registry = Registry::from_builtin_families();
        registry
            .add_target_family_from_yaml(option_bytes::PY32_OPT_YAML)
            .expect("bundled PY32 OPT YAML must parse");
        let target = registry
            .get_target_by_name(option_bytes::PY32_OB_VARIANT)
            .expect("bundled PY32 OPT YAML must define its variant");
        assert_eq!(target.flash_algorithms.len(), 1);
        let algo = &target.flash_algorithms[0];
        assert_eq!(algo.flash_properties.address_range.start, 0x1FFF_0E80);
        assert_eq!(algo.flash_properties.address_range.end, 0x1FFF_0E90);
        assert_eq!(algo.flash_properties.page_size, 16);
        assert!(target
            .memory_map
            .iter()
            .any(|r| r.as_nvm_region().is_some() && r.contains(0x1FFF_0E80)));
        // It must NOT be a selectable chip in the general library.
        assert!(!BUNDLED_FAMILIES
            .iter()
            .any(|(name, _)| name.contains("OPT")));
    }

    /// Writing must be impossible for chips whose OB address is not covered by
    /// a bundled algorithm, while reading stays available for all of them.
    #[test]
    fn option_byte_profiles_gate_writes_but_never_reads() {
        assert!(option_bytes::profile_for_chip("PY32F003x8").writable);
        assert!(!option_bytes::profile_for_chip("PY32F002Bxx").writable);
        assert!(!option_bytes::profile_for_chip("STM32F103C8").writable);
        assert_eq!(option_bytes::OB_IMAGE_LEN, 16);
    }

    /// Idempotent probe jobs get another chance after a HID hiccup, while a
    /// configuration-NVM write must never be replayed automatically.
    #[test]
    fn retry_policy_is_limited_to_safe_jobs() {
        let cfg = || Box::new(FlashConfig::default());
        assert!(job_is_retryable(&Job::Connect(cfg())));
        assert!(job_is_retryable(&Job::ReadBack(cfg())));
        assert!(job_is_retryable(&Job::Flash(cfg())));
        assert!(job_is_retryable(&Job::VerifyFirmware(cfg())));
        assert!(job_is_retryable(&Job::EraseAll(cfg())));
        assert!(job_is_retryable(&Job::ReadOptionBytes {
            cfg: cfg(),
            base: 0,
        }));
        assert!(!job_is_retryable(&Job::WriteOptionBytes {
            cfg: cfg(),
            base: 0,
            image: ObImage::default(),
            acknowledged: true,
        }));
        assert!(!job_is_retryable(&Job::RefreshProbes));
        assert_eq!(PROBE_JOB_ATTEMPTS, 3);
    }

    /// Only transport-level failures are retried; a real verification error or
    /// a missing file must be reported immediately.
    #[test]
    fn only_transport_errors_trigger_a_retry() {
        assert!(is_transient_probe_error(&anyhow!(
            "Command ID in response (0x0) does not match sent command ID (Transfer - 0x5)"
        )));
        assert!(is_transient_probe_error(&anyhow!(
            "An ARM specific error occurred: No target responded to the debug port"
        )));
        assert!(is_transient_probe_error(&anyhow!(
            "hidapi error: WinError: WaitTimedOut"
        )));
        assert!(!is_transient_probe_error(&anyhow!("请先选择固件文件")));
        assert!(!is_transient_probe_error(&anyhow!(
            "校验失败: SHA256 不一致"
        )));
        assert!(!is_transient_probe_error(&anyhow!(
            "选项字节反码校验失败，拒绝写入"
        )));
    }

    /// Chip-ID reads are read-only, so a transport hiccup may safely restart
    /// the whole job just like a read-back.
    #[test]
    fn chip_id_read_is_retryable() {
        assert!(job_is_retryable(&Job::ReadChipId {
            cfg: Box::new(FlashConfig::default()),
            custom: Some(0x4001_5800),
        }));
    }

    /// Settings written before this feature existed must still deserialise,
    /// and the harmless pre-flash identification check stays enabled.
    #[test]
    fn older_config_without_chip_id_option_still_loads() {
        let mut value = serde_json::to_value(FlashConfig::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("read_chip_id_before_flash");
        let cfg: FlashConfig = serde_json::from_value(value).unwrap();
        assert!(!cfg.read_chip_id_before_flash);
        assert!(include_str!("backend.rs").contains("collect_chip_id(&mut session, cfg, None)"));
    }

    /// `ChipIdSnapshot::text()` owns warning rendering; the CLI must not
    /// print the same `[WARN]` lines a second time.
    #[test]
    fn chip_id_cli_does_not_duplicate_warnings() {
        let source = include_str!("backend.rs");
        let body = source.split("pub fn chip_id_cli").nth(1).unwrap();
        assert!(!body.contains("for warning in &snapshot.warnings"));
        assert!(include_str!("chip_id.rs").contains("[WARN] {warning}"));
    }

    /// The chip-ID report must cross-check CPUID against the core declared by
    /// the selected target, otherwise a wrong model stays invisible.
    #[test]
    fn chip_id_report_checks_core_consistency() {
        let source = include_str!("backend.rs");
        assert!(source.contains("let expected_core = session"));
        assert!(source.contains("chip_id::core_mismatch_note(expected_core.as_deref(), raw)"));
        assert!(include_str!("chip_id.rs").contains("pub fn core_mismatch_note"));
        // The check runs on the successful CPUID read only, never on failure.
        let block = source.split("match core.read_word_32(chip_id::SCB_CPUID)").nth(1).unwrap();
        assert!(block.contains("core_mismatch_note"));
    }

    /// A probe with a dead HID endpoint must produce an actionable message
    /// immediately, not nine futile retries of the same connection attempt.
    #[test]
    fn unresponsive_probe_is_not_replayed_and_has_an_actionable_hint() {
        let raw = "hidapi error: WinError: WaitTimedOut";
        let err: anyhow::Error = ProbeUnresponsive(raw.to_owned()).into();
        assert!(!is_transient_probe_error(&err));
        let text = format!("{err:#}");
        assert!(text.contains("重新插拔 DAPLink USB"), "{text}");
        assert!(text.contains(raw), "{text}");
        assert!(is_probe_open_transient(raw));
        assert!(probe_open_hint(raw).contains("重新插拔"));
        assert!(probe_open_hint("GetLastError=5").contains("独占"));
        assert_eq!(probe_open_hint("descriptor parse failed"), "打开探针失败");
    }

    /// The GUI「连接测试」has a headless twin, and probe opening re-enumerates
    /// instead of retrying on a stale `Lister` snapshot.
    #[test]
    fn connection_test_and_probe_open_recovery_are_wired() {
        let source = include_str!("backend.rs");
        for needle in [
            "fn open_probe",
            "list_all()",
            "pub fn connect_cli",
            "ProbeUnresponsive",
        ] {
            assert!(source.contains(needle), "missing probe recovery: {needle}");
        }
        assert!(include_str!("main.rs").contains("--connect"));
    }

    /// Chips without a bundled vendor map still get a generic identity, so
    /// selecting e.g. an MM32/HC32 part does not produce an empty report.
    #[test]
    fn generic_chips_are_identified_through_the_coresight_rom_table() {
        let source = include_str!("backend.rs");
        assert!(source.contains("fn read_rom_table_identity"));
        assert!(source.contains("Component::Class1RomTable"));
        assert!(source.contains("\"ROM Table\""));
        let chip_id = include_str!("chip_id.rs");
        assert!(chip_id.contains("\"ROM Table\""));
        assert!(chip_id.contains("IdKind::Generic"));
    }

    /// Network CMSIS-DAP probes are selected by a placeholder VID:PID plus
    /// their address, because USB enumeration can never find them.
    #[test]
    fn network_probe_selectors_cover_both_backends() {
        let elk = parse_network_probe("dap.local").unwrap();
        assert_eq!(elk.vendor_id, 0x0000);
        assert_eq!(elk.product_id, 0x0002);
        assert_eq!(elk.serial_number.as_deref(), Some("dap.local"));

        // A bare host:port must not be mistaken for an explicit backend prefix.
        let with_port = parse_network_probe("192.168.1.50:3240").unwrap();
        assert_eq!(with_port.product_id, 0x0002);
        assert_eq!(
            with_port.serial_number.as_deref(),
            Some("192.168.1.50:3240")
        );

        let tcp = parse_network_probe("tcp:127.0.0.1:4441").unwrap();
        assert_eq!(tcp.product_id, 0x0001);
        assert_eq!(tcp.serial_number.as_deref(), Some("127.0.0.1:4441"));

        assert!(parse_network_probe("").is_none());
        assert!(parse_network_probe("   ").is_none());
        assert!(parse_network_probe("elaphurelink:").is_none());
    }

    #[test]
    fn network_probe_entry_matches_the_stable_identity() {
        let selector = parse_network_probe("elaphurelink:dap.local").unwrap();
        let entry = network_probe_entry("elaphurelink:dap.local").unwrap();
        assert_eq!(entry.id, network_probe_identity(&selector));
        assert_eq!(entry.id, "0000:0002:dap.local::usb");
        assert!(entry.label.contains("elaphureLink"));
        assert!(network_probe_entry("").is_none());
    }

    /// Configs saved before network probes existed must still deserialise,
    /// and get the same default address as a fresh install.
    #[test]
    fn older_settings_without_network_probe_still_load() {
        let mut value = serde_json::to_value(FlashConfig::default()).unwrap();
        value.as_object_mut().unwrap().remove("network_probe");
        let cfg: FlashConfig = serde_json::from_value(value).unwrap();
        assert_eq!(cfg.network_probe, "dap.local");
    }

    /// The address is prefilled so the network probe shows up in the dropdown
    /// without any manual setup (OpenOCD's elaphurelink.cfg default).
    #[test]
    fn network_probe_defaults_to_the_openocd_address() {
        assert_eq!(FlashConfig::default().network_probe, "dap.local");
        assert!(network_probe_entry(&FlashConfig::default().network_probe).is_some());
    }

    #[test]
    fn network_probe_is_opened_directly_without_usb_enumeration() {
        let source = include_str!("backend.rs");
        assert!(source.contains("parse_network_probe(&cfg.network_probe)"));
        assert!(source.contains("Lister::new().open(selector)"));
        assert!(source.contains("无法连接网络探针"));
        let app = include_str!("app.rs");
        assert!(app.contains("network_probe_entry"));
        assert!(app.contains("sync_network_probe_entry"));
        assert!(app.contains("网络探针（ElaphureLink"));
        let main = include_str!("main.rs");
        assert!(main.contains("--network"));
        assert_eq!(main.matches("apply_network_cli(&mut cfg, &args)").count(), 4);
    }

    #[test]
    fn network_check_reports_backend_and_handles_bad_addresses() {
        assert!(network_check("").contains("为空"));
        assert!(network_check("tcp:127.0.0.1:1").contains("4441"));
        assert!(network_check("dap.local").contains("3240"));
    }


    #[test]
    fn network_watcher_resolves_backend_ports() {
        let tcp: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(resolve_network_probe_address("tcp:127.0.0.1:1"), vec![tcp]);
        let elaphure = resolve_network_probe_address("192.168.5.5:3240");
        assert_eq!(elaphure[0].port(), 3240);
        let (online, detail) = network_probe_reachable(&[tcp]);
        assert!(!online);
        assert!(detail.contains("127.0.0.1:1"));
    }

}
