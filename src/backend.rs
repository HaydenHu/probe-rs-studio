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
    probe::{list::Lister, DebugProbeInfo, WireProtocol},
    MemoryInterface, Permissions, Session,
};
use sha2::{Digest, Sha256};

use crate::{
    firmware::{self, FirmwareFormat, FirmwareInfo},
    option_bytes::{self, ObImage, ObSnapshot, OB_IMAGE_LEN},
    pack::FlmSpec,
};

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
    pub readback_address: u64,
    pub readback_size: u64,
    pub backup_path: PathBuf,
}

impl Default for FlashConfig {
    fn default() -> Self {
        Self {
            probe_index: 0,
            probe_id: None,
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
    ReadBack(Box<FlashConfig>),
    Flash(Box<FlashConfig>),
    VerifyFirmware(Box<FlashConfig>),
    EraseAll(Box<FlashConfig>),
    Reset(Box<FlashConfig>),
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
            | Job::ReadOptionBytes { .. }
    )
}

/// Only transport-level hiccups justify a retry. A missing firmware file, a
/// failed verification or a rejected option-byte image must surface at once.
fn is_transient_probe_error(err: &anyhow::Error) -> bool {
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
fn connect_with(cfg: &FlashConfig, tx: &Sender<Event>, attach_ob: bool) -> Result<Session> {
    let lister = Lister::new();
    let probes = lister.list_all();
    // Prefer the stable device identity: enumeration indices shift whenever a
    // probe is unplugged, so an index alone could attach to the wrong device.
    let info = match cfg.probe_id.as_deref() {
        Some(id) if !id.is_empty() => probes
            .iter()
            .find(|p| probe_identity(p) == id)
            .ok_or_else(|| anyhow!("所选调试探针已断开，请在「设备」页重新选择"))?,
        _ => probes
            .get(cfg.probe_index)
            .ok_or_else(|| anyhow!("探针索引 {} 不存在，请先刷新探针列表", cfg.probe_index))?,
    };
    log(
        tx,
        Level::Info,
        format!("打开探针: {} [{:04x}:{:04x}]", info.identifier, info.vendor_id, info.product_id),
    );
    let mut probe = lister.open(info).context("打开探针失败（可能被其它程序占用）")?;

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
}
