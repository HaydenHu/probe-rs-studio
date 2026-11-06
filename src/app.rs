//! DSH-desktop-style UI: dark rail, tinted sidebar, card-based centre,
//! console log and a persistent status bar.

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::Ordering,
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    time::Duration,
};

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use crate::{
    backend::{self, ChipItem, Event, FlashConfig, Job, Level, ProbeItem, Protocol, WatcherControl},
    firmware::{self, FirmwareFormat, FirmwareInfo},
    icons::{IconKind, IconSet},
    pack::{self, FlmSpec},
    serial::{self, SerialPort},
    theme::{self, Palette, ThemeMode},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    Workbench,
    Serial,
    Chips,
    Logs,
    Settings,
}

impl Nav {
    /// Shown in the in-app top bar. The OS window title already carries the
    /// product name, so the inner bar shows context instead of repeating it.
    fn title(self) -> &'static str {
        match self {
            Nav::Workbench => "工作台",
            Nav::Serial => "串口识别",
            Nav::Chips => "芯片库",
            Nav::Logs => "运行日志",
            Nav::Settings => "设置",
        }
    }
}

/// The one and only "add chip support" button, shared by the chip-library
/// sidebar and the settings page. A pack is simply a container around FLM
/// algorithms, so users should not have to guess which button fits their file.
const CHIP_SOURCE_BUTTON: &str = "加载芯片支持…";

/// Every file picker offers an "all files" option: call this *first* in the
/// chain for open dialogs (so no vendor file is hidden by its extension) and
/// *last* for save dialogs (so the default extension stays the document one).
fn with_all_files(dialog: rfd::FileDialog) -> rfd::FileDialog {
    dialog.add_filter("全部文件 (*.*)", &["*"])
}

fn default_true() -> bool {
    true
}

/// Guard against accidentally dropping a huge file onto the window. Firmware
/// images are small; the viewer only keeps this many bytes in memory.
const HEX_VIEW_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HexSource {
    /// Decoded firmware image, addressed by its load address.
    Image,
    /// Raw bytes of the file on disk.
    File,
}

impl HexSource {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "固件镜像",
            Self::File => "文件原始字节",
        }
    }
}

/// Cheap-to-clone snapshot of the loaded firmware. `Arc` lets the hex viewer
/// hold the bytes without borrowing `App` while its controls mutate view state.
#[derive(Clone)]
struct FirmwareView {
    raw: Arc<[u8]>,
    image: Option<Arc<[u8]>>,
    image_base: u64,
}

fn read_file_preview(path: &std::path::Path) -> Vec<u8> {
    use std::io::Read as _;
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    while out.len() < HEX_VIEW_LIMIT {
        let n = file.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out.truncate(HEX_VIEW_LIMIT);
    out
}

fn resolve_hex_view(
    view: Option<&FirmwareView>,
    source: HexSource,
) -> (&[u8], u64, &'static str) {
    match view {
        Some(v) if source == HexSource::Image && v.image.is_some() => (
            v.image.as_deref().unwrap_or(&[]),
            v.image_base,
            "固件镜像",
        ),
        Some(v) => (&v.raw, 0, "文件原始字节"),
        None => (&[], 0, "—"),
    }
}

/// One classic hex-dump line: offset, padded hex bytes, ASCII gutter.
fn format_hex_row(
    data: &[u8],
    base: u64,
    row: usize,
    cols: usize,
    ascii: bool,
) -> (String, String, String) {
    let start = row.saturating_mul(cols);
    let chunk = &data[start.min(data.len())..start.saturating_add(cols).min(data.len())];
    let offset = format!("{:08X}", base.saturating_add(start as u64));
    let mut hex = String::with_capacity(cols * 3 + 2);
    for i in 0..cols {
        if i > 0 {
            hex.push(' ');
        }
        if cols > 8 && i == cols / 2 {
            hex.push(' ');
        }
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02X}")),
            None => hex.push_str("  "),
        }
    }
    let gutter = if ascii {
        chunk
            .iter()
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    char::from(*b)
                } else {
                    '.'
                }
            })
            .collect()
    } else {
        String::new()
    };
    (offset, hex, gutter)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Settings {
    config: FlashConfig,
    theme: ThemeMode,
    #[serde(default = "default_true")]
    auto_detect: bool,
    recent: Vec<PathBuf>,
    /// Chip models the user has actually picked / connected / flashed, newest
    /// first. `default` keeps settings written by older versions readable.
    #[serde(default)]
    recent_chips: Vec<String>,
    addr_text: String,
    size_text: String,
    bin_base_text: String,
    speed_text: String,
    chip_search: String,
}

#[derive(Clone)]
struct LogEntry {
    time: String,
    level: Level,
    msg: String,
}

/// Supported core architectures for a standalone `.FLM` import.
const FLM_CORES: &[(&str, &str)] = &[
    ("armv6m", "Cortex-M0 / M0+"),
    ("armv7m", "Cortex-M3"),
    ("armv7em", "Cortex-M4 / M7"),
    ("armv8m", "Cortex-M23 / M33 / M55 / M85"),
];

/// State of the "load .FLM" dialog. Everything the algorithm declares is
/// pre-filled from the ELF; only chip name / core / RAM need user input.
#[derive(Clone)]
struct FlmForm {
    path: PathBuf,
    info: String,
    family: String,
    chips: String,
    core: String,
    flash_base: String,
    flash_size: String,
    ram_base: String,
    ram_size: String,
    load_address: String,
    ap: String,
    error: String,
}

impl FlmForm {
    fn from_file(path: PathBuf, info: &pack::FlmInfo) -> Self {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("FLM_CHIP")
            .to_owned();
        Self {
            path,
            info: info.summary(),
            family: format!("{stem} Series"),
            chips: stem,
            core: "armv7em".to_owned(),
            flash_base: format!("{:#x}", info.flash_base),
            flash_size: format!("{:#x}", info.flash_size),
            ram_base: "0x20000000".to_owned(),
            ram_size: "0x10000".to_owned(),
            load_address: String::new(),
            ap: "0".to_owned(),
            error: String::new(),
        }
    }

    fn to_spec(&self) -> Result<FlmSpec, String> {
        fn number(text: &str, label: &str) -> Result<u64, String> {
            parse_num(text).ok_or_else(|| format!("{label} 不是有效的数字（支持 0x 前缀）"))
        }
        fn optional(text: &str, label: &str) -> Result<Option<u64>, String> {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                number(text, label).map(Some)
            }
        }
        let chips: Vec<String> = self
            .chips
            .split([',', '，', ';', '；'])
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if chips.is_empty() {
            return Err("请填写至少一个芯片型号".to_owned());
        }
        if self.family.trim().is_empty() {
            return Err("系列名称不能为空".to_owned());
        }
        let ap = number(&self.ap, "AP 编号")?;
        if ap > u8::MAX as u64 {
            return Err("AP 编号必须在 0..255 之间".to_owned());
        }
        Ok(FlmSpec {
            family: self.family.trim().to_owned(),
            chips,
            core: self.core.clone(),
            flash_base: optional(&self.flash_base, "Flash 起始地址")?,
            flash_size: optional(&self.flash_size, "Flash 容量")?,
            ram_base: number(&self.ram_base, "RAM 起始地址")?,
            ram_size: number(&self.ram_size, "RAM 容量")?,
            load_address: optional(&self.load_address, "固定加载地址")?,
            ap: ap as u8,
        })
    }
}

pub struct App {
    icons: IconSet,
    cfg: FlashConfig,
    theme: ThemeMode,
    applied_theme: ThemeMode,
    auto_detect: bool,
    watch: Arc<WatcherControl>,
    last_hotplug: Option<String>,
    nav: Nav,
    probes: Vec<ProbeItem>,
    chips: Vec<ChipItem>,
    /// Currently attached COM ports, maintained by `spawn_serial_watcher`.
    ports: Vec<SerialPort>,
    selected_port: Option<String>,
    chip_search: String,
    chip_details: String,
    firmware: Option<FirmwareInfo>,
    /// Bytes shown by the hex viewer (decoded image + raw file preview).
    firmware_view: Option<FirmwareView>,
    hex_source: HexSource,
    hex_columns: usize,
    hex_ascii: bool,
    logs: Vec<LogEntry>,
    show_debug: bool,
    rx: Receiver<Event>,
    tx: Sender<Event>,
    busy: bool,
    busy_label: String,
    progress: f32,
    progress_label: String,
    connection: Option<String>,
    last_result: Option<(bool, String, f32)>,
    recent: Vec<PathBuf>,
    recent_chips: Vec<String>,
    addr_text: String,
    size_text: String,
    bin_base_text: String,
    speed_text: String,
    erase_armed: bool,
    log_panel_open: bool,
    config_path: PathBuf,
    last_saved: String,
    /// `Some` while the standalone .FLM import dialog is open.
    flm_form: Option<FlmForm>,
}

/// How many recently confirmed chip models are kept in the settings file.
const RECENT_CHIPS_LIMIT: usize = 8;

