//! Hardware operations, executed on worker threads.
//!
//! Everything talks to the probe-rs *library* directly - no subprocess, no CLI,
//! no RPC. The UI thread only receives `Event`s over a channel.

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
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

use crate::firmware::{self, FirmwareFormat, FirmwareInfo};

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
    Connect(Box<FlashConfig>),
    ReadBack(Box<FlashConfig>),
    Flash(Box<FlashConfig>),
    VerifyFirmware(Box<FlashConfig>),
    EraseAll(Box<FlashConfig>),
    Reset(Box<FlashConfig>),
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

fn run(job: Job, tx: &Sender<Event>) -> Result<String> {
    match job {
        Job::RefreshProbes => refresh_probes(tx),
        Job::LoadChips => load_chips(tx),
        Job::ChipInfo { chip, cfg } => chip_info(&chip, Some(cfg.as_ref()), tx),
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
    let target = registry
        .get_target_by_name(&cfg.chip)
        .with_context(|| format!("芯片 {} 不在描述库中", cfg.chip))?;
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
}