/// History entries matching the current search text (all of them when the
/// search box is empty), in most-recently-used order.
fn matched_recent_chips(history: &[String], needle: &str) -> Vec<String> {
    let needle = needle.trim().to_ascii_lowercase();
    history
        .iter()
        .filter(|name| needle.is_empty() || name.to_ascii_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// Stable sort key: recent models first (in recency order); everything else
/// keeps its original registry order.
fn recent_chip_rank(history: &[String], name: &str) -> usize {
    history
        .iter()
        .position(|h| h.eq_ignore_ascii_case(name))
        .unwrap_or(history.len())
}

/// Push a confirmed model to the front, newest first, case-insensitively
/// de-duplicated and bounded. Kept free of `App` so it can be unit-tested.
fn push_recent_chip(history: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    history.retain(|c| !c.eq_ignore_ascii_case(name));
    history.insert(0, name.to_owned());
    history.truncate(RECENT_CHIPS_LIMIT);
}

/// Settings written before this feature have no history at all. Seed it with
/// the model the user had already selected, so the shortcut row is useful the
/// first time the upgraded build starts instead of staying empty until the
/// next manual chip pick. An existing history is never overwritten.
fn seed_recent_chips(history: &mut Vec<String>, current: &str) {
    if history.is_empty() {
        push_recent_chip(history, current);
    }
}

fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim().replace('_', "");
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("probe-rs-studio.json")))
        .unwrap_or_else(|| PathBuf::from("probe-rs-studio.json"))
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let path = config_path();
        let saved: Option<Settings> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let mut s = saved.unwrap_or_else(|| Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            recent_chips: vec![],
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        });
        let seeded_chip = s.config.chip.clone();
        seed_recent_chips(&mut s.recent_chips, &seeded_chip);
        theme::apply(&cc.egui_ctx, s.theme);

        let (tx, rx) = channel();
        let watch = Arc::new(WatcherControl::new(s.auto_detect));
        let repaint_ctx = cc.egui_ctx.clone();
        let serial_repaint_ctx = repaint_ctx.clone();
        let mut app = Self {
            icons: IconSet::load(),
            cfg: s.config,
            theme: s.theme,
            applied_theme: s.theme,
            auto_detect: s.auto_detect,
            watch: watch.clone(),
            last_hotplug: None,
            nav: Nav::Workbench,
            probes: vec![],
            chips: vec![],
            ports: vec![],
            selected_port: None,
            chip_search: s.chip_search,
            chip_details: String::new(),
            firmware: None,
            firmware_view: None,
            hex_source: HexSource::Image,
            hex_columns: 16,
            hex_ascii: true,
            logs: vec![],
            show_debug: true,
            rx,
            tx,
            busy: false,
            busy_label: String::new(),
            progress: 0.0,
            progress_label: String::new(),
            connection: None,
            last_result: None,
            recent: s.recent,
            recent_chips: s.recent_chips,
            addr_text: s.addr_text,
            size_text: s.size_text,
            bin_base_text: s.bin_base_text,
            speed_text: s.speed_text,
            erase_armed: false,
            log_panel_open: true,
            config_path: path,
            last_saved: String::new(),
            flm_form: None,
        };
        app.push_log(Level::Info, "ProbeRS Studio 已启动 · 直接链接 probe-rs 库");
        app.push_log(Level::Debug, format!("配置文件: {}", app.config_path.display()));
        // Warm up the chip registry; the probe list is now maintained by the
        // hot-plug watcher (its first sample is taken immediately).
        backend::spawn_probe_watcher(
            app.tx.clone(),
            watch.clone(),
            Duration::from_millis(1500),
            move || repaint_ctx.request_repaint(),
        );
        // COM ports (including a DAPLink's built-in virtual COM port) are
        // watched separately: registry/PnP reads never contend with the probe.
        backend::spawn_serial_watcher(
            app.tx.clone(),
            watch,
            Duration::from_millis(2000),
            move || serial_repaint_ctx.request_repaint(),
        );
        backend::spawn(Job::LoadChips, app.tx.clone());
        // Restore the previously used firmware if it still exists.
        if let Some(p) = app.cfg.firmware.clone() {
            if p.exists() {
                app.set_firmware(p);
            }
        }
        app
    }

    fn push_log(&mut self, level: Level, msg: impl Into<String>) {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        self.logs.push(LogEntry {
            time,
            level,
            msg: msg.into(),
        });
        if self.logs.len() > 2000 {
            self.logs.drain(0..500);
        }
    }

    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Log(level, msg) => self.push_log(level, msg),
                Event::Probes(items) => {
                    self.apply_probes(items);
                }
                Event::ProbeHotplug { items, added, removed } => {
                    let was_ready = self.probe_ready();
                    let had_connection = self.connection.is_some();
                    let ready = self.apply_probes(items);
                    self.last_hotplug =
                        Some(chrono::Local::now().format("%H:%M:%S").to_string());
                    if !ready {
                        if had_connection {
                            self.connection = None;
                            self.push_log(Level::Warn, "探针已拔出，连接状态已清除");
                        }
                        if !removed.is_empty() {
                            self.push_log(Level::Warn, "当前所选探针已断开，请重新选择设备");
                        }
                    } else if !was_ready && !added.is_empty() {
                        self.push_log(Level::Info, "调试探针已就绪，可重新连接");
                    }
                }
                Event::Chips(items) => self.chips = items,
                Event::ChipDetails(text) => self.chip_details = text,
                Event::SerialPorts(items) => {
                    self.ports = items;
                    if self
                        .selected_port
                        .as_deref()
                        .is_some_and(|name| !self.ports.iter().any(|p| p.name == name))
                    {
                        self.selected_port = None;
                    }
                }
                Event::Connected(text) => self.connection = Some(text),
                Event::Progress { value, label } => {
                    self.progress = value;
                    self.progress_label = label;
                }
                Event::Finished { ok, summary, elapsed } => {
                    // Background warm-up jobs must not overwrite the status line.
                    let was_explicit = self.busy;
                    self.busy = false;
                    self.busy_label.clear();
                    if was_explicit {
                        self.last_result = Some((ok, summary, elapsed));
                        if ok {
                            self.progress = 1.0;
                        }
                    }
                }
            }
        }
    }

    /// Replace the probe list while keeping the *same physical probe* selected.
    /// Returns whether that probe is still attached.
    fn apply_probes(&mut self, items: Vec<ProbeItem>) -> bool {
        let chosen = self.cfg.probe_id.clone();
        self.probes = items;
        if self.probes.is_empty() {
            self.cfg.probe_index = 0;
            return false;
        }
        match chosen.as_deref() {
            Some(id) => {
                if let Some(i) = self.probes.iter().position(|x| x.id == id) {
                    // Enumeration order may have changed after a replug.
                    self.cfg.probe_index = i;
                    true
                } else {
                    self.cfg.probe_index = self.cfg.probe_index.min(self.probes.len() - 1);
                    false
                }
            }
            None => {
                // Legacy config without a device identity: adopt the indexed probe.
                let i = self.cfg.probe_index.min(self.probes.len() - 1);
                self.cfg.probe_index = i;
                self.cfg.probe_id = Some(self.probes[i].id.clone());
                true
            }
        }
    }

    /// Select a probe by its stable identity; enumeration indices change after
    /// a USB replug, so the id is what gets persisted.
    fn select_probe(&mut self, i: usize) {
        if let Some(item) = self.probes.get(i) {
            self.cfg.probe_index = i;
            self.cfg.probe_id = Some(item.id.clone());
            self.connection = None;
        }
    }

    fn probe_ready(&self) -> bool {
        self.cfg
            .probe_id
            .as_deref()
            .is_some_and(|id| self.probes.iter().any(|x| x.id == id))
    }

    fn can_run(&self) -> bool {
        !self.busy && self.probe_ready()
    }

    fn snapshot(&self) -> Settings {
        Settings {
            config: self.cfg.clone(),
            theme: self.theme,
            auto_detect: self.auto_detect,
            recent: self.recent.clone(),
            recent_chips: self.recent_chips.clone(),
            addr_text: self.addr_text.clone(),
            size_text: self.size_text.clone(),
            bin_base_text: self.bin_base_text.clone(),
            speed_text: self.speed_text.clone(),
            chip_search: self.chip_search.clone(),
        }
    }

    fn persist(&mut self) {
        let Ok(text) = serde_json::to_string_pretty(&self.snapshot()) else {
            return;
        };
        if text != self.last_saved {
            let _ = fs::write(&self.config_path, &text);
            self.last_saved = text;
        }
    }

    fn sync_config(&mut self) {
        if let Some(v) = parse_num(&self.addr_text) {
            self.cfg.readback_address = v;
        }
        if let Some(v) = parse_num(&self.size_text) {
            self.cfg.readback_size = v.max(1);
        }
        if let Some(v) = parse_num(&self.bin_base_text) {
            self.cfg.bin_base = v;
        }
        if let Some(v) = parse_num(&self.speed_text) {
            self.cfg.speed_khz = v.clamp(1, 50_000) as u32;
        }
    }

    /// Record a chip model the user has just committed to. Newest first,
    /// case-insensitively de-duplicated and bounded so the sidebar stays compact.
    fn remember_chip(&mut self, name: &str) {
        push_recent_chip(&mut self.recent_chips, name);
    }

    /// Apply a chip choice coming from the library list, a history shortcut or
    /// the workbench. `load_details` mirrors the original list-click behaviour.
    fn select_chip(&mut self, name: &str, load_details: bool) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.cfg.chip = name.clone();
        self.chip_details.clear();
        self.connection = None;
        self.remember_chip(&name);
        if load_details {
            let tx = self.tx.clone();
            backend::spawn(
                Job::ChipInfo {
                    chip: name,
                    cfg: Box::new(self.cfg.clone()),
                },
                tx,
            );
        }
    }

    fn start_job(&mut self, job: Job, label: &str) {
        if self.busy {
            self.push_log(Level::Warn, "上一个任务尚未结束，请稍候");
            return;
        }
        self.sync_config();
        // A manually typed model becomes "confirmed" as soon as it is used for
        // a real probe operation, so it joins the recent list too.
        if matches!(
            &job,
            Job::Connect(_)
                | Job::ReadBack(_)
                | Job::Flash(_)
                | Job::VerifyFirmware(_)
                | Job::EraseAll(_)
                | Job::Reset(_)
        ) {
            let chip = self.cfg.chip.clone();
            self.remember_chip(&chip);
        }
        self.busy = true;
        self.busy_label = label.to_owned();
        self.progress = 0.0;
        self.progress_label = label.to_owned();
        self.push_log(Level::Info, format!("▶ {label}"));
        backend::spawn(job, self.tx.clone());
    }

    fn set_firmware(&mut self, path: PathBuf) {
        let format = FirmwareFormat::from_path(&path).unwrap_or(self.cfg.format);
        self.cfg.format = format;
        self.cfg.firmware = Some(path.clone());
        // The hex viewer shows raw bytes even when parsing fails (an ELF, a
        // malformed HEX, or an unknown extension is still worth inspecting).
        let raw = read_file_preview(&path);
        match firmware::inspect(&path, format, self.cfg.bin_base) {
            Ok(info) => {
                self.push_log(
                    Level::Ok,
                    format!("固件已载入: {} ({} 字节)", path.display(), info.size),
                );
                self.firmware_view = Some(FirmwareView {
                    raw: Arc::from(raw.as_slice()),
                    image: info.image.as_deref().map(Arc::from),
                    image_base: info.image_base.unwrap_or(0),
                });
                self.firmware = Some(info);
            }
            Err(e) => {
                self.push_log(Level::Error, format!("解析固件失败: {e:#}"));
                self.firmware_view = Some(FirmwareView {
                    raw: Arc::from(raw.as_slice()),
                    image: None,
                    image_base: 0,
                });
                self.firmware = None;
                // A file without a decodable image is best shown as raw bytes.
                self.hex_source = HexSource::File;
            }
        }
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(8);
    }

    fn card(ui: &mut Ui, p: Palette, title: &str, body: impl FnOnce(&mut Ui)) {
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(12.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(title).size(14.0).strong().color(p.text));
                ui.add_space(10.0);
                body(ui);
            });
    }

    fn primary_button(ui: &mut Ui, p: Palette, text: &str, enabled: bool) -> egui::Response {
        let btn = egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
            .fill(if enabled { p.accent } else { p.card_alt })
            .min_size(Vec2::new(132.0, 36.0));
        ui.add_enabled(enabled, btn)
    }

    fn ghost_button(ui: &mut Ui, p: Palette, text: &str, enabled: bool) -> egui::Response {
        let btn = egui::Button::new(RichText::new(text).color(p.text2))
            .fill(p.card_alt)
            .stroke(Stroke::new(1.0, p.border))
            .min_size(Vec2::new(104.0, 32.0));
        let _ = p;
        ui.add_enabled(enabled, btn)
    }

    fn draw_top(&mut self, root: &mut egui::Ui, p: Palette) {
        egui::Panel::top("top_bar")
            .exact_size(52.0)
            .frame(Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    // Brand mark: logo image on the accent plate.
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(30.0, 30.0), Sense::hover());
                    ui.painter().rect_filled(rect, CornerRadius::same(8), p.accent);
                    let logo_rect = Rect::from_center_size(rect.center(), Vec2::splat(25.0));
                    self.icons
                        .logo
                        .paint(ui.painter(), logo_rect, Color32::WHITE);
                    ui.add_space(6.0);
                    // The OS title bar owns the product name; keep this bar
                    // contextual so the same words are not printed twice.
                    ui.label(
                        RichText::new(self.nav.title())
                            .size(15.0)
                            .strong()
                            .color(p.text),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("BETA")
                            .size(10.0)
                            .strong()
                            .color(p.accent)
                            .background_color(p.accent_soft),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let theme_label = match self.theme {
                            ThemeMode::Dark => "浅色",
                            ThemeMode::Light => "深色",
                        };
                        if Self::ghost_button(ui, p, theme_label, true).clicked() {
                            self.theme = match self.theme {
                                ThemeMode::Dark => ThemeMode::Light,
                                ThemeMode::Light => ThemeMode::Dark,
                            };
                        }
                        if Self::ghost_button(ui, p, "芯片库", true).clicked() {
                            self.nav = Nav::Chips;
                        }
                        let log_label = if self.log_panel_open { "隐藏日志" } else { "显示日志" };
                        if Self::ghost_button(ui, p, log_label, true).clicked() {
                            self.log_panel_open = !self.log_panel_open;
                        }
                        ui.label(
                            RichText::new(format!("芯片库 {} 型号", self.chips.len()))
                                .size(11.0)
                                .color(p.text3),
                        );
                    });
                });
            });
    }

    fn draw_rail(&mut self, root: &mut egui::Ui, p: Palette) {
        egui::Panel::left("rail")
            .exact_size(62.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(p.rail)
                    .stroke(Stroke::new(1.0, p.rail_border))
                    .inner_margin(egui::Margin::symmetric(8, 12)),
            )
            .show(root, |ui| {
                ui.vertical_centered(|ui| {
                    let items: [(Nav, IconKind, &str); 5] = [
                        (Nav::Workbench, IconKind::Workbench, "工作台"),
                        (Nav::Serial, IconKind::Serial, "串口识别 / 虚拟串口"),
                        (Nav::Chips, IconKind::Chips, "芯片库"),
                        (Nav::Logs, IconKind::Logs, "运行日志"),
                        (Nav::Settings, IconKind::Settings, "设置"),
                    ];
                    for (nav, kind, label) in items {
                        let selected = self.nav == nav;
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(44.0, 46.0), Sense::click());
                        let bg = if selected {
                            p.accent
                        } else if resp.hovered() {
                            p.rail_hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
                        let fg = if selected {
                            Color32::WHITE
                        } else if resp.hovered() {
                            p.text
                        } else {
                            p.rail_icon
                        };
                        let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(26.0));
                        self.icons.get(kind).paint(ui.painter(), icon_rect, fg);
                        let resp = resp.on_hover_text(label);
                        if resp.clicked() {
                            self.nav = nav;
                        }
                        ui.add_space(6.0);
                    }
                });
            });
    }

    fn draw_sidebar(&mut self, root: &mut egui::Ui, p: Palette) {
        egui::Panel::left("sidebar")
            .exact_size(248.0)
            .resizable(false)
            .frame(Frame::new().fill(p.sidebar).inner_margin(14.0))
            .show(root, |ui| match self.nav {
                Nav::Workbench => self.sidebar_workbench(ui, p),
                Nav::Serial => self.sidebar_serial(ui, p),
                Nav::Chips => self.sidebar_chips(ui, p),
                Nav::Logs => self.sidebar_logs(ui, p),
                Nav::Settings => self.sidebar_settings(ui, p),
            });
    }

    /// The narrow "current session" rail owns every control: connection,
    /// firmware, flashing options and actions. The centre pane is reserved
    /// for the hex viewer, so it must stay scrollable on short windows.
    fn sidebar_workbench(&mut self, ui: &mut Ui, p: Palette) {
        let (state, color) = match &self.connection {
            Some(_) => ("已连接", p.success),
            None => ("未连接", p.text3),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("当前会话").size(13.0).strong().color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("● {state}")).size(10.0).color(color));
            });
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.session_connection(ui, p);
                ui.add_space(14.0);
                self.session_firmware(ui, p);
                ui.add_space(14.0);
                self.session_actions(ui, p);
                ui.add_space(14.0);
                self.session_recents(ui, p);
            });
    }

    fn session_connection(&mut self, ui: &mut Ui, p: Palette) {
        Self::section_title(ui, p, "连接配置");
        ui.add_space(6.0);
        ui.label(RichText::new("调试探针").size(10.0).color(p.text3));
        let selected = self
            .cfg
            .probe_id
            .as_deref()
            .and_then(|id| self.probes.iter().find(|x| x.id == id))
            .map(|x| x.label.clone())
            .unwrap_or_else(|| {
                if self.probes.is_empty() {
                    "未检测到探针".to_owned()
                } else {
                    "所选探针已断开，请重新选择".to_owned()
                }
            });
        let w = ui.available_width();
        egui::ComboBox::from_id_salt("sidebar_probe_combo")
            .selected_text(RichText::new(selected).size(10.5).color(p.text))
            .width(w)
            .show_ui(ui, |ui| {
                if self.probes.is_empty() {
                    ui.label(RichText::new("请插入调试器").size(10.0).color(p.text3));
                }
                for (i, item) in self.probes.clone().iter().enumerate() {
                    let chosen = self.cfg.probe_id.as_deref() == Some(item.id.as_str());
                    if ui
                        .selectable_label(chosen, item.label.as_str())
                        .on_hover_text(item.detail.as_str())
                        .clicked()
                    {
                        self.select_probe(i);
                    }
                }
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("协议").size(10.0).color(p.text3));
            egui::ComboBox::from_id_salt("sidebar_proto_combo")
                .selected_text(proto_label(self.cfg.protocol))
                .width(66.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.cfg.protocol, Protocol::Swd, "SWD");
                    ui.selectable_value(&mut self.cfg.protocol, Protocol::Jtag, "JTAG");
                });
            ui.label(RichText::new("kHz").size(10.0).color(p.text3));
            ui.add_sized(
                [ui.available_width(), 24.0],
                egui::TextEdit::singleline(&mut self.speed_text),
            );
        });
        ui.add_space(6.0);
        ui.label(RichText::new("目标芯片").size(10.0).color(p.text3));
        let w = ui.available_width();
        ui.add_sized(
            [w, 24.0],
            egui::TextEdit::singleline(&mut self.cfg.chip).hint_text("如 PY32F003x8"),
        );
        if !self.recent_chips.is_empty() {
            ui.add_space(6.0);
            let recent = self.recent_chips.clone();
            let mut picked: Option<String> = None;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("最近").size(9.0).color(p.text3));
                for name in &recent {
                    let active = self.cfg.chip.eq_ignore_ascii_case(name);
                    let btn = egui::Button::new(
                        RichText::new(name)
                            .size(9.0)
                            .color(if active { p.accent } else { p.text2 }),
                    )
                    .fill(if active { p.accent_soft } else { p.card_alt });
                    if ui.add(btn).clicked() {
                        picked = Some(name.clone());
                    }
                }
            });
            if let Some(name) = picked {
                self.select_chip(&name, false);
            }
        }
        ui.add_space(8.0);
        ui.checkbox(&mut self.cfg.connect_under_reset, "复位下连接");
        ui.add_space(6.0);
        let half = (ui.available_width() - 6.0) / 2.0;
        ui.horizontal(|ui| {
            if Self::sidebar_button(ui, p, "连接测试", self.can_run(), false, half).clicked() {
                self.start_job(Job::Connect(Box::new(self.cfg.clone())), "连接测试");
            }
            if Self::sidebar_button(ui, p, "芯片详情", !self.busy, false, half).clicked() {
                let chip = self.cfg.chip.clone();
                self.remember_chip(&chip);
                backend::spawn(
                    Job::ChipInfo {
                        chip,
                        cfg: Box::new(self.cfg.clone()),
                    },
                    self.tx.clone(),
                );
            }
        });
        if !self.chip_details.is_empty() {
            let details = self.chip_details.clone();
            egui::CollapsingHeader::new(RichText::new("芯片描述").size(10.0).color(p.text3))
                .default_open(false)
                .show(ui, |ui| {
                    for line in details.lines() {
                        ui.label(RichText::new(line).monospace().size(9.0).color(p.text2));
                    }
                });
        }
    }

    fn session_firmware(&mut self, ui: &mut Ui, p: Palette) {
        Self::section_title(ui, p, "固件文件");
        ui.add_space(6.0);
        let w = ui.available_width();
        let mut picked: Option<PathBuf> = None;
        if Self::sidebar_button(ui, p, "浏览… / 拖入固件", true, false, w).clicked() {
            picked = with_all_files(rfd::FileDialog::new())
                .add_filter("固件", &["hex", "ihex", "elf", "axf", "out", "bin"])
                .pick_file();
        }
        if let Some(path) = picked {
            self.set_firmware(path);
        }
        ui.add_space(6.0);
        ui.label(RichText::new("格式").size(10.0).color(p.text3));
        let before = self.cfg.format;
        egui::ComboBox::from_id_salt("sidebar_format_combo")
            .selected_text(self.cfg.format.label())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for f in [FirmwareFormat::Hex, FirmwareFormat::Elf, FirmwareFormat::Bin] {
                    ui.selectable_value(&mut self.cfg.format, f, f.label());
                }
            });
        if self.cfg.format != before {
            if let Some(path) = self.cfg.firmware.clone() {
                self.set_firmware(path);
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("BIN 基址").size(10.0).color(p.text3));
            ui.add_sized(
                [ui.available_width(), 24.0],
                egui::TextEdit::singleline(&mut self.bin_base_text),
            );
        });
        if let Some(path) = self.cfg.firmware.clone() {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            ui.add_space(6.0);
            ui.label(RichText::new(name).size(10.0).color(p.text))
                .on_hover_text(path.display().to_string());
        }
        if let Some(info) = self.firmware.clone() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "{} · {} 字节 · {} 条记录",
                    info.format.label(),
                    info.size,
                    info.records
                ))
                .size(9.0)
                .color(p.text2),
            );
            ui.label(
                RichText::new(format!("SHA256 {}", &info.sha256[..16]))
                    .monospace()
                    .size(9.0)
                    .color(p.text3),
            );
            if !self.firmware_view.as_ref().is_some_and(|v| v.image.is_some()) {
                ui.label(
                    RichText::new("无解码镜像 · 右侧显示文件原始字节")
                        .size(9.0)
                        .color(p.warn),
                );
            }
        }
    }

    fn session_actions(&mut self, ui: &mut Ui, p: Palette) {
        Self::section_title(ui, p, "烧录选项与操作");
        ui.add_space(6.0);
        ui.checkbox(&mut self.cfg.verify, "写入后校验");
        ui.checkbox(&mut self.cfg.restore_unwritten, "保留未写入字节");
        ui.checkbox(&mut self.cfg.verify_readback, "独立读回比对");
        ui.checkbox(&mut self.cfg.reset_after, "完成后复位运行");
        ui.checkbox(&mut self.cfg.preverify, "预校验跳过相同区域");
        ui.checkbox(&mut self.cfg.chip_erase, "整片擦除");
        ui.add_space(8.0);
        let full = ui.available_width();
        if Self::sidebar_button(ui, p, "开始烧录", self.can_run() && self.firmware.is_some(), true, full)
            .clicked()
        {
            self.start_job(Job::Flash(Box::new(self.cfg.clone())), "烧录固件");
        }
        ui.add_space(6.0);
        let half = (ui.available_width() - 6.0) / 2.0;
        ui.horizontal(|ui| {
            if Self::sidebar_button(ui, p, "独立校验", self.can_run() && self.firmware.is_some(), false, half).clicked() {
                self.start_job(Job::VerifyFirmware(Box::new(self.cfg.clone())), "读回校验");
            }
            if Self::sidebar_button(ui, p, "读回备份", self.can_run(), false, half).clicked() {
                self.start_job(Job::ReadBack(Box::new(self.cfg.clone())), "读回备份");
            }
        });
        ui.horizontal(|ui| {
            if Self::sidebar_button(ui, p, "复位运行", self.can_run(), false, half).clicked() {
                self.start_job(Job::Reset(Box::new(self.cfg.clone())), "复位目标");
            }
            let erase_text = if self.erase_armed { "确认擦除？" } else { "整片擦除" };
            if Self::sidebar_button(ui, p, erase_text, self.can_run(), false, half).clicked() {
                if self.erase_armed {
                    self.erase_armed = false;
                    self.start_job(Job::EraseAll(Box::new(self.cfg.clone())), "整片擦除");
                } else {
                    self.erase_armed = true;
                    self.push_log(Level::Warn, "整片擦除需二次点击确认，会清除芯片全部 Flash");
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("读回地址").size(9.0).color(p.text3));
            ui.add_sized(
                [ui.available_width(), 22.0],
                egui::TextEdit::singleline(&mut self.addr_text),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("长度 (字节)").size(9.0).color(p.text3));
            ui.add_sized(
                [ui.available_width(), 22.0],
                egui::TextEdit::singleline(&mut self.size_text),
            );
        });
        ui.horizontal(|ui| {
            let path = self.cfg.backup_path.display().to_string();
            ui.label(RichText::new(path).size(9.0).color(p.text3))
                .on_hover_text(self.cfg.backup_path.display().to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("更改…").clicked() {
                    if let Some(path) = with_all_files(
                        rfd::FileDialog::new().add_filter("二进制", &["bin"]),
                    )
                    .set_file_name("backup_before.bin")
                    .save_file()
                    {
                        self.cfg.backup_path = path;
                    }
                }
            });
        });
    }

    fn session_recents(&mut self, ui: &mut Ui, p: Palette) {
        Self::section_title(ui, p, "最近固件");
        ui.add_space(6.0);
        if self.recent.is_empty() {
            ui.label(RichText::new("暂无记录").size(10.0).color(p.text3));
        }
        let recent = self.recent.clone();
        for path in recent {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ui
                .add(egui::Button::new(RichText::new(name).size(10.0).color(p.text2)).fill(Color32::TRANSPARENT))
                .on_hover_text(path.display().to_string())
                .clicked()
            {
                self.set_firmware(path);
            }
        }
        ui.add_space(12.0);
        Self::section_title(ui, p, "快捷操作");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("刷新探针").clicked() {
                self.start_job(Job::RefreshProbes, "刷新探针");
            }
            if ui.button("查看日志").clicked() {
                self.nav = Nav::Logs;
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("运行日志固定在底部，可拖拽上边缘调整高度")
                .size(9.0)
                .color(p.text3),
        );
    }

    fn section_title(ui: &mut Ui, p: Palette, text: &str) {
        ui.label(RichText::new(text).size(11.0).strong().color(p.text2));
    }

    fn sidebar_button(
        ui: &mut Ui,
        p: Palette,
        text: &str,
        enabled: bool,
        primary: bool,
        width: f32,
    ) -> egui::Response {
        let btn = egui::Button::new(
            RichText::new(text)
                .size(11.0)
                .color(if primary { Color32::WHITE } else { p.text2 }),
        )
        .fill(if enabled && primary { p.accent } else { p.card_alt })
        .stroke(if primary {
            Stroke::NONE
        } else {
            Stroke::new(1.0, p.border)
        })
        .min_size(Vec2::new(width, 28.0));
        ui.add_enabled(enabled, btn)
    }

    fn sidebar_serial(&mut self, ui: &mut Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("串口识别").size(13.0).strong().color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    self.ports = serial::enumerate();
                    self.push_log(
                        Level::Info,
                        format!("已刷新串口列表 · 当前在线 {} 个", self.ports.len()),
                    );
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.5, p.success);
            ui.label(
                RichText::new("自动检测中 · 插入/拔出自动刷新 (2s)")
                    .size(10.0)
                    .color(p.text3),
            );
        });
        ui.add_space(4.0);
        let vcp = self.ports.iter().filter(|x| x.is_debug_vcp()).count();
        ui.label(
            RichText::new(format!("在线 {} 个 · 调试器虚拟串口 {vcp} 个", self.ports.len()))
                .size(10.0)
                .color(p.text3),
        );
        ui.add_space(10.0);
        if self.ports.is_empty() {
            ui.label(RichText::new("未发现串口，请检查 USB 连接").size(11.0).color(p.text3));
        }
        let ports = self.ports.clone();
        for item in &ports {
            let selected = self.selected_port.as_deref() == Some(item.name.as_str());
            let highlight = item.is_debug_vcp();
            Frame::new()
                .fill(if selected { p.accent_soft } else { p.card })
                .stroke(Stroke::new(
                    1.0,
                    if selected {
                        p.accent
                    } else if highlight {
                        p.accent
                    } else {
                        p.border
                    },
                ))
                .corner_radius(9.0)
                .inner_margin(10.0)
                .show(ui, |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(item.name.as_str())
                                    .size(12.0)
                                    .strong()
                                    .color(if highlight { p.accent } else { p.text }),
                            )
                            .fill(Color32::TRANSPARENT),
                        )
                        .clicked()
                    {
                        self.selected_port = Some(item.name.clone());
                    }
                    ui.label(
                        RichText::new(item.kind.label())
                            .size(10.0)
                            .color(if highlight { p.accent } else { p.text3 }),
                    );
                    ui.label(
                        RichText::new(item.description.as_str())
                            .size(9.0)
                            .color(p.text3),
                    );
                    ui.label(
                        RichText::new(if item.openable {
                            "● 可打开"
                        } else {
                            "● 被占用 / 不可用"
                        })
                        .size(9.0)
                        .color(if item.openable { p.success } else { p.warn }),
                    );
                });
            ui.add_space(6.0);
        }
        ui.add_space(16.0);
        ui.label(RichText::new("关于虚拟串口").size(12.0).strong().color(p.text));
        ui.label(
            RichText::new(
                "DAPLink / CMSIS-DAP 常把 CDC 虚拟串口与调试接口做在同一个 USB 设备上；\
                 这里按 VID/PID 与设备描述自动标注，并可与当前探针关联。",
            )
            .size(10.0)
            .color(p.text3),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new("调试探针仍在工作台的连接配置中选择，不再单独占一个页面。")
                .size(10.0)
                .color(p.text3),
        );
    }

    fn sidebar_chips(&mut self, ui: &mut Ui, p: Palette) {
        ui.label(RichText::new("芯片库").size(13.0).strong().color(p.text));
        ui.add_space(8.0);
        if Self::primary_button(ui, p, CHIP_SOURCE_BUTTON, !self.busy).clicked() {
            self.pick_chip_source();
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "内置库没有的芯片：选厂商 .pack / .pack.zip，\n\
                 或只给一个 .FLM 算法，自动识别并永久保存。\n\
                 pack 里的算法会全部导入，无需逐一挑选；\n\
                 已解压的 pack 目录可直接拖入窗口。",
            )
            .size(9.0)
            .color(p.text3),
        );
        ui.add_space(10.0);
        ui.add_sized(
            [ui.available_width(), 30.0],
            egui::TextEdit::singleline(&mut self.chip_search).hint_text("搜索型号，如 PY32F003"),
        );
        ui.add_space(4.0);
        let history = self.recent_chips.clone();
        // One-tap shortcuts for models the user already confirmed, narrowed by
        // the very same search text typed above.
        let recent_matches = matched_recent_chips(&history, &self.chip_search);
        if !recent_matches.is_empty() {
            ui.label(RichText::new("最近确认的型号").size(10.0).color(p.text3));
            let mut picked: Option<String> = None;
            ui.horizontal_wrapped(|ui| {
                for name in &recent_matches {
                    let active = self.cfg.chip.eq_ignore_ascii_case(name);
                    let btn = egui::Button::new(
                        RichText::new(name)
                            .size(10.0)
                            .color(if active { p.accent } else { p.text2 }),
                    )
                    .fill(if active { p.accent_soft } else { p.card_alt });
                    if ui.add(btn).clicked() {
                        picked = Some(name.clone());
                    }
                }
            });
            if let Some(name) = picked {
                self.select_chip(&name, true);
                self.chip_search.clear();
            }
            ui.add_space(4.0);
        }
        let needle = self.chip_search.trim().to_ascii_lowercase();
        let mut filtered: Vec<&ChipItem> = if needle.is_empty() {
            self.chips.iter().collect()
        } else {
            self.chips
                .iter()
                .filter(|c| c.name.to_ascii_lowercase().contains(&needle))
                .collect()
        };
        // Stable sort: recent models first (recency order), everything else
        // keeps its registry order. Works for both empty and filtered queries.
        filtered.sort_by_key(|c| recent_chip_rank(&history, &c.name));
        let order_hint = if history.is_empty() { "" } else { " · 最近置顶" };
        ui.label(
            RichText::new(format!(
                "匹配 {} / {} 个型号{order_hint}",
                filtered.len(),
                self.chips.len()
            ))
            .size(10.0)
            .color(p.text3),
        );
        let extra = self.chips.iter().filter(|c| c.external).count();
        ui.label(
            RichText::new(format!("其中扩展包型号 {extra} 个"))
                .size(10.0)
                .color(p.accent),
        );
        ui.add_space(6.0);
        let mut picked: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, 34.0, filtered.len(), |ui, range| {
                for i in range {
                    let c = filtered[i];
                    let selected = self.cfg.chip.eq_ignore_ascii_case(&c.name);
                    let is_recent = recent_chip_rank(&history, &c.name) < history.len();
                    Frame::new()
                        .fill(if selected { p.accent_soft } else { Color32::TRANSPARENT })
                        .corner_radius(7.0)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            let r = ui.add(
                                egui::Button::new(
                                    RichText::new(&c.name)
                                        .size(11.0)
                                        .color(if selected { p.accent } else { p.text }),
                                )
                                .fill(Color32::TRANSPARENT),
                            );
                            let mut family = if c.external {
                                format!("{} · 扩展包", c.family)
                            } else {
                                c.family.clone()
                            };
                            if is_recent {
                                family.push_str(" · 最近");
                            }
                            ui.label(
                                RichText::new(family)
                                    .size(9.0)
                                    .color(if c.external || is_recent { p.accent } else { p.text3 }),
                            );
                            if r.clicked() {
                                picked = Some(c.name.clone());
                            }
                        });
                }
            });
        if let Some(name) = picked {
            self.select_chip(&name, true);
        }
    }

    fn sidebar_logs(&mut self, ui: &mut Ui, p: Palette) {
        ui.label(RichText::new("日志选项").size(13.0).strong().color(p.text));
        ui.add_space(10.0);
        ui.checkbox(&mut self.show_debug, "显示调试信息");
        ui.add_space(8.0);
        if ui.button("清空日志").clicked() {
            self.logs.clear();
        }
        if ui.button("导出日志").clicked() {
            self.export_logs();
        }
        ui.add_space(16.0);
        ui.label(RichText::new("统计").size(12.0).strong().color(p.text));
        ui.label(
            RichText::new(format!("共 {} 条记录", self.logs.len()))
                .size(11.0)
                .color(p.text3),
        );
        ui.add_space(16.0);
        ui.label(RichText::new("日志保存在内存中，重启后清空。").size(10.0).color(p.text3));
        let _ = p;
    }

    fn sidebar_settings(&mut self, ui: &mut Ui, p: Palette) {
        ui.label(RichText::new("外观与连接").size(13.0).strong().color(p.text));
        ui.add_space(12.0);
        ui.label(RichText::new("主题").size(11.0).color(p.text3));
        ui.horizontal(|ui| {
            if ui.selectable_label(self.theme == ThemeMode::Dark, "深色").clicked() {
                self.theme = ThemeMode::Dark;
            }
            if ui.selectable_label(self.theme == ThemeMode::Light, "浅色").clicked() {
                self.theme = ThemeMode::Light;
            }
        });
        ui.add_space(12.0);
        ui.label(RichText::new("设备检测").size(11.0).color(p.text3));
        ui.checkbox(&mut self.auto_detect, "自动检测调试探针 / 串口插入拔出");
        ui.label(
            RichText::new("每 1.5 秒检查一次；烧录期间自动暂停，避免占用 USB。")
                .size(10.0)
                .color(p.text3),
        );
        ui.add_space(12.0);
        ui.label(RichText::new("默认协议").size(11.0).color(p.text3));
        ui.horizontal(|ui| {
            if ui.selectable_label(self.cfg.protocol == Protocol::Swd, "SWD").clicked() {
                self.cfg.protocol = Protocol::Swd;
            }
            if ui.selectable_label(self.cfg.protocol == Protocol::Jtag, "JTAG").clicked() {
                self.cfg.protocol = Protocol::Jtag;
            }
        });
        ui.add_space(12.0);
        ui.label(RichText::new("外部芯片描述 (.yaml)").size(11.0).color(p.text3));
        ui.label(
            RichText::new("程序同目录 targets/*.yaml 会在启动时自动加载")
                .size(9.0)
                .color(p.text3),
        );
        let desc = self
            .cfg
            .chip_description
            .as_ref()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|| "未设置".to_owned());
        ui.label(RichText::new(desc).size(10.0).color(p.text2));
        ui.horizontal(|ui| {
            if ui.button("选择…").clicked() {
                if let Some(path) = with_all_files(rfd::FileDialog::new())
                    .add_filter("芯片描述", &["yaml", "yml"])
                    .pick_file()
                {
                    self.cfg.chip_description = Some(path);
                }
            }
            if ui.button("清除").clicked() {
                self.cfg.chip_description = None;
            }
        });
        ui.add_space(16.0);
        ui.label(RichText::new("扩展芯片支持").size(11.0).color(p.text3));
        ui.label(
            RichText::new("一个入口同时支持 .pack / .pack.zip / .FLM；\n转换结果写入 targets/，启动时自动加载。")
                .size(9.0)
                .color(p.text3),
        );
        ui.horizontal(|ui| {
            if ui.button(CHIP_SOURCE_BUTTON).clicked() {
                self.pick_chip_source();
            }
            if ui.button("已解压目录…").clicked() {
                self.pick_chip_source_folder();
            }
        });
        ui.add_space(16.0);
        ui.label(
            RichText::new("所有设置会自动保存到程序同目录的 probe-rs-studio.json")
                .size(10.0)
                .color(p.text3),
        );
        let _ = p;
    }

    /// The single "add chip support" file picker. One filter covers every
    /// supported source; the specialised filters are only there to narrow the
    /// listing if the user wants to.
    fn pick_chip_source(&mut self) {
        if self.busy {
            self.push_log(Level::Warn, "上一个任务尚未结束，请稍候");
            return;
        }
        let Some(path) = with_all_files(rfd::FileDialog::new())
            .add_filter("芯片支持 (.pack / .pack.zip / .FLM)", &["pack", "zip", "FLM", "flm"])
            .add_filter("CMSIS-Pack", &["pack", "zip"])
            .add_filter("Flash 算法", &["FLM", "flm"])
            .pick_file()
        else {
            return;
        };
        self.load_chip_source(path);
    }

    /// Already-extracted pack directory: file pickers cannot select folders,
    /// so this stays a small secondary entry (or just drop the folder on the
    /// window). It routes through exactly the same code as the main button.
    fn pick_chip_source_folder(&mut self) {
        if self.busy {
            self.push_log(Level::Warn, "上一个任务尚未结束，请稍候");
            return;
        }
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.load_chip_source(path);
        }
    }

    /// One dispatcher for everything that can add a chip family: the main
    /// button, the folder picker and drag & drop all end up here.
    fn load_chip_source(&mut self, path: PathBuf) {
        if self.busy {
            self.push_log(Level::Warn, "上一个任务尚未结束，请稍候");
            return;
        }
        match pack::classify_source(&path) {
            Some(pack::ChipSource::Pack) => {
                self.push_log(
                    Level::Info,
                    format!("已选择芯片支持包: {}（自动解析其中的 FLM）", path.display()),
                );
                self.start_job(Job::LoadPack(path), "导入芯片支持");
            }
            Some(pack::ChipSource::Flm) => match pack::inspect_flm(&path) {
                Ok(info) => {
                    self.push_log(
                        Level::Info,
                        format!("已选择 .FLM: {} · {}", path.display(), info.summary()),
                    );
                    self.flm_form = Some(FlmForm::from_file(path, &info));
                }
                Err(err) => self.push_log(Level::Error, format!("读取 .FLM 失败: {err:#}")),
            },
            None => self.push_log(
                Level::Warn,
                format!(
                    "不是芯片支持文件: {}（需要 .pack / .pack.zip / .FLM）",
                    path.display()
                ),
            ),
        }
    }

    fn submit_flm(&mut self, form: &FlmForm) {
        if self.busy {
            return;
        }
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(err) => {
                if let Some(f) = self.flm_form.as_mut() {
                    f.error = err;
                }
                return;
            }
        };
        self.start_job(
            Job::LoadFlm {
                path: form.path.clone(),
                spec: Box::new(spec),
            },
            "导入 .FLM 算法",
        );
        self.flm_form = None;
    }

    /// Modal dialog for a bare `.FLM`: only the values the ELF cannot know.
    fn draw_flm_dialog(&mut self, ctx: &egui::Context, p: Palette) {
        let mut open = true;
        let mut cancel = false;
        let mut submit: Option<FlmForm> = None;
        if let Some(form) = self.flm_form.as_mut() {
            egui::Window::new("加载芯片支持 · 单个 .FLM 算法")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(560.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new("用单个 Flash 算法创建新型号")
                            .size(15.0)
                            .strong()
                            .color(p.text),
                    );
                    ui.label(
                        RichText::new(form.path.display().to_string())
                            .size(10.0)
                            .color(p.text3),
                    );
                    ui.add_space(4.0);
                    ui.label(RichText::new(&form.info).size(11.0).color(p.accent));
                    ui.label(
                        RichText::new(
                            "Flash 地址、容量、页大小、扇区、擦除值、超时与入口地址均从 .FLM 自动读取；\n\
                             只需填写芯片名、内核与 RAM。留空的地址项表示使用 .FLM 自带值。",
                        )
                        .size(10.0)
                        .color(p.text2),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    egui::Grid::new("flm_form_grid")
                        .num_columns(2)
                        .spacing([12.0, 9.0])
                        .show(ui, |ui| {
                            let field = |ui: &mut Ui, value: &mut String, hint: &str| {
                                ui.add_sized(
                                    [320.0, 24.0],
                                    egui::TextEdit::singleline(value).hint_text(hint),
                                );
                            };
                            ui.label(RichText::new("芯片型号（逗号分隔可多个）").color(p.text2));
                            field(ui, &mut form.chips, "MYCHIP_A, MYCHIP_B");
                            ui.end_row();
                            ui.label(RichText::new("系列名称").color(p.text2));
                            field(ui, &mut form.family, "MYCHIP Series");
                            ui.end_row();
                            ui.label(RichText::new("内核").color(p.text2));
                            let current = FLM_CORES
                                .iter()
                                .find(|(v, _)| *v == form.core)
                                .map(|(_, l)| (*l).to_owned())
                                .unwrap_or_else(|| form.core.clone());
                            egui::ComboBox::from_id_salt("flm_core")
                                .selected_text(current)
                                .width(310.0)
                                .show_ui(ui, |ui| {
                                    for (value, label) in FLM_CORES {
                                        ui.selectable_value(
                                            &mut form.core,
                                            (*value).to_owned(),
                                            *label,
                                        );
                                    }
                                });
                            ui.end_row();
                            ui.label(RichText::new("Flash 起始地址 / 容量").color(p.text2));
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [152.0, 24.0],
                                    egui::TextEdit::singleline(&mut form.flash_base),
                                );
                                ui.add_sized(
                                    [152.0, 24.0],
                                    egui::TextEdit::singleline(&mut form.flash_size),
                                );
                            });
                            ui.end_row();
                            ui.label(RichText::new("RAM 起始地址 / 容量").color(p.text2));
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [152.0, 24.0],
                                    egui::TextEdit::singleline(&mut form.ram_base),
                                );
                                ui.add_sized(
                                    [152.0, 24.0],
                                    egui::TextEdit::singleline(&mut form.ram_size),
                                );
                            });
                            ui.end_row();
                            ui.label(RichText::new("固定加载地址（可选）").color(p.text2));
                            field(ui, &mut form.load_address, "留空 = 位置无关算法");
                            ui.end_row();
                            ui.label(RichText::new("AP 编号").color(p.text2));
                            field(ui, &mut form.ap, "0");
                            ui.end_row();
                        });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "RAM 需容纳算法代码、栈与至少 2 个页缓冲，默认 64 KiB；\n\
                             非位置无关的算法才需要填写固定加载地址。",
                        )
                        .size(9.5)
                        .color(p.text3),
                    );
                    if !form.error.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(&form.error).color(p.error));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                        if Self::primary_button(ui, p, "导入并加载芯片", true).clicked() {
                            submit = Some(form.clone());
                        }
                    });
                });
        }
        if cancel || !open {
            self.flm_form = None;
            return;
        }
        if let Some(form) = submit {
            self.submit_flm(&form);
        }
    }

    fn export_logs(&mut self) {
        let Some(path) = with_all_files(
            rfd::FileDialog::new().add_filter("日志", &["txt", "log"]),
        )
        .set_file_name("probe-rs-studio.log")
        .save_file()
        else {
            return;
        };
        let text = self
            .logs
            .iter()
            .map(|l| format!("[{}] [{}] {}", l.time, level_label(l.level), l.msg))
            .collect::<Vec<_>>()
            .join("\n");
        match fs::write(&path, text) {
            Ok(_) => self.push_log(Level::Ok, format!("日志已导出: {}", path.display())),
            Err(e) => self.push_log(Level::Error, format!("导出失败: {e}")),
        }
    }

    fn draw_center(&mut self, root: &mut egui::Ui, p: Palette) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(20, 16)))
            .show(root, |ui| {
                if self.nav == Nav::Serial {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.draw_serial_center(ui, p);
                        });
                } else {
                    self.draw_hex_viewer(ui, p);
                }
            });
    }

    /// The session pane: a virtualised hex dump of the loaded firmware. All
    /// settings and actions live in the narrow "当前会话" sidebar.
    fn draw_hex_viewer(&mut self, ui: &mut Ui, p: Palette) {
        let mut picked: Option<PathBuf> = None;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("固件 16 进制查看器")
                    .size(21.0)
                    .strong()
                    .color(p.text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Self::ghost_button(ui, p, "浏览…", true).clicked() {
                    picked = with_all_files(rfd::FileDialog::new())
                        .add_filter("固件", &["hex", "ihex", "elf", "axf", "out", "bin"])
                        .pick_file();
                }
            });
        });
        if let Some(path) = picked {
            self.set_firmware(path);
        }
        ui.label(
            RichText::new("左侧「当前会话」完成连接与烧录；这里显示已载入固件的字节，可拖入文件直接查看")
                .size(11.0)
                .color(p.text3),
        );
        ui.add_space(10.0);

        let file_name = self
            .cfg
            .firmware
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "尚未载入固件".to_owned());
        let summary = self
            .firmware
            .as_ref()
            .map(|i| {
                format!(
                    "{} · {} 字节 · {} 条记录 · {}",
                    i.format.label(),
                    i.size,
                    i.records,
                    i.note
                )
            })
            .unwrap_or_default();
        let sha = self
            .firmware
            .as_ref()
            .map(|i| i.sha256.clone())
            .unwrap_or_default();
        let view = self.firmware_view.clone();
        let (bytes, base, label) = resolve_hex_view(view.as_ref(), self.hex_source);
        let rows = bytes.len().saturating_add(self.hex_columns - 1) / self.hex_columns;
        let capped = view.as_ref().is_some_and(|v| v.raw.len() >= HEX_VIEW_LIMIT);

        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(12.0)
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&file_name).size(12.0).strong().color(p.text));
                if !summary.is_empty() {
                    ui.label(RichText::new(&summary).size(10.0).color(p.text2));
                }
                if !sha.is_empty() {
                    ui.label(
                        RichText::new(format!("SHA256  {sha}"))
                            .monospace()
                            .size(9.5)
                            .color(p.text3),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("数据源").size(10.0).color(p.text3));
                    egui::ComboBox::from_id_salt("hex_source_combo")
                        .selected_text(self.hex_source.label())
                        .width(132.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.hex_source, HexSource::Image, "固件镜像");
                            ui.selectable_value(
                                &mut self.hex_source,
                                HexSource::File,
                                "文件原始字节",
                            );
                        });
                    ui.add_space(8.0);
                    ui.label(RichText::new("每行字节").size(10.0).color(p.text3));
                    egui::ComboBox::from_id_salt("hex_columns_combo")
                        .selected_text(self.hex_columns.to_string())
                        .width(58.0)
                        .show_ui(ui, |ui| {
                            for n in [8usize, 16, 32] {
                                ui.selectable_value(&mut self.hex_columns, n, n.to_string());
                            }
                        });
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.hex_ascii, "ASCII 栏");
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("{label} · 基址 0x{base:08X} · {rows} 行"))
                            .size(10.0)
                            .color(p.accent),
                    );
                });
            });
        ui.add_space(10.0);

        if bytes.is_empty() {
            Frame::new()
                .fill(p.card_alt)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(12.0)
                .inner_margin(20.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), 140.0));
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "拖拽 .hex / .elf / .bin 到窗口，或点击「浏览…」载入固件",
                        FontId::proportional(13.0),
                        p.text3,
                    );
                });
            return;
        }

        let cols = self.hex_columns;
        let ascii = self.hex_ascii;
        Frame::new()
            .fill(p.console)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Same widget structure as the rows, so the labels line up
                // with the offset / hex / ASCII columns for every row width.
                let hex_width = cols * 3 - 1 + usize::from(cols > 8);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{:<8}", "OFFSET"))
                            .monospace()
                            .size(11.0)
                            .color(p.text3),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("{:<hex_width$}", "HEX BYTES"))
                            .monospace()
                            .size(11.0)
                            .color(p.text3),
                    );
                    if ascii {
                        ui.add_space(10.0);
                        ui.label(RichText::new("ASCII").monospace().size(11.0).color(p.text3));
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, 19.0, rows, |ui, range| {
                        for row in range {
                            let (offset, hex, gutter) = format_hex_row(bytes, base, row, cols, ascii);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(offset).monospace().size(11.0).color(p.text3));
                                ui.add_space(10.0);
                                ui.label(RichText::new(hex).monospace().size(11.0).color(p.text));
                                if ascii {
                                    ui.add_space(10.0);
                                    ui.label(
                                        RichText::new(gutter)
                                            .monospace()
                                            .size(11.0)
                                            .color(p.text2),
                                    );
                                }
                            });
                        }
                    });
            });
        ui.add_space(6.0);
        ui.label(
            RichText::new(if capped {
                format!(
                    "{label} · 仅显示前 {} MiB（可在左侧更换固件）",
                    HEX_VIEW_LIMIT / 1024 / 1024
                )
            } else {
                format!("{label} · {} 字节 · 虚拟滚动，只渲染可见行", bytes.len())
            })
            .size(9.5)
            .color(p.text3),
        );
    }

    /// Fixed, resizable console dock below the work area (DSH "tool bench").
    fn draw_log_panel(&mut self, root: &mut egui::Ui, p: Palette) {
        if !self.log_panel_open {
            return;
        }
        egui::Panel::bottom("log_panel")
            .resizable(true)
            .default_size(190.0)
            .size_range(90.0..=420.0)
            .frame(Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("运行日志").size(12.0).strong().color(p.text));
                    ui.label(
                        RichText::new(format!("{} 条", self.logs.len()))
                            .size(10.0)
                            .color(p.text3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("收起").clicked() {
                            self.log_panel_open = false;
                        }
                        if ui.button("清空").clicked() {
                            self.logs.clear();
                        }
                        ui.checkbox(&mut self.show_debug, "调试信息");
                    });
                });
                ui.add_space(6.0);
                Frame::new()
                    .fill(p.console)
                    .corner_radius(8.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let available = ui.available_height();
                        egui::ScrollArea::vertical()
                            .max_height(available)
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let logs = self.logs.clone();
                                for entry in logs.iter() {
                                    if entry.level == Level::Debug && !self.show_debug {
                                        continue;
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(&entry.time)
                                                .monospace()
                                                .size(10.5)
                                                .color(p.text3),
                                        );
                                        ui.label(
                                            RichText::new(level_label(entry.level))
                                                .monospace()
                                                .size(10.5)
                                                .color(level_color(entry.level, p)),
                                        );
                                        ui.label(
                                            RichText::new(&entry.msg)
                                                .monospace()
                                                .size(10.5)
                                                .color(p.text2),
                                        );
                                    });
                                }
                            });
                    });
            });
    }

    /// The serial page replaces the old duplicated probe list: the centre
    /// panel shows port details instead of the workbench's flashing form.
    fn draw_serial_center(&mut self, ui: &mut Ui, p: Palette) {
        ui.label(RichText::new("串口识别").size(22.0).strong().color(p.text));
        ui.label(
            RichText::new("识别当前连接的串口设备，自动标注 DAPLink / CMSIS-DAP 自带的虚拟串口")
                .size(12.0)
                .color(p.text3),
        );
        ui.add_space(16.0);

        let total = self.ports.len();
        let vcp = self.ports.iter().filter(|x| x.is_debug_vcp()).count();
        let busy = self.ports.iter().filter(|x| !x.openable).count();
        Self::card(ui, p, "连接概览", |ui| {
            egui::Grid::new("serial_summary_grid")
                .num_columns(3)
                .spacing([36.0, 8.0])
                .show(ui, |ui| {
                    for (label, value, color) in [
                        ("在线串口", total.to_string(), p.text),
                        ("调试器虚拟串口", vcp.to_string(), p.accent),
                        (
                            "被占用 / 不可用",
                            busy.to_string(),
                            if busy > 0 { p.warn } else { p.success },
                        ),
                    ] {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(label).size(10.0).color(p.text3));
                            ui.label(RichText::new(value).size(20.0).strong().color(color));
                        });
                    }
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "每 2 秒自动检测一次 · 插入 / 拔出调试器时，其虚拟串口会与调试接口一起出现或消失。",
                )
                .size(10.0)
                .color(p.text3),
            );
        });
        ui.add_space(14.0);

        if self.ports.is_empty() {
            Self::card(ui, p, "端口列表", |ui| {
                ui.label(
                    RichText::new(
                        "当前没有检测到串口设备。插入 USB 转串口或带虚拟串口的调试器后，会自动出现在这里。",
                    )
                    .size(12.0)
                    .color(p.text3),
                );
            });
            return;
        }

        let ports = self.ports.clone();
        let selected = self.selected_port.clone();
        let probe_id = self
            .cfg
            .probe_id
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut picked: Option<String> = None;
        for port in &ports {
            let active = selected.as_deref() == Some(port.name.as_str());
            let same_probe = port
                .vid_pid()
                .is_some_and(|id| probe_id.starts_with(&id));
            let vid_pid = port.vid_pid().unwrap_or_else(|| "—".to_owned());
            let iface = port
                .interface
                .map(|i| format!("MI_{i:02}"))
                .unwrap_or_else(|| "—".to_owned());
            let instance = if port.instance_id.is_empty() {
                "—".to_owned()
            } else {
                port.instance_id.clone()
            };
            let link = if same_probe {
                "是 · 与当前调试探针同一 USB 设备".to_owned()
            } else {
                "否".to_owned()
            };
            let rows = [
                ("设备描述", port.description.clone()),
                ("设备对象", port.device_object.clone()),
                ("VID:PID", vid_pid),
                ("USB 接口", iface),
                ("设备实例 ID", instance),
                ("关联当前探针", link),
            ];
            Frame::new()
                .fill(if active { p.accent_soft } else { p.card })
                .stroke(Stroke::new(1.0, if active { p.accent } else { p.border }))
                .corner_radius(12.0)
                .inner_margin(16.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(port.name.as_str())
                                .size(18.0)
                                .strong()
                                .color(p.text),
                        );
                        ui.label(
                            RichText::new(format!("· {}", port.kind.label()))
                                .size(11.0)
                                .color(if port.is_debug_vcp() { p.accent } else { p.text2 }),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(if port.openable {
                                        "● 可打开"
                                    } else {
                                        "● 被占用 / 不可用"
                                    })
                                    .size(10.0)
                                    .color(if port.openable { p.success } else { p.warn }),
                                );
                            },
                        );
                    });
                    ui.add_space(8.0);
                    egui::Grid::new(port.name.as_str())
                        .num_columns(2)
                        .spacing([16.0, 5.0])
                        .show(ui, |ui| {
                            for (key, value) in &rows {
                                ui.label(RichText::new(*key).size(10.0).color(p.text3));
                                ui.label(
                                    RichText::new(value.as_str()).size(10.0).color(p.text2),
                                );
                                ui.end_row();
                            }
                        });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(if active { "已选中" } else { "选中" }).clicked() {
                            picked = Some(port.name.clone());
                        }
                        if ui.button("复制 COM 号").clicked() {
                            ui.ctx().copy_text(port.name.clone());
                            self.push_log(Level::Info, format!("已复制 {}", port.name));
                        }
                    });
                });
            ui.add_space(10.0);
        }
        if let Some(name) = picked {
            self.selected_port = Some(name);
        }
    }

    fn draw_status(&mut self, root: &mut egui::Ui, p: Palette) {
        egui::Panel::bottom("status_bar")
            .exact_size(38.0)
            .frame(Frame::new().fill(p.sidebar).inner_margin(egui::Margin::symmetric(16, 6)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                    let color = if self.busy {
                        p.warn
                    } else if self.connection.is_some() {
                        p.success
                    } else {
                        p.text3
                    };
                    ui.painter().circle_filled(rect.center(), 4.0, color);
                    ui.add_space(4.0);
                    let state = if self.busy {
                        self.busy_label.clone()
                    } else if let Some((ok, ref msg, t)) = self.last_result {
                        format!("{} · {:.2}s", if ok { "成功" } else { "失败" }, t).to_string() + "" + &msg.chars().take(60).collect::<String>()
                    } else {
                        "就绪".to_owned()
                    };
                    ui.label(RichText::new(state).size(11.0).color(p.text2));
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!(
                            "{} · {} · {} kHz",
                            self.cfg.chip,
                            proto_label(self.cfg.protocol),
                            self.cfg.speed_khz
                        ))
                        .size(11.0)
                        .color(p.text3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_sized(
                            [220.0, 14.0],
                            egui::ProgressBar::new(self.progress)
                                .desired_width(220.0)
                                .show_percentage(),
                        );
                        ui.label(
                            RichText::new(&self.progress_label).size(10.0).color(p.text3),
                        );
                    });
                });
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.37 hands the root `Ui` to the app instead of a `Context`.
        // Keep a clone of the context for input, repaint and global style calls.
        let ctx = root.ctx().clone();
        self.pump();
        // Keep the watcher in sync with UI state: never poll USB while a job
        // owns the debug probe, and honour the settings toggle.
        self.watch.enabled.store(self.auto_detect, Ordering::Relaxed);
        self.watch.busy.store(self.busy, Ordering::Relaxed);
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        let p = theme::palette(self.theme);

        // File drag & drop (eframe feeds dropped paths through egui input).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            // One drop target for both: chip-support sources are routed to the
            // shared importer, everything else is treated as firmware.
            if pack::classify_source(&path).is_some() {
                self.load_chip_source(path);
            } else {
                self.set_firmware(path);
            }
        }

        self.draw_top(root, p);
        // The top bar (or the settings sidebar) may have toggled the theme
        // mid-frame. Refresh the palette so the rail and every other panel are
        // painted with the new theme immediately, not one frame later.
        let p = theme::palette(self.theme);
        self.draw_status(root, p);
        self.draw_log_panel(root, p);
        self.draw_rail(root, p);
        self.draw_sidebar(root, p);
        self.draw_center(root, p);
        self.draw_flm_dialog(&ctx, p);
        self.persist();

        // Keep egui's global Visuals (checkboxes, selectable labels, scroll
        // bars...) in sync with our custom palette, whatever the toggle source.
        if self.applied_theme != self.theme {
            theme::apply(&ctx, self.theme);
            self.applied_theme = self.theme;
            ctx.request_repaint();
        }
    }
}

fn proto_label(p: Protocol) -> &'static str {
    match p {
        Protocol::Swd => "SWD",
        Protocol::Jtag => "JTAG",
    }
}

fn level_label(l: Level) -> &'static str {
    match l {
        Level::Info => "INFO ",
        Level::Ok => "OK   ",
        Level::Warn => "WARN ",
        Level::Error => "ERROR",
        Level::Debug => "DEBUG",
    }
}

fn level_color(l: Level, p: Palette) -> Color32 {
    match l {
        Level::Info => p.accent,
        Level::Ok => p.success,
        Level::Warn => p.warn,
        Level::Error => p.error,
        Level::Debug => p.text3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old "调试探针" page duplicated the workbench probe selector; the
    /// rail now opens a dedicated serial-port page with its own centre panel.
    #[test]
    fn probe_page_was_replaced_by_a_serial_page() {
        let source = include_str!("app.rs");
        assert!(Nav::Serial.title().contains("串口"));
        // Split literals so the guard does not match its own source line.
        assert!(!source.contains(concat!("Nav::", "Devices")));
        assert!(!source.contains(concat!("sidebar_", "devices")));
        assert!(source.contains("self.draw_serial_center(ui, p)"));
        assert!(source.contains("Event::SerialPorts(items)"));
        assert!(source.contains("spawn_serial_watcher"));
        // Probe selection still belongs to the workbench.
        assert!(source.contains("Job::RefreshProbes"));
    }

    /// Regression guard: the OS title bar and the in-app top bar used to both
    /// print "ProbeRS Studio · 固件烧录工作台", producing a duplicate heading.
    #[test]
    fn top_bar_never_repeats_the_window_title() {
        let pages = [
            Nav::Workbench,
            Nav::Serial,
            Nav::Chips,
            Nav::Logs,
            Nav::Settings,
        ];
        let mut unique = std::collections::HashSet::new();
        for nav in pages {
            let title = nav.title();
            assert!(!title.is_empty(), "every page needs a top-bar label");
            assert!(
                !crate::WINDOW_TITLE.contains(title) && !title.contains(crate::WINDOW_TITLE),
                "top bar label {title:?} duplicates window title {:?}",
                crate::WINDOW_TITLE
            );
            assert!(!title.contains("ProbeRS"), "brand belongs to the OS title bar");
            assert!(unique.insert(title), "duplicate page label {title:?}");
        }
        assert_eq!(unique.len(), pages.len());
    }

    /// The .FLM dialog must never hand an invalid spec to the backend.
    #[test]
    fn flm_form_validates_user_input() {
        let mut form = FlmForm::from_file(
            PathBuf::from("C:/algos/MYCHIP_256.FLM"),
            &pack::FlmInfo {
                description: "MYCHIP 256K".to_owned(),
                flash_base: 0x0800_0000,
                flash_size: 0x4_0000,
                page_size: 0x400,
                erased: 0xff,
                program_timeout: 100,
                erase_timeout: 500,
                sectors: vec![(0x800, 0)],
                blob_size: 364,
                entry_points: vec![("pc_init", 0x1d)],
                data_section_offset: 0x168,
                code_load_address: 0x2000_0000,
            },
        );
        assert_eq!(form.chips, "MYCHIP_256");
        assert_eq!(form.family, "MYCHIP_256 Series");
        assert_eq!(form.flash_base, "0x8000000");
        assert!(form.info.contains("256 KiB"));

        form.chips = "MYCHIP_A，MYCHIP_B; MYCHIP_C".to_owned();
        form.load_address = String::new();
        let spec = form.to_spec().unwrap();
        assert_eq!(spec.chips, vec!["MYCHIP_A", "MYCHIP_B", "MYCHIP_C"]);
        assert_eq!(spec.flash_base, Some(0x0800_0000));
        assert_eq!(spec.flash_size, Some(0x4_0000));
        assert_eq!(spec.ram_base, 0x2000_0000);
        assert_eq!(spec.ram_size, 0x1_0000);
        assert_eq!(spec.load_address, None);
        assert_eq!(spec.core, "armv7em");

        form.flash_base = "留空示例".to_owned();
        assert!(form.to_spec().is_err(), "non-numeric address");
        form.flash_base = "0x08000000".to_owned();
        form.chips = " , ".to_owned();
        assert!(form.to_spec().is_err(), "no chip name");
        form.chips = "MYCHIP_A".to_owned();
        form.ap = "300".to_owned();
        assert!(form.to_spec().is_err(), "AP out of range");
        form.ap = "0".to_owned();
        form.load_address = "0x20000400".to_owned();
        assert_eq!(form.to_spec().unwrap().load_address, Some(0x2000_0400));
    }

    /// Regression guard: pack and FLM used to be two separate buttons. A pack
    /// is only a container around FLM algorithms, so both pages must keep
    /// exactly one shared "load chip support" entry.
    #[test]
    fn chip_library_and_settings_share_one_load_button() {
        let source = include_str!("app.rs");
        let call = concat!("self.pick_chip_", "source()");
        assert_eq!(source.matches(call).count(), 2);
        // concat! keeps the legacy labels out of this test's own source text,
        // otherwise include_str! would see them and the guard would be useless.
        for old in [
            concat!("加载 CMSIS-", "Pack…"),
            concat!("加载 .FLM ", "算法…"),
            concat!("加载 .", "pack…"),
            concat!("加载 .FLM", "…"),
            concat!("fn pick_", "pack("),
            concat!("fn pick_", "flm("),
        ] {
            assert!(!source.contains(old), "old split entry point still present: {old}");
        }
        assert_eq!(CHIP_SOURCE_BUTTON, "加载芯片支持…");
        assert!(source.contains("const CHIP_SOURCE_BUTTON"));
    }

    /// Every file picker must keep the "all files" filter, otherwise a vendor
    /// file with an unusual extension is invisible in the open dialog.
    #[test]
    fn every_file_picker_offers_all_files() {
        let source = include_str!("app.rs");
        let helper = concat!("with_all_", "files(");
        // One helper definition + chip support, chip description, sidebar
        // firmware picker, log export, backup path and the hex viewer's
        // browse button = 7 occurrences.
        assert_eq!(source.matches(helper).count(), 7);
        let open_first = concat!("with_all_", "files(rfd::FileDialog::new())");
        assert_eq!(source.matches(open_first).count(), 4);
        assert!(source.contains(concat!("全部文件 (*.", "*)")));
    }

    #[test]
    fn recent_chips_are_deduplicated_and_bounded() {
        let mut h = vec![];
        for name in [
            "PY32F003x8",
            "STM32F103C8",
            "py32f003X8",
            "  ",
            "HC32F401BRCX",
        ] {
            push_recent_chip(&mut h, name);
        }
        assert_eq!(h, vec!["HC32F401BRCX", "py32f003X8", "STM32F103C8"]);
        for i in 1..=12 {
            push_recent_chip(&mut h, &format!("CHIP{i}"));
        }
        assert_eq!(h.len(), RECENT_CHIPS_LIMIT);
        assert_eq!(h[0], "CHIP12");
        assert_eq!(h[7], "CHIP5");
    }

    #[test]
    fn recent_chip_search_shortcuts_and_ranking() {
        let history = vec![
            "PY32F003x8".to_owned(),
            "STM32F103C8".to_owned(),
            "HC32F401BRCX".to_owned(),
        ];
        assert_eq!(matched_recent_chips(&history, ""), history);
        assert_eq!(
            matched_recent_chips(&history, " stm "),
            vec!["STM32F103C8".to_owned()]
        );
        assert!(matched_recent_chips(&history, "nope").is_empty());

        // Stable sort lifts the history to the front in recency order and
        // leaves the remaining registry order untouched.
        let mut names = vec!["AAA", "PY32F003x8", "BBB", "stm32f103c8", "CCC"];
        names.sort_by_key(|n| recent_chip_rank(&history, n));
        assert_eq!(
            names,
            vec!["PY32F003x8", "stm32f103c8", "AAA", "BBB", "CCC"]
        );
    }

    /// Settings written before this feature have no `recent_chips` key.
    #[test]
    fn older_settings_without_recent_chips_still_load() {
        let s = Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            recent_chips: vec!["PY32F003x8".to_owned()],
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        };
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("recent_chips");
        let mut old: Settings = serde_json::from_value(value).unwrap();
        assert!(old.recent_chips.is_empty());
        assert_eq!(old.config.chip, s.config.chip);
        // App::new seeds the empty history with the model already in use.
        let chip = old.config.chip.clone();
        seed_recent_chips(&mut old.recent_chips, &chip);
        assert_eq!(old.recent_chips, vec![old.config.chip.clone()]);
    }

    #[test]
    fn history_seed_never_overwrites_real_history() {
        let mut h = vec![];
        seed_recent_chips(&mut h, "PY32F003x8");
        assert_eq!(h, vec!["PY32F003x8"]);
        seed_recent_chips(&mut h, "STM32F103C8");
        assert_eq!(h, vec!["PY32F003x8"]);
        let mut blank = vec![];
        seed_recent_chips(&mut blank, "   ");
        assert!(blank.is_empty());
    }

    /// Config + firmware + actions live in the narrow "当前会话" rail; the
    /// centre pane is now a hex viewer instead of three oversized cards.
    #[test]
    fn workbench_controls_live_in_the_session_sidebar() {
        let source = include_str!("app.rs");
        for section in [
            concat!("fn session_", "connection"),
            concat!("fn session_", "firmware"),
            concat!("fn session_", "actions"),
            concat!("fn draw_hex_", "viewer"),
        ] {
            assert!(source.contains(section), "missing {section}");
        }
        for old in [
            concat!("Self::card(ui, p, \"连接", "配置\""),
            concat!("Self::card(ui, p, \"烧录选项", "与操作\""),
            concat!("RichText::new(\"连接与", "烧录\")"),
        ] {
            assert!(!source.contains(old), "old centre card returned: {old}");
        }
        assert!(source.contains(concat!("self.draw_hex_", "viewer(ui, p)")));
    }

    #[test]
    fn hex_dump_rows_are_padded_and_addressable() {
        let data = b"HELLO\x00WORLD\xff\x01!";
        let (off, hex, gutter) = format_hex_row(data, 0x0800_0000, 0, 8, true);
        assert_eq!(off, "08000000");
        assert_eq!(hex, "48 45 4C 4C 4F 00 57 4F");
        assert_eq!(gutter, "HELLO.WO");
        let (off2, hex2, gutter2) = format_hex_row(data, 0x0800_0000, 1, 8, true);
        assert_eq!(off2, "08000008");
        assert!(hex2.starts_with("52 4C 44 FF 01 21"));
        assert_eq!(hex2.len(), 8 * 3 - 1, "short rows keep fixed width");
        assert_eq!(gutter2, "RLD..!");
        let (_, _, no_ascii) = format_hex_row(data, 0, 0, 8, false);
        assert!(no_ascii.is_empty());
    }

    #[test]
    fn hex_view_prefers_the_decoded_image() {
        let view = FirmwareView {
            raw: Arc::from(b":10000000".as_slice()),
            image: Some(Arc::from(b"\x01\x02".as_slice())),
            image_base: 0x0800_0000,
        };
        let (bytes, base, label) = resolve_hex_view(Some(&view), HexSource::Image);
        assert_eq!(bytes, &[1, 2]);
        assert_eq!(base, 0x0800_0000);
        assert_eq!(label, "固件镜像");
        let (raw, raw_base, raw_label) = resolve_hex_view(Some(&view), HexSource::File);
        assert_eq!(raw, b":10000000");
        assert_eq!(raw_base, 0);
        assert_eq!(raw_label, "文件原始字节");
        // ELF / malformed HEX / unknown extension: fall back to raw bytes.
        let no_image = FirmwareView {
            raw: Arc::from(b"\x7fELF".as_slice()),
            image: None,
            image_base: 0,
        };
        assert_eq!(
            resolve_hex_view(Some(&no_image), HexSource::Image).2,
            "文件原始字节"
        );
        assert!(resolve_hex_view(None, HexSource::Image).0.is_empty());
    }
}
