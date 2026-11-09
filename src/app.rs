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
    time::{Duration, Instant},
};

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use crate::{
    backend::{
        self, ChipItem, Event, FlashConfig, Job, Level, ProbeItem, Protocol, SerialCommand,
        WatcherControl,
    },
    chip_id::{self, ChipIdSnapshot},
    firmware::{self, FirmwareFormat, FirmwareInfo},
    icons::{IconKind, IconSet},
    option_bytes::{self, ObImage, ObSnapshot},
    pack::{self, FlmSpec},
    serial::{self, SerialPort, SerialSettings},
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

fn default_timer_ms() -> u32 {
    SERIAL_TIMER_DEFAULT_MS
}

fn default_rx_font() -> f32 {
    SERIAL_FONT_DEFAULT_RX
}

fn default_tx_font() -> f32 {
    SERIAL_FONT_DEFAULT_TX
}

/// Guard against accidentally dropping a huge file onto the window. Firmware
/// images are small; the viewer only keeps this many bytes in memory.
const HEX_VIEW_LIMIT: usize = 32 * 1024 * 1024;

/// Rolling buffer for the serial terminal's receive pane.
const SERIAL_RX_LIMIT: usize = 64 * 1024;

/// Default period for the "定时发送" checkbox (milliseconds).
const SERIAL_TIMER_DEFAULT_MS: u32 = 1000;
/// Bounds for a periodic send: below 100 ms the egui repaint loop cannot keep
/// up, above one hour is almost certainly a typo.
const SERIAL_TIMER_MIN_MS: u64 = 100;
const SERIAL_TIMER_MAX_MS: u64 = 3_600_000;

/// Font sizes for the serial RX / TX panes, adjustable with Ctrl + wheel.
const SERIAL_FONT_DEFAULT_RX: f32 = 11.5;
const SERIAL_FONT_DEFAULT_TX: f32 = 13.0;
const SERIAL_FONT_MIN: f32 = 8.0;
const SERIAL_FONT_MAX: f32 = 32.0;

/// Quick-insert palette for the serial send box. Emoji are handy for testing
/// UTF-8 payloads; the trailing commands are the usual bring-up strings for
/// modules and modems. Everything is inserted as real characters, never as
/// escape text, so the bytes on the wire are exactly what is shown.
const QUICK_EMOJI: &[&str] = &[
    "😀", "😂", "😍", "🤔", "😎", "😭", "👍", "👎", "👏", "🙏", "💪", "✌", "❤", "🔥", "✅", "❌",
    "⚠", "⭐", "🎉", "💡", "🚀", "📡", "🔌", "🔧", "🐛", "⏰", "📦", "🎯",
];
const QUICK_COMMANDS: &[(&str, &str)] = &[
    ("AT", "AT"),
    ("AT+GMR", "AT+GMR"),
    ("AT+RST", "AT+RST"),
    ("CRLF", "\r\n"),
    ("00 FF", "00 FF"),
];

/// Insert `insert` at a character-index caret (NOT a byte offset) and move
/// the caret past the inserted characters. Kept pure so the UTF-8 / emoji
/// behaviour is unit-testable without constructing an egui app.
fn insert_at_caret(text: &mut String, caret: &mut usize, insert: &str) {
    let mut chars: Vec<char> = text.chars().collect();
    let mut at = (*caret).min(chars.len());
    for c in insert.chars() {
        chars.insert(at, c);
        at += 1;
    }
    *text = chars.into_iter().collect();
    *caret = at;
}

/// egui's `zoom_delta()` is 1.0 for "no change", >1 for ctrl+wheel up or a
/// pinch-out gesture. Map one wheel notch to roughly one font point.
fn font_zoom_delta(factor: f32) -> f32 {
    if !factor.is_finite() || factor <= 0.0 {
        return 0.0;
    }
    // log2 makes zoom-in / zoom-out symmetric: 1.1 and 1/1.1 cancel out.
    factor.clamp(0.5, 2.0).log2() * 8.0
}

/// Whether a periodic-send tick is due. `Instant::duration_since` saturates
/// at zero, so a clock that jumps backwards cannot panic here.
fn timer_due(last: Option<Instant>, now: Instant, period: Duration) -> bool {
    !last.is_some_and(|t| now.duration_since(t) < period)
}

fn clamp_font_size(size: f32, delta: f32) -> f32 {
    if !size.is_finite() {
        return SERIAL_FONT_DEFAULT_RX;
    }
    (size + delta).clamp(SERIAL_FONT_MIN, SERIAL_FONT_MAX)
}

/// Validated periodic-send period in milliseconds.
fn parse_timer_ms(text: &str) -> Option<u64> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|v| (SERIAL_TIMER_MIN_MS..=SERIAL_TIMER_MAX_MS).contains(v))
}

fn append_rx(buffer: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    buffer.extend_from_slice(bytes);
    if buffer.len() > limit {
        let drop = buffer.len() - limit;
        buffer.drain(..drop);
    }
}

/// Short log-only preview of an outgoing payload.
fn preview_bytes(bytes: &[u8]) -> String {
    let n = bytes.len().min(24);
    let head: String = bytes[..n].iter().map(|b| format!("{b:02X} ")).collect();
    if bytes.len() > n {
        format!("{head}…")
    } else {
        head
    }
}

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
    /// Serial-terminal settings (baud / frame / flow control). `default`
    /// keeps settings files written by older versions readable.
    #[serde(default)]
    serial: SerialSettings,
    /// Periodic-send period and quick-input panel visibility. The *enabled*
    /// flag is deliberately not persisted, so the app never starts sending
    /// on its own after a restart.
    #[serde(default = "default_timer_ms")]
    serial_timer_ms: u32,
    #[serde(default)]
    serial_emoji_open: bool,
    /// Per-pane font sizes (Ctrl + wheel to zoom).
    #[serde(default = "default_rx_font")]
    serial_rx_font_size: f32,
    #[serde(default = "default_tx_font")]
    serial_tx_font_size: f32,
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

/// Deferred actions from the option-byte dialog; applied after the egui
/// closure releases its borrow of `self`.
enum ObAction {
    None,
    Read(u64),
    Write(ObImage, u64),
    Reset,
    Copy(String),
}

/// Actions collected inside the chip-ID window closure, folded back into the
/// app after `Window::show()` so `self` is never borrowed from inside egui.
enum ChipIdAction {
    None,
    Read(Option<u64>),
    Copy(String),
    Clear,
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
    /// Terminal configuration, persisted with the rest of the session.
    serial_cfg: SerialSettings,
    serial_baud_text: String,
    /// Command channel of the running serial-session worker thread.
    serial_cmd: Option<Sender<SerialCommand>>,
    serial_open_name: Option<String>,
    serial_open: bool,
    serial_error: Option<String>,
    serial_rx: Vec<u8>,
    serial_tx_text: String,
    serial_rx_hex: bool,
    serial_tx_hex: bool,
    serial_auto_scroll: bool,
    serial_append_nl: bool,
    serial_rx_bytes: usize,
    serial_tx_bytes: usize,
    /// Periodic send ("定时发送"): period text, effective period, last tick,
    /// number of payloads emitted and whether the loop is armed.
    serial_timer_on: bool,
    serial_timer_ms: u32,
    serial_timer_text: String,
    serial_last_tick: Option<Instant>,
    serial_timer_sent: usize,
    /// Quick-input panel toggle and the last caret position (a character
    /// index, not a byte offset) of the send box.
    serial_emoji_open: bool,
    serial_tx_cursor: usize,
    serial_rx_font_size: f32,
    serial_tx_font_size: f32,
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
    /// Option-byte editor. Deliberately NOT persisted: a restart must never
    /// re-arm a pending OB write or silently restore an edited image.
    ob_open: bool,
    ob_snapshot: Option<ObSnapshot>,
    ob_edit: Option<ObImage>,
    ob_raw_hex: String,
    ob_raw_error: Option<String>,
    ob_base_text: String,
    ob_acknowledge: bool,
    ob_armed: bool,
    /// Read-only chip identification window. Never persisted: a snapshot from
    /// a previous board must not be mistaken for the currently attached one.
    chip_id_open: bool,
    chip_id_snapshot: Option<ChipIdSnapshot>,
    chip_id_custom: String,
    chip_id_custom_error: Option<String>,
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
            serial: SerialSettings::default(),
            serial_timer_ms: SERIAL_TIMER_DEFAULT_MS,
            serial_emoji_open: false,
            serial_rx_font_size: SERIAL_FONT_DEFAULT_RX,
            serial_tx_font_size: SERIAL_FONT_DEFAULT_TX,
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
            serial_cfg: s.serial,
            serial_baud_text: s.serial.baud.to_string(),
            serial_cmd: None,
            serial_open_name: None,
            serial_open: false,
            serial_error: None,
            serial_rx: vec![],
            serial_tx_text: String::new(),
            serial_rx_hex: false,
            serial_tx_hex: false,
            serial_auto_scroll: true,
            serial_append_nl: true,
            serial_rx_bytes: 0,
            serial_tx_bytes: 0,
            serial_timer_on: false,
            serial_timer_ms: s.serial_timer_ms.clamp(
                SERIAL_TIMER_MIN_MS as u32,
                SERIAL_TIMER_MAX_MS as u32,
            ),
            serial_timer_text: s
                .serial_timer_ms
                .clamp(SERIAL_TIMER_MIN_MS as u32, SERIAL_TIMER_MAX_MS as u32)
                .to_string(),
            serial_last_tick: None,
            serial_timer_sent: 0,
            serial_emoji_open: s.serial_emoji_open,
            serial_tx_cursor: 0,
            serial_rx_font_size: clamp_font_size(s.serial_rx_font_size, 0.0),
            serial_tx_font_size: clamp_font_size(s.serial_tx_font_size, 0.0),
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
            // `--ob-open` starts straight into the option-byte editor; handy
            // for quickly inspecting a freshly connected board.
            ob_open: std::env::args().any(|a| a == "--ob-open"),
            ob_snapshot: None,
            ob_edit: None,
            ob_raw_hex: String::new(),
            ob_raw_error: None,
            ob_base_text: format!(
                "0x{:08X}",
                option_bytes::profile_for_chip(&seeded_chip).base
            ),
            ob_acknowledge: false,
            ob_armed: false,
            // `--id-open` starts straight into the chip-ID report; handy when
            // checking a freshly connected board.
            chip_id_open: std::env::args().any(|a| a == "--id-open"),
            chip_id_snapshot: None,
            chip_id_custom: String::new(),
            chip_id_custom_error: None,
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

    /// Returns true when at least one background event was consumed, so the
    /// UI can schedule another repaint (egui is otherwise idle when nothing
    /// moves the mouse, which would freeze a live serial terminal).
    fn pump(&mut self) -> bool {
        let mut received = false;
        while let Ok(ev) = self.rx.try_recv() {
            received = true;
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
                    if self.serial_open
                        && self
                            .serial_open_name
                            .as_deref()
                            .is_some_and(|name| !self.ports.iter().any(|p| p.name == name))
                    {
                        self.push_log(Level::Warn, "串口设备已拔出，会话自动关闭");
                        self.close_serial();
                    }
                }
                Event::SerialRx(bytes) => {
                    self.serial_rx_bytes = self.serial_rx_bytes.saturating_add(bytes.len());
                    append_rx(&mut self.serial_rx, &bytes, SERIAL_RX_LIMIT);
                }
                Event::SerialTxCount(n) => {
                    self.serial_tx_bytes = self.serial_tx_bytes.saturating_add(n);
                }
                Event::SerialClosed(reason) => {
                    let was_open = self.serial_open;
                    self.serial_open = false;
                    self.serial_open_name = None;
                    self.serial_cmd = None;
                    self.serial_timer_on = false;
                    self.serial_last_tick = None;
                    if !reason.is_empty() {
                        self.serial_error = Some(reason.clone());
                        if was_open {
                            self.push_log(Level::Error, format!("串口会话结束: {reason}"));
                        }
                    }
                }
                Event::OptionBytes(snapshot) => self.on_option_bytes(snapshot),
                Event::ChipId(snapshot) => {
                    self.chip_id_snapshot = Some(snapshot);
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
        received
    }

    /// Replace the probe list while keeping the *same physical probe* selected.
    /// Returns whether that probe is still attached.
    fn apply_probes(&mut self, items: Vec<ProbeItem>) -> bool {
        let chosen = self.cfg.probe_id.clone();
        let mut items = items;
        // Network probes cannot be found by USB polling, so keep the configured
        // one in the dropdown on every refresh.
        if let Some(entry) = backend::network_probe_entry(&self.cfg.network_probe) {
            if !items.iter().any(|x| x.id == entry.id) {
                items.push(entry);
            }
        }
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
            serial: self.serial_cfg,
            serial_timer_ms: self
                .serial_timer_text
                .trim()
                .parse::<u32>()
                .unwrap_or(self.serial_timer_ms),
            serial_emoji_open: self.serial_emoji_open,
            serial_rx_font_size: self.serial_rx_font_size,
            serial_tx_font_size: self.serial_tx_font_size,
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
                | Job::ReadChipId { .. }
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

    /// Anything worth offering a "close" button for. A file whose parsing
    /// failed still owns a raw-byte preview, so it must be removable too.
    fn has_firmware(&self) -> bool {
        self.cfg.firmware.is_some() || self.firmware.is_some() || self.firmware_view.is_some()
    }

    /// Unload the current firmware. Taking `cfg.firmware` also stops the app
    /// from silently restoring that file on the next launch. The recent-file
    /// history is deliberately kept so the user can reload it with one click.
    fn clear_firmware(&mut self) {
        if !self.has_firmware() {
            return;
        }
        let removed = self.cfg.firmware.take();
        self.firmware = None;
        self.firmware_view = None;
        self.hex_source = HexSource::Image;
        match removed {
            Some(path) => self.push_log(
                Level::Info,
                format!("已移除固件: {}（16 进制查看器已清空）", path.display()),
            ),
            None => self.push_log(Level::Info, "已清空固件预览"),
        }
    }

    fn open_serial(&mut self) {
        if self.serial_open {
            return;
        }
        let Some(name) = self.selected_port.clone() else {
            self.push_log(Level::Warn, "请先在左侧选择一个串口");
            return;
        };
        let baud = match self.serial_baud_text.trim().parse::<u32>() {
            Ok(v) if v > 0 => v,
            _ => {
                self.serial_error = Some("波特率必须是正整数".to_owned());
                self.push_log(Level::Error, "波特率无效，请填写如 115200");
                return;
            }
        };
        let mut cfg = self.serial_cfg;
        cfg.baud = baud;
        if let Err(err) = cfg.validate() {
            self.serial_error = Some(err.clone());
            self.push_log(Level::Error, format!("串口参数无效: {err}"));
            return;
        }
        self.serial_cfg = cfg;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        backend::spawn_serial_session(name.clone(), cfg, self.tx.clone(), cmd_rx);
        self.serial_cmd = Some(cmd_tx);
        self.serial_open = true;
        self.serial_open_name = Some(name.clone());
        self.serial_error = None;
        self.serial_last_tick = None;
        self.serial_timer_sent = 0;
        self.push_log(Level::Info, format!("正在打开 {name} · {}", cfg.label()));
    }

    fn close_serial(&mut self) {
        if let Some(tx) = self.serial_cmd.take() {
            let _ = tx.send(SerialCommand::Close);
        }
        self.serial_open = false;
        self.serial_open_name = None;
        // Never leave a periodic sender armed against a closed port.
        self.serial_timer_on = false;
        self.serial_last_tick = None;
    }

    /// Periodic send tick, driven by the egui frame loop. `ui()` already
    /// requests a 50 ms repaint while a serial session is alive, so no extra
    /// thread is needed. Returns true when a payload was actually queued.
    fn tick_serial_timer(&mut self) -> bool {
        if !self.serial_timer_on || !self.serial_open {
            return false;
        }
        let Some(ms) = parse_timer_ms(&self.serial_timer_text) else {
            return false;
        };
        let period = Duration::from_millis(ms);
        let now = Instant::now();
        if !timer_due(self.serial_last_tick, now, period) {
            return false;
        }
        self.serial_last_tick = Some(now);
        if self.serial_tx_text.trim().is_empty() {
            return false;
        }
        self.send_serial(true);
        self.serial_timer_sent = self.serial_timer_sent.saturating_add(1);
        true
    }

    /// Insert a quick-input entry at the caret (Rust `char` index, not bytes).
    fn insert_quick_text(&mut self, text: &str) {
        insert_at_caret(
            &mut self.serial_tx_text,
            &mut self.serial_tx_cursor,
            text,
        );
    }

    /// `quiet` suppresses the per-payload DEBUG line for periodic sends;
    /// the RX/TX counters still update through the worker's events.
    fn send_serial(&mut self, quiet: bool) {
        if !self.serial_open {
            if !quiet {
                self.push_log(Level::Warn, "请先打开串口再发送数据");
            }
            return;
        }
        let text = self.serial_tx_text.clone();
        let mut payload = if self.serial_tx_hex {
            match serial::parse_hex_text(&text) {
                Ok(bytes) => bytes,
                Err(err) => {
                    self.serial_error = Some(err.clone());
                    if !quiet {
                        self.push_log(Level::Error, format!("HEX 发送内容无效: {err}"));
                    }
                    return;
                }
            }
        } else {
            text.into_bytes()
        };
        if self.serial_append_nl && !self.serial_tx_hex {
            payload.extend_from_slice(b"\r\n");
        }
        if payload.is_empty() {
            return;
        }
        match &self.serial_cmd {
            Some(tx) => match tx.send(SerialCommand::Send(payload.clone())) {
                Ok(()) => {
                    self.serial_error = None;
                    if !quiet {
                        self.push_log(
                            Level::Debug,
                            format!("TX {} 字节: {}", payload.len(), preview_bytes(&payload)),
                        );
                    }
                }
                Err(_) => {
                    self.serial_timer_on = false;
                    self.push_log(Level::Error, "串口会话已断开，发送失败");
                }
            },
            None => self.push_log(Level::Error, "串口会话不存在，发送失败"),
        }
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
                        if Self::ghost_button(ui, p, "芯片 ID", true).clicked() {
                            self.chip_id_open = true;
                        }
                        if Self::ghost_button(ui, p, "选项字节", true).clicked() {
                            self.ob_open = true;
                            if self.ob_snapshot.is_none() {
                                let base = option_bytes::profile_for_chip(&self.cfg.chip).base;
                                self.ob_base_text = format!("0x{base:08X}");
                            }
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
        // Flashing is the single most used action. Pin it above the scroll
        // area so it stays reachable no matter how far the settings are
        // scrolled; there is deliberately no duplicate button further down.
        let can_flash = self.can_run() && self.firmware.is_some();
        let hint = if self.firmware.is_none() {
            "请先载入固件，再连接调试探针"
        } else if !self.can_run() {
            "需等待当前任务结束并确认探针就绪"
        } else {
            "固定在侧栏顶部，滚动设置时依然可见"
        };
        let full = ui.available_width();
        if Self::sidebar_button(ui, p, "开始烧录", can_flash, true, full)
            .on_hover_text(hint)
            .clicked()
        {
            self.start_job(Job::Flash(Box::new(self.cfg.clone())), "烧录固件");
        }
        ui.add_space(10.0);
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
        ui.label(
            RichText::new("网络探针（ElaphureLink / CMSIS-DAP TCP）")
                .size(10.0)
                .color(p.text3),
        );
        let mut apply_network = false;
        let mut clear_network = false;
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 82.0).max(60.0);
            ui.add_sized(
                [w, 24.0],
                egui::TextEdit::singleline(&mut self.cfg.network_probe)
                    .hint_text("dap.local / 192.168.1.50:3240"),
            );
            if ui.button("应用").clicked() {
                apply_network = true;
            }
            if ui.button("清除").clicked() {
                clear_network = true;
            }
        });
        ui.label(
            RichText::new("填地址点「应用」即可在上方下拉框选中；清除则只用 USB 探针")
                .size(9.0)
                .color(p.text3),
        );
        if apply_network || clear_network {
            self.cfg.network_probe = self.cfg.network_probe.trim().to_owned();
            if clear_network {
                self.cfg.network_probe.clear();
            }
            if let Some(selector) = backend::parse_network_probe(&self.cfg.network_probe) {
                self.cfg.probe_id = Some(backend::network_probe_identity(&selector));
                self.connection = None;
                self.push_log(
                    Level::Info,
                    format!("已启用网络探针: {}", self.cfg.network_probe),
                );
                if let Some(entry) = backend::network_probe_entry(&self.cfg.network_probe) {
                    if !self.probes.iter().any(|x| x.id == entry.id) {
                        self.probes.push(entry);
                    }
                }
            } else {
                self.cfg.probe_id = None;
                self.connection = None;
                self.probes
                    .retain(|x| !x.id.starts_with("0000:0001:") && !x.id.starts_with("0000:0002:"));
                self.push_log(
                    Level::Info,
                    if clear_network {
                        "已清除网络探针，改用 USB 探针"
                    } else {
                        "网络探针地址为空，未启用"
                    },
                );
            }
            self.start_job(Job::RefreshProbes, "刷新探针");
        }
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
        let mut clear = false;
        ui.horizontal(|ui| {
            Self::section_title(ui, p, "固件文件");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let btn = egui::Button::new(RichText::new("✕").size(11.0).color(p.error))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::new(1.0, p.border))
                    .min_size(Vec2::new(20.0, 20.0));
                if ui
                    .add_enabled(self.has_firmware(), btn)
                    .on_hover_text("关闭固件：移除当前固件并清空右侧 16 进制查看器（保留最近固件记录）")
                    .clicked()
                {
                    clear = true;
                }
            });
        });
        ui.add_space(6.0);
        let w = ui.available_width();
        let mut picked: Option<PathBuf> = None;
        if Self::sidebar_button(ui, p, "浏览… / 拖入固件", true, false, w).clicked() {
            picked = with_all_files(rfd::FileDialog::new())
                .add_filter("固件", &["hex", "ihex", "elf", "axf", "out", "bin"])
                .pick_file();
        }
        if clear {
            self.clear_firmware();
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
        ui.checkbox(&mut self.cfg.read_chip_id_before_flash, "烧录前读取芯片 ID")
            .on_hover_text("在同一连接内只读 DPIDR / CPUID / 厂商 IDCODE / UID，并记录到日志，不中断烧录");
        // 烧录主按钮已固定在侧栏顶部（不随滚动），此处不再重复放置。
        ui.add_space(8.0);
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
        ui.add_space(6.0);
        if Self::sidebar_button(
            ui,
            p,
            if self.ob_open {
                "选项字节设置（已打开）"
            } else {
                "选项字节设置…"
            },
            true,
            false,
            ui.available_width(),
        )
        .on_hover_text("读取 / 编辑芯片的读保护、BOR、看门狗、启动与写保护配置")
        .clicked()
        {
            self.ob_open = true;
            // Follow the currently selected chip unless the user edited the base.
            if self.ob_snapshot.is_none() {
                let base = option_bytes::profile_for_chip(&self.cfg.chip).base;
                self.ob_base_text = format!("0x{base:08X}");
            }
        }
        if Self::sidebar_button(
            ui,
            p,
            if self.chip_id_open {
                "芯片 ID（已打开）"
            } else {
                "读取芯片 ID…"
            },
            true,
            false,
            ui.available_width(),
        )
        .on_hover_text("只读识别目标：DPIDR / CPUID / 厂商 IDCODE / 96 位 UID / Flash 容量")
        .clicked()
        {
            self.chip_id_open = true;
        }
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
        let mut load: Option<PathBuf> = None;
        let mut remove: Option<PathBuf> = None;
        let mut clear_all = false;
        ui.horizontal(|ui| {
            Self::section_title(ui, p, "最近固件");
            if !self.recent.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let btn = egui::Button::new(RichText::new("清空").size(9.0).color(p.text3))
                        .fill(Color32::TRANSPARENT)
                        .min_size(Vec2::new(0.0, 18.0));
                    if ui
                        .add(btn)
                        .on_hover_text("清空最近固件记录（不影响当前已载入的固件）")
                        .clicked()
                    {
                        clear_all = true;
                    }
                });
            }
        });
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
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(name).size(10.0).color(p.text2))
                            .fill(Color32::TRANSPARENT),
                    )
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    load = Some(path.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let btn = egui::Button::new(RichText::new("✕").size(9.0).color(p.text3))
                        .fill(Color32::TRANSPARENT)
                        .min_size(Vec2::new(18.0, 18.0));
                    if ui
                        .add(btn)
                        .on_hover_text(format!("从最近固件记录中移除：{}", path.display()))
                        .clicked()
                    {
                        remove = Some(path.clone());
                    }
                });
            });
        }
        if clear_all {
            self.recent.clear();
            self.push_log(Level::Info, "已清空最近固件记录");
        } else if let Some(path) = remove {
            self.recent.retain(|p| p != &path);
            self.push_log(Level::Info, format!("已从最近固件移除: {}", path.display()));
        }
        if let Some(path) = load {
            self.set_firmware(path);
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
            ui.label(RichText::new("串口设置").size(13.0).strong().color(p.text));
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
        let vcp = self.ports.iter().filter(|x| x.is_debug_vcp()).count();
        ui.label(
            RichText::new(format!("在线 {} 个 · 调试器虚拟串口 {vcp} 个", self.ports.len()))
                .size(10.0)
                .color(p.text3),
        );
        ui.add_space(8.0);

        let ports = self.ports.clone();
        let selected = self.selected_port.clone();
        let probe_id = self
            .cfg
            .probe_id
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let error = self.serial_error.clone();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                Self::section_title(ui, p, "串口参数");
                ui.add_space(6.0);
                ui.label(RichText::new("端口").size(10.0).color(p.text3));
                let current = selected.clone().unwrap_or_else(|| "未选择".to_owned());
                egui::ComboBox::from_id_salt("serial_port_combo")
                    .selected_text(RichText::new(current).color(p.text))
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        if ports.is_empty() {
                            ui.label(RichText::new("未发现串口").size(10.0).color(p.text3));
                        }
                        for port in &ports {
                            let chosen = selected.as_deref() == Some(port.name.as_str());
                            let text = format!("{} · {}", port.name, port.kind.label());
                            if ui.selectable_label(chosen, text).clicked() {
                                self.selected_port = Some(port.name.clone());
                            }
                        }
                    });
                ui.add_space(6.0);
                ui.label(RichText::new("波特率").size(10.0).color(p.text3));
                ui.add_sized(
                    [ui.available_width(), 24.0],
                    egui::TextEdit::singleline(&mut self.serial_baud_text).hint_text("115200"),
                );
                egui::ComboBox::from_id_salt("serial_baud_presets")
                    .selected_text("常用波特率…")
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for baud in serial::BAUD_PRESETS {
                            let text = baud.to_string();
                            if ui
                                .selectable_label(self.serial_baud_text == text, &text)
                                .clicked()
                            {
                                self.serial_baud_text = text;
                            }
                        }
                    });
                ui.add_space(4.0);
                // Parameters are locked while a session owns the handle; the
                // worker would otherwise keep the old DCB.
                let editable = !self.serial_open;
                ui.add_enabled_ui(editable, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("数据位").size(9.0).color(p.text3));
                        egui::ComboBox::from_id_salt("serial_databits_combo")
                            .selected_text(self.serial_cfg.data_bits.to_string())
                            .width(54.0)
                            .show_ui(ui, |ui| {
                                for n in [5u8, 6, 7, 8] {
                                    ui.selectable_value(
                                        &mut self.serial_cfg.data_bits,
                                        n,
                                        n.to_string(),
                                    );
                                }
                            });
                        ui.label(RichText::new("停止位").size(9.0).color(p.text3));
                        egui::ComboBox::from_id_salt("serial_stopbits_combo")
                            .selected_text(self.serial_cfg.stop_bits.label())
                            .width(54.0)
                            .show_ui(ui, |ui| {
                                for s in [
                                    serial::StopBits::One,
                                    serial::StopBits::OneFive,
                                    serial::StopBits::Two,
                                ] {
                                    ui.selectable_value(
                                        &mut self.serial_cfg.stop_bits,
                                        s,
                                        s.label(),
                                    );
                                }
                            });
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("校验位").size(9.0).color(p.text3));
                    egui::ComboBox::from_id_salt("serial_parity_combo")
                        .selected_text(self.serial_cfg.parity.label())
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for q in [
                                serial::Parity::None,
                                serial::Parity::Odd,
                                serial::Parity::Even,
                                serial::Parity::Mark,
                                serial::Parity::Space,
                            ] {
                                ui.selectable_value(
                                    &mut self.serial_cfg.parity,
                                    q,
                                    q.label(),
                                );
                            }
                        });
                    ui.add_space(4.0);
                    ui.label(RichText::new("流控").size(9.0).color(p.text3));
                    egui::ComboBox::from_id_salt("serial_flow_combo")
                        .selected_text(self.serial_cfg.flow.label())
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for f in [
                                serial::FlowControl::None,
                                serial::FlowControl::XonXoff,
                                serial::FlowControl::RtsCts,
                            ] {
                                ui.selectable_value(
                                    &mut self.serial_cfg.flow,
                                    f,
                                    f.label(),
                                );
                            }
                        });
                });
                ui.add_space(8.0);
                let full = ui.available_width();
                let mut toggle = None;
                if self.serial_open {
                    if Self::sidebar_button(ui, p, "关闭串口", true, false, full).clicked() {
                        toggle = Some(false);
                    }
                } else if Self::sidebar_button(
                    ui,
                    p,
                    "打开串口",
                    selected.is_some(),
                    true,
                    full,
                )
                .clicked()
                {
                    toggle = Some(true);
                }
                if toggle == Some(true) {
                    self.open_serial();
                } else if toggle == Some(false) {
                    self.close_serial();
                }
                ui.add_space(4.0);
                let (status, color) = if self.serial_open {
                    (
                        format!(
                            "● 已打开 {} · {}",
                            self.serial_open_name.as_deref().unwrap_or("?"),
                            self.serial_cfg.label()
                        ),
                        p.success,
                    )
                } else {
                    ("● 未打开".to_owned(), p.text3)
                };
                ui.label(RichText::new(status).size(10.0).color(color));
                if let Some(err) = &error {
                    ui.label(RichText::new(err).size(9.0).color(p.error));
                }
                if self.serial_open {
                    ui.label(
                        RichText::new("参数已锁定，关闭串口后可修改")
                            .size(9.0)
                            .color(p.text3),
                    );
                }
                ui.add_space(12.0);

                Self::section_title(ui, p, "在线端口");
                ui.add_space(6.0);
                if ports.is_empty() {
                    ui.label(
                        RichText::new("未发现串口，请检查 USB 连接")
                            .size(10.0)
                            .color(p.text3),
                    );
                }
                let mut picked: Option<String> = None;
                for item in &ports {
                    let active = selected.as_deref() == Some(item.name.as_str());
                    let highlight = item.is_debug_vcp();
                    Frame::new()
                        .fill(if active { p.accent_soft } else { p.card })
                        .stroke(Stroke::new(
                            1.0,
                            if active { p.accent } else { p.border },
                        ))
                        .corner_radius(8.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            let label = format!("{} · {}", item.name, item.kind.label());
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(label)
                                            .size(10.5)
                                            .color(if highlight { p.accent } else { p.text }),
                                    )
                                    .fill(Color32::TRANSPARENT),
                                )
                                .clicked()
                            {
                                picked = Some(item.name.clone());
                            }
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
                    ui.add_space(4.0);
                }
                if let Some(name) = picked {
                    self.selected_port = Some(name);
                }
                ui.add_space(12.0);

                if let Some(port) = ports
                    .iter()
                    .find(|x| selected.as_deref() == Some(x.name.as_str()))
                {
                    let same_probe = port
                        .vid_pid()
                        .is_some_and(|id| probe_id.starts_with(&id));
                    let rows = [
                        ("设备描述", port.description.clone()),
                        ("设备对象", port.device_object.clone()),
                        ("VID:PID", port.vid_pid().unwrap_or_else(|| "—".to_owned())),
                        (
                            "USB 接口",
                            port.interface
                                .map(|i| format!("MI_{i:02}"))
                                .unwrap_or_else(|| "—".to_owned()),
                        ),
                        ("设备实例 ID", port.instance_id.clone()),
                        (
                            "关联当前探针",
                            if same_probe {
                                "是 · 同一 USB 设备".to_owned()
                            } else {
                                "否".to_owned()
                            },
                        ),
                    ];
                    egui::CollapsingHeader::new(
                        RichText::new("端口详情").size(11.0).strong().color(p.text2),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Grid::new("serial_sidebar_details")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                for (key, value) in &rows {
                                    ui.label(RichText::new(*key).size(9.0).color(p.text3));
                                    ui.label(
                                        RichText::new(value.as_str()).size(9.0).color(p.text2),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                    ui.add_space(10.0);
                }
                Self::section_title(ui, p, "关于虚拟串口");
                ui.label(
                    RichText::new(
                        "DAPLink / CMSIS-DAP 常把 CDC 虚拟串口与调试接口做在同一个 USB 设备上；\
                         这里按 VID/PID 与设备描述自动标注。调试探针仍在工作台的连接配置中选择。",
                    )
                    .size(9.0)
                    .color(p.text3),
                );
            });
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
        if ui.button("复制全部日志").clicked() {
            self.copy_all_logs(ui.ctx());
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

    // ---------------------------------------------------------------------
    // Option-byte editor
    // ---------------------------------------------------------------------

    /// A fresh read resets every pending write state. Nothing about OB editing
    /// is persisted, so a restart can never leave a write armed.
    fn on_option_bytes(&mut self, snapshot: ObSnapshot) {
        let Some(image) = ObImage::from_slice(&snapshot.bytes) else {
            self.push_log(
                Level::Error,
                format!("选项字节长度异常：{} 字节", snapshot.bytes.len()),
            );
            return;
        };
        self.ob_raw_hex = image.hex();
        self.ob_raw_error = None;
        self.ob_edit = Some(image);
        self.ob_snapshot = Some(snapshot);
        self.ob_acknowledge = false;
        self.ob_armed = false;
        self.ob_open = true;
        self.push_log(Level::Ok, format!("已读取选项字节: {}", image.hex()));
    }

    fn draw_option_bytes_dialog(&mut self, ctx: &egui::Context, p: Palette) {
        if !self.ob_open {
            return;
        }
        let chip = self.cfg.chip.clone();
        let profile = option_bytes::profile_for_chip(&chip);
        let can_run = self.can_run();
        let snapshot = self.ob_snapshot.clone();
        let mut image = self
            .ob_edit
            .unwrap_or_else(ObImage::factory_default);
        let mut base_text = self.ob_base_text.clone();
        let mut raw_hex = self.ob_raw_hex.clone();
        if raw_hex.trim().is_empty() {
            // First open (before any read): keep the HEX box in sync with the
            // baseline image instead of showing an empty editor.
            raw_hex = image.hex();
        }
        let mut raw_error = self.ob_raw_error.clone();
        let mut ack = self.ob_acknowledge;
        let mut armed = self.ob_armed;
        let mut open = true;
        let mut action = ObAction::None;

        egui::Window::new("选项字节设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(660.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("{} · {}", chip, profile.label))
                        .size(15.0)
                        .strong()
                        .color(p.text),
                );
                ui.label(RichText::new(profile.note).size(10.0).color(p.text3));
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("OB 基址").size(11.0).color(p.text2));
                    ui.add_sized(
                        [150.0, 24.0],
                        egui::TextEdit::singleline(&mut base_text),
                    );
                    if ui
                        .add_enabled(can_run, egui::Button::new("读取当前值"))
                        .on_hover_text("只读，不改动芯片；按当前选中的探针与芯片连接一次")
                        .clicked()
                    {
                        if let Some(base) = option_bytes::parse_base(&base_text) {
                            action = ObAction::Read(base);
                        }
                    }
                });
                if !profile.writable {
                    ui.label(
                        RichText::new("该芯片未内置匹配的 OPT.FLM：可读取、编辑并复制，但不能写入")
                            .size(10.0)
                            .color(p.warn),
                    );
                }
                ui.add_space(4.0);
                ui.separator();

                // Raw block: what the chip has now vs. what would be written.
                if let Some(snap) = &snapshot {
                    if let Some(old) = ObImage::from_slice(&snap.bytes) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("当前").size(10.0).color(p.text3));
                            ui.label(
                                RichText::new(old.hex())
                                    .monospace()
                                    .size(11.0)
                                    .color(p.text2),
                            );
                        });
                    }
                } else {
                    ui.label(
                        RichText::new("尚未读取芯片 · 显示的是无保护参考值，请先点「读取当前值」")
                            .size(10.0)
                            .color(p.warn),
                    );
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("目标").size(10.0).color(p.text3));
                    ui.label(
                        RichText::new(image.hex())
                            .monospace()
                            .size(11.0)
                            .color(p.accent),
                    );
                });
                let ok = image.complements_valid();
                ui.label(
                    RichText::new(if ok {
                        "反码与保留字节校验通过"
                    } else {
                        "反码/保留字节校验未通过：写入前需点「补全反码」"
                    })
                    .size(10.0)
                    .color(if ok { p.success } else { p.warn }),
                );
                ui.add_space(4.0);
                ui.separator();

                let fields = option_bytes::fields_for(profile.kind);
                if fields.is_empty() {
                    ui.label(
                        RichText::new("该芯片没有已核对的字段布局，请用下方原始 HEX 编辑")
                            .size(11.0)
                            .color(p.text2),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("ob_field_scroll")
                        .max_height(238.0)
                        .show(ui, |ui| {
                            egui::Grid::new("ob_field_grid")
                                .num_columns(2)
                                .spacing([12.0, 6.0])
                                .show(ui, |ui| {
                                    for def in fields {
                                        ui.label(
                                            RichText::new(def.label)
                                                .size(11.0)
                                                .color(if def.danger {
                                                    p.warn
                                                } else {
                                                    p.text2
                                                }),
                                        )
                                        .on_hover_text(def.help);
                                        let before =
                                            option_bytes::read_field(&image, def);
                                        let mut value = before;
                                        if !def.choices.is_empty() {
                                            egui::ComboBox::from_id_salt(def.id)
                                                .selected_text(
                                                    option_bytes::format_value(def, value),
                                                )
                                                .width(250.0)
                                                .show_ui(ui, |ui| {
                                                    for choice in def.choices {
                                                        ui.selectable_value(
                                                            &mut value,
                                                            choice.value,
                                                            choice.label,
                                                        );
                                                    }
                                                });
                                        } else {
                                            let mask = if def.width >= 32 {
                                                u32::MAX
                                            } else {
                                                (1u32 << def.width) - 1
                                            };
                                            let mut drag = egui::DragValue::new(&mut value)
                                                .range(0u32..=mask)
                                                .speed(1.0);
                                            if def.width > 8 {
                                                drag = drag.hexadecimal(
                                                    (def.width as usize + 3) / 4,
                                                    false,
                                                    true,
                                                );
                                            }
                                            ui.add(drag);
                                        }
                                        if value != before
                                            && option_bytes::write_field(&mut image, def, value)
                                                .is_ok()
                                        {
                                            // Field edits always keep complements valid.
                                            raw_hex = image.hex();
                                            raw_error = None;
                                            armed = false;
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                }
                ui.add_space(4.0);
                ui.separator();

                ui.label(
                    RichText::new("原始 16 字节 HEX（可直接编辑 / 复制）")
                        .size(10.0)
                        .color(p.text3),
                );
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [(ui.available_width() - 168.0).max(180.0), 24.0],
                        egui::TextEdit::singleline(&mut raw_hex),
                    );
                    if ui.button("应用").clicked() {
                        match option_bytes::parse_hex_image(&raw_hex) {
                            Ok(parsed) => {
                                image = parsed;
                                raw_error = None;
                                armed = false;
                            }
                            Err(err) => raw_error = Some(err),
                        }
                    }
                    if ui
                        .button("补全反码")
                        .on_hover_text("按当前配置字节重算全部反码，并把保留字节置为 0xFF")
                        .clicked()
                    {
                        image.refresh_complements();
                        raw_hex = image.hex();
                        raw_error = None;
                        armed = false;
                    }
                });
                if let Some(err) = &raw_error {
                    ui.label(RichText::new(err).size(10.0).color(p.error));
                }

                // Change list drives the confirmation copy.
                let changes = snapshot
                    .as_ref()
                    .and_then(|s| ObImage::from_slice(&s.bytes))
                    .map(|old| option_bytes::diff(profile.kind, &old, &image))
                    .unwrap_or_default();
                ui.add_space(4.0);
                if snapshot.is_none() {
                    ui.label(
                        RichText::new("尚未读取芯片 · 请先点「读取当前值」，再修改并写入")
                            .size(10.0)
                            .color(p.warn),
                    );
                } else if changes.is_empty() {
                    ui.label(
                        RichText::new("目标与当前值一致，没有待写入的变更")
                            .size(10.0)
                            .color(p.text3),
                    );
                } else {
                    ui.label(
                        RichText::new(format!("待写入 {} 项变更：", changes.len()))
                            .size(10.0)
                            .color(p.text2),
                    );
                    for change in &changes {
                        ui.label(
                            RichText::new(format!(
                                "  {} · {} → {}{}",
                                change.label,
                                change.from,
                                change.to,
                                if change.danger { "  ⚠ 高风险" } else { "" }
                            ))
                            .size(10.0)
                            .color(if change.danger { p.warn } else { p.text2 }),
                        );
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "⚠ 写入会改动读保护、启动模式、复位脚与写保护配置。错误的 RDP / BOOT / NRST 设置\n\
                         可能让芯片无法通过 SWD 再次连接；恢复通常需要整片擦除（会清除用户 Flash）。\n\
                         写入后请给目标板重新上电，选项字节才会完全生效。",
                    )
                    .size(10.0)
                    .color(p.warn),
                );
                ui.checkbox(&mut ack, "我已了解风险，确认要改写芯片配置");
                ui.add_space(4.0);
                let write_ready = profile.writable
                    && can_run
                    && snapshot.is_some()
                    && !changes.is_empty()
                    && ack
                    && image.complements_valid();
                ui.horizontal(|ui| {
                    if ui.button("恢复为当前值").clicked() {
                        action = ObAction::Reset;
                    }
                    if ui
                        .button("复制目标 HEX")
                        .on_hover_text("复制到剪贴板，便于粘贴到其它烧录工具")
                        .clicked()
                    {
                        action = ObAction::Copy(image.hex());
                    }
                    let label = if armed { "再次点击以确认写入" } else { "写入选项字节" };
                    if ui
                        .add_enabled(write_ready, egui::Button::new(label))
                        .on_hover_text("需要二次点击确认；只写 16 字节 OB 区，不动用户 Flash 内容")
                        .clicked()
                    {
                        if armed {
                            if let Some(base) = option_bytes::parse_base(&base_text) {
                                action = ObAction::Write(image, base);
                            }
                            armed = false;
                        } else {
                            armed = true;
                        }
                    }
                });
            });

        // Fold local edits back into the app. Taking this after `show` avoids
        // borrowing `self` from inside the egui closure.
        self.ob_edit = Some(image);
        self.ob_base_text = base_text;
        self.ob_raw_hex = raw_hex;
        self.ob_raw_error = raw_error;
        self.ob_acknowledge = ack;
        self.ob_armed = armed;
        if !open {
            self.ob_open = false;
            return;
        }
        match action {
            ObAction::None => {}
            ObAction::Read(base) => {
                self.push_log(Level::Info, format!("读取选项字节 0x{base:08X} …"));
                self.start_job(
                    Job::ReadOptionBytes {
                        cfg: Box::new(self.cfg.clone()),
                        base,
                    },
                    "读取选项字节",
                );
            }
            ObAction::Write(image, base) => {
                self.push_log(
                    Level::Warn,
                    format!("二次确认通过 · 准备写入选项字节 0x{base:08X}: {}", image.hex()),
                );
                self.start_job(
                    Job::WriteOptionBytes {
                        cfg: Box::new(self.cfg.clone()),
                        base,
                        image,
                        acknowledged: self.ob_acknowledge,
                    },
                    "写入选项字节",
                );
            }
            ObAction::Reset => {
                if let Some(old) = self
                    .ob_snapshot
                    .as_ref()
                    .and_then(|s| ObImage::from_slice(&s.bytes))
                {
                    self.ob_edit = Some(old);
                    self.ob_raw_hex = old.hex();
                    self.ob_raw_error = None;
                    self.ob_armed = false;
                    self.ob_acknowledge = false;
                    self.push_log(Level::Info, "已恢复为芯片当前选项字节");
                }
            }
            ObAction::Copy(text) => {
                ctx.copy_text(text);
                self.push_log(Level::Info, "目标选项字节 HEX 已复制到剪贴板");
            }
        }
    }

    /// Read-only chip identification window. A snapshot is only ever displayed
    /// after an explicit read (or a flash with the pre-check option enabled),
    /// and a chip mismatch is flagged so an old board's ID is never mistaken
    /// for the currently selected model.
    fn draw_chip_id_dialog(&mut self, ctx: &egui::Context, p: Palette) {
        if !self.chip_id_open {
            return;
        }
        let chip = self.cfg.chip.clone();
        let profile = chip_id::profile_for_chip(&chip);
        let snapshot = self.chip_id_snapshot.clone();
        let can_run = self.can_run();
        let mut custom = self.chip_id_custom.clone();
        let mut custom_error = self.chip_id_custom_error.clone();
        let mut open = true;
        let mut action = ChipIdAction::None;

        egui::Window::new("目标芯片 ID")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(700.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("{chip} · {}", profile.label))
                        .size(15.0)
                        .strong()
                        .color(p.text),
                );
                ui.label(RichText::new(profile.source).size(10.0).color(p.text3));
                if profile.kind == chip_id::IdKind::Generic {
                    ui.label(
                        RichText::new(
                            "未内置厂商映射：仅能读取通用 ARM 寄存器，可填写下方自定义地址",
                        )
                        .size(10.0)
                        .color(p.warn),
                    );
                }
                ui.label(
                    RichText::new("只读：全部为寄存器读取，不会改写 Flash 或选项字节")
                        .size(10.0)
                        .color(p.success),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_run, egui::Button::new("读取芯片 ID"))
                        .on_hover_text("按当前选中的探针与芯片连接一次，读取全部可用 ID 寄存器")
                        .clicked()
                    {
                        let addr = if custom.trim().is_empty() {
                            custom_error = None;
                            None
                        } else {
                            match parse_num(&custom) {
                                Some(a) => {
                                    custom_error = None;
                                    Some(a)
                                }
                                None => {
                                    custom_error = Some(
                                        "自定义地址无效，示例：0x40015800（可留空）".to_owned(),
                                    );
                                    None
                                }
                            }
                        };
                        if custom_error.is_none() {
                            action = ChipIdAction::Read(addr);
                        }
                    }
                    if ui.button("清空结果").clicked() {
                        action = ChipIdAction::Clear;
                    }
                    if ui
                        .add_enabled(snapshot.is_some(), egui::Button::new("复制全部"))
                        .clicked()
                    {
                        action = ChipIdAction::Copy(
                            snapshot.as_ref().map(|s| s.text()).unwrap_or_default(),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("自定义地址").size(10.0).color(p.text3));
                    ui.add_sized(
                        [160.0, 22.0],
                        egui::TextEdit::singleline(&mut custom)
                            .hint_text("0x40015800（可留空）"),
                    );
                    ui.label(
                        RichText::new("额外按小端 32 位读取，适合未内置映射的型号")
                            .size(10.0)
                            .color(p.text3),
                    );
                });
                if let Some(err) = &custom_error {
                    ui.label(RichText::new(err.clone()).size(10.0).color(p.error));
                }
                ui.add_space(4.0);
                ui.separator();

                match &snapshot {
                    None => {
                        ui.label(
                            RichText::new("尚未读取芯片 · 点「读取芯片 ID」连接目标并采集身份信息")
                                .size(11.0)
                                .color(p.warn),
                        );
                    }
                    Some(snap) => {
                        if snap.chip != chip {
                            ui.label(
                                RichText::new(format!(
                                    "⚠ 该结果来自 {}，当前选择为 {chip}，请重新读取",
                                    snap.chip
                                ))
                                .size(10.0)
                                .color(p.warn),
                            );
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("chip_id_scroll")
                            .max_height(330.0)
                            .show(ui, |ui| {
                                egui::Grid::new("chip_id_grid")
                                    .num_columns(3)
                                    .spacing([12.0, 6.0])
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new("寄存器").size(10.0).color(p.text3),
                                        );
                                        ui.label(
                                            RichText::new("地址").size(10.0).color(p.text3),
                                        );
                                        ui.label(
                                            RichText::new("原始值 / 解析")
                                                .size(10.0)
                                                .color(p.text3),
                                        );
                                        ui.end_row();
                                        for field in &snap.fields {
                                            ui.label(
                                                RichText::new(field.label.clone())
                                                    .size(11.0)
                                                    .strong()
                                                    .color(p.text),
                                            );
                                            let addr = field
                                                .address
                                                .map(|a| format!("0x{a:08X}"))
                                                .unwrap_or_else(|| "DP 寄存器".to_owned());
                                            ui.label(
                                                RichText::new(addr)
                                                    .monospace()
                                                    .size(10.0)
                                                    .color(p.text3),
                                            );
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(field.raw.clone())
                                                        .monospace()
                                                        .size(11.0)
                                                        .color(p.accent),
                                                );
                                                ui.label(
                                                    RichText::new(field.detail.clone())
                                                        .size(10.0)
                                                        .color(p.text2),
                                                );
                                            });
                                            ui.end_row();
                                        }
                                    });
                            });
                        for warning in &snap.warnings {
                            ui.label(
                                RichText::new(format!("⚠ {warning}"))
                                    .size(10.0)
                                    .color(p.warn),
                            );
                        }
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!("共 {} 项", snap.fields.len()))
                                .size(10.0)
                                .color(p.text3),
                        );
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(profile.note).size(10.0).color(p.text3));
            });

        self.chip_id_custom = custom;
        self.chip_id_custom_error = custom_error;
        if !open {
            self.chip_id_open = false;
            return;
        }
        match action {
            ChipIdAction::None => {}
            ChipIdAction::Clear => {
                self.chip_id_snapshot = None;
                self.chip_id_custom_error = None;
            }
            ChipIdAction::Copy(text) => {
                ctx.copy_text(text);
                self.push_log(Level::Info, "芯片 ID 报告已复制到剪贴板");
            }
            ChipIdAction::Read(custom) => {
                self.push_log(Level::Info, "读取目标芯片 ID（只读）…");
                self.start_job(
                    Job::ReadChipId {
                        cfg: Box::new(self.cfg.clone()),
                        custom,
                    },
                    "读取芯片 ID",
                );
            }
        }
    }

    /// Plain-text log shared by clipboard copy and file export. With
    /// `include_debug = false`, hidden DEBUG rows are left out so the copied
    /// text matches exactly what the console is currently showing.
    fn log_text(&self, include_debug: bool) -> String {
        logs_to_text(&self.logs, include_debug)
    }

    /// Copy the visible log to the system clipboard. egui can select label
    /// text, but a Windows-subsystem window does not always deliver Ctrl+C as
    /// an `Event::Copy`, so an explicit button is the reliable path.
    fn copy_all_logs(&mut self, ctx: &egui::Context) {
        let count = self
            .logs
            .iter()
            .filter(|l| self.show_debug || l.level != Level::Debug)
            .count();
        let text = self.log_text(self.show_debug);
        ctx.copy_text(text);
        self.push_log(Level::Info, format!("已复制 {count} 条日志到剪贴板"));
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
        let text = self.log_text(true);
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
                    // The terminal owns its own scroll areas (receive pane),
                    // so there must be no outer scrolling container.
                    self.draw_serial_center(ui, p);
                } else {
                    self.draw_hex_viewer(ui, p);
                }
            });
    }

    /// The session pane: a virtualised hex dump of the loaded firmware. All
    /// settings and actions live in the narrow "当前会话" sidebar.
    fn draw_hex_viewer(&mut self, ui: &mut Ui, p: Palette) {
        let mut picked: Option<PathBuf> = None;
        let mut clear = false;
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

        let has_fw = self.has_firmware();
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(12.0)
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Top-right corner of the firmware card: a bare ✕ with no
                // text label, so it reads as a standard close affordance.
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&file_name).size(12.0).strong().color(p.text),
                        )
                        .truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn = egui::Button::new(RichText::new("✕").size(12.0).color(p.error))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, p.border))
                            .min_size(Vec2::new(22.0, 20.0));
                        if ui
                            .add_enabled(has_fw, btn)
                            .on_hover_text("关闭固件：移除当前固件并清空查看器（保留最近固件记录）")
                            .clicked()
                        {
                            clear = true;
                        }
                    });
                });
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
        if clear {
            self.clear_firmware();
        }
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
                let mut copy_all = false;
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
                        if ui
                            .button("复制全部")
                            .on_hover_text("把当前显示的日志按时间顺序复制到剪贴板；单条日志可右键复制")
                            .clicked()
                        {
                            copy_all = true;
                        }
                        ui.checkbox(&mut self.show_debug, "调试信息");
                    });
                });
                if copy_all {
                    self.copy_all_logs(ui.ctx());
                }
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
                                    let line = format!(
                                        "[{}] [{}] {}",
                                        entry.time,
                                        level_label(entry.level),
                                        entry.msg
                                    );
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
                                        let _ = ui
                                            .label(
                                                RichText::new(&entry.msg)
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(p.text2),
                                            )
                                            .on_hover_text("选中文字可用 Ctrl+C；右键可复制整行")
                                            .context_menu(|ui| {
                                                if ui.button("复制整行").clicked() {
                                                    ui.ctx().copy_text(line.clone());
                                                }
                                                if ui.button("只复制消息内容").clicked() {
                                                    ui.ctx().copy_text(entry.msg.clone());
                                                }
                                            });
                                    });
                                }
                            });
                    });
            });
    }

    /// The serial page's session pane: a receive / send terminal.
    fn draw_serial_center(&mut self, ui: &mut Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("串口会话").size(22.0).strong().color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = if self.serial_open { "● 已打开" } else { "● 未打开" };
                ui.label(
                    RichText::new(text)
                        .size(11.0)
                        .color(if self.serial_open { p.success } else { p.text3 }),
                );
            });
        });
        ui.label(
            RichText::new("左侧设置波特率与帧格式并打开端口；接收区实时显示数据，发送区支持文本 / HEX")
                .size(11.0)
                .color(p.text3),
        );
        ui.add_space(10.0);

        // Reserve room for the send pane even when the bottom log dock is
        // expanded. Both panes now keep their controls on the header row,
        // so the freed rows go to the receive area instead.
        let send_extra = 32.0 + if self.serial_emoji_open { 42.0 } else { 0.0 };
        let rx_height = (ui.available_height() - 195.0 - send_extra).max(110.0);
        let name = self
            .serial_open_name
            .clone()
            .or_else(|| self.selected_port.clone())
            .unwrap_or_else(|| "未选择端口".to_owned());
        let cfg = self.serial_cfg.label();
        let open = self.serial_open;
        let rx_total = self.serial_rx_bytes;
        let tx_total = self.serial_tx_bytes;
        let mut clear_rx = false;
        let mut toggle = None;
        let serial_error = self.serial_error.clone();

        let rx_hex = self.serial_rx_hex;
        let auto_scroll = self.serial_auto_scroll;
        let rx = self.serial_rx.clone();
        // One zoom gesture per frame; each pane only reacts when the pointer
        // is over it. `zoom_delta` is egui's unified ctrl+wheel / pinch value.
        let zoom = ui.input(|i| font_zoom_delta(i.zoom_delta()));
        let rx_size = self.serial_rx_font_size;
        let tx_size = self.serial_tx_font_size;
        let rows = rx.len().saturating_add(15) / 16;
        let rx_text = if rx_hex {
            String::new()
        } else {
            String::from_utf8_lossy(&rx).into_owned()
        };
        let rx_frame = Frame::new()
            .fill(p.console)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The separate info strip is gone: port, parameters, counters
                // and the open / clear actions all live on this header row.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("接收区").size(12.0).strong().color(p.text));
                    ui.label(
                        RichText::new(format!("{name} · {cfg}"))
                            .size(9.0)
                            .color(p.accent),
                    );
                    ui.label(
                        RichText::new(format!(
                            "RX {rx_total} · TX {tx_total} · 缓冲 {} B",
                            rx.len()
                        ))
                        .size(9.0)
                        .color(p.text3),
                    );
                    // Non-UTF-8 bytes show up as U+FFFD in text mode. Point at
                    // the HEX view instead of leaving the user with "乱码".
                    if !rx_hex && rx_text.contains('\u{FFFD}') {
                        ui.label(
                            RichText::new("含非文本字节 · 建议勾选 HEX 显示")
                                .size(9.0)
                                .color(p.warn),
                        );
                    }
                    if let Some(err) = &serial_error {
                        ui.label(RichText::new(err).size(9.0).color(p.error));
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.checkbox(&mut self.serial_rx_hex, "HEX 显示");
                            ui.checkbox(&mut self.serial_auto_scroll, "自动滚动");
                            if ui
                                .button(if open { "关闭串口" } else { "打开串口" })
                                .on_hover_text("打开 / 关闭当前串口")
                                .clicked()
                            {
                                toggle = Some(open);
                            }
                            if ui
                                .button("清空接收")
                                .on_hover_text("清空接收缓冲（累计 RX / TX 计数保留）")
                                .clicked()
                            {
                                clear_rx = true;
                            }
                            ui.label(
                                RichText::new(format!("{rx_size:.1} pt"))
                                    .size(9.0)
                                    .color(p.text3),
                            )
                            .on_hover_text("Ctrl + 滚轮缩放接收区文字");
                        },
                    );
                });
                ui.separator();
                if rx_hex {
                    let hex_width = 16 * 3 - 1 + 1;
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{:<8}", "OFFSET"))
                                .monospace()
                                .size(rx_size)
                                .color(p.text3),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(format!("{:<hex_width$}", "HEX BYTES"))
                                .monospace()
                                .size(rx_size)
                                .color(p.text3),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("ASCII")
                                .monospace()
                                .size(rx_size)
                                .color(p.text3),
                        );
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(rx_height)
                        .stick_to_bottom(auto_scroll)
                        .show_rows(ui, rx_size + 7.0, rows, |ui, range| {
                            for row in range {
                                let (offset, hex, gutter) = format_hex_row(&rx, 0, row, 16, true);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(offset)
                                            .monospace()
                                            .size(rx_size)
                                            .color(p.text3),
                                    );
                                    ui.add_space(10.0);
                                    ui.label(
                                        RichText::new(hex)
                                            .monospace()
                                            .size(rx_size)
                                            .color(p.text),
                                    );
                                    ui.add_space(10.0);
                                    ui.label(
                                        RichText::new(gutter)
                                            .monospace()
                                            .size(rx_size)
                                            .color(p.text2),
                                    );
                                });
                            }
                        });
                } else {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(rx_height)
                        .stick_to_bottom(auto_scroll)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(rx_text)
                                    .monospace()
                                    .size(rx_size)
                                    .color(p.text2),
                            );
                        });
                }
            });
        if clear_rx {
            self.serial_rx.clear();
        }
        if toggle == Some(false) {
            self.close_serial();
        } else if toggle == Some(true) {
            self.open_serial();
        }
        if zoom != 0.0 && ui.rect_contains_pointer(rx_frame.response.rect) {
            self.serial_rx_font_size = clamp_font_size(self.serial_rx_font_size, zoom);
        }
        ui.add_space(10.0);

        let mut send = false;
        let mut clear_tx = false;
        let mut quick_insert: Option<String> = None;
        let timer_was_on = self.serial_timer_on;
        let tx_frame = Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Mirror the receive pane: all actions live on one header
                // row, leaving the card body for the input box only.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("发送区").size(12.0).strong().color(p.text));
                    if parse_timer_ms(&self.serial_timer_text).is_none() {
                        ui.label(
                            RichText::new(format!(
                                "周期范围 {}–{} ms",
                                SERIAL_TIMER_MIN_MS, SERIAL_TIMER_MAX_MS
                            ))
                            .size(9.0)
                            .color(p.error),
                        );
                    } else if self.serial_timer_on {
                        ui.label(
                            RichText::new(format!("定时发送已发 {} 次", self.serial_timer_sent))
                                .size(9.0)
                                .color(p.accent),
                        );
                    } else if !open {
                        ui.label(RichText::new("需先打开串口").size(9.0).color(p.text3));
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(format!("{tx_size:.1} pt"))
                                    .size(9.0)
                                    .color(p.text3),
                            )
                            .on_hover_text("Ctrl + 滚轮缩放发送区文字");
                            let label = if self.serial_emoji_open {
                                "收起快捷输入"
                            } else {
                                "快捷输入 😀"
                            };
                            if ui.button(label).clicked() {
                                self.serial_emoji_open = !self.serial_emoji_open;
                            }
                            ui.checkbox(&mut self.serial_tx_hex, "HEX 发送");
                            ui.checkbox(&mut self.serial_append_nl, "追加 CRLF");
                            if ui.button("清空输入").clicked() {
                                clear_tx = true;
                            }
                            // The send action is a round button in the
                            // bottom-right corner (chat-input style), so the
                            // header only carries auxiliary controls.
                            // Added last so this group reads left-to-right as
                            // "定时发送 每 [1000] ms" in a right-to-left layout.
                            ui.label(RichText::new("ms").size(9.0).color(p.text3));
                            ui.add_sized(
                                [56.0, 20.0],
                                egui::TextEdit::singleline(&mut self.serial_timer_text)
                                    .desired_width(56.0),
                            );
                            ui.label(RichText::new("每").size(9.0).color(p.text3));
                            ui.checkbox(&mut self.serial_timer_on, "定时发送");
                        },
                    );
                });
                ui.add_space(6.0);
                // `show()` (instead of `add_sized`) exposes the caret position
                // so quick-input entries land where the user is typing.
                let output = egui::TextEdit::multiline(&mut self.serial_tx_text)
                    .id_salt("serial_send_box")
                    .hint_text(if self.serial_tx_hex {
                        "HEX 字节，例如 48 65 6C 6C 6F"
                    } else {
                        "输入要发送的文本…"
                    })
                    .desired_width(ui.available_width())
                    .desired_rows(2)
                    .font(FontId::new(tx_size, egui::FontFamily::Monospace))
                    .show(ui);
                if let Some(range) = output.cursor_range {
                    self.serial_tx_cursor = range.primary.index.0;
                }
                if self.serial_emoji_open {
                    ui.add_space(6.0);
                    egui::ScrollArea::horizontal()
                        .id_salt("quick_input_strip")
                        .max_height(32.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for emoji in QUICK_EMOJI {
                                    if ui
                                        .small_button(*emoji)
                                        .on_hover_text("插入到光标处")
                                        .clicked()
                                    {
                                        quick_insert = Some((*emoji).to_owned());
                                    }
                                }
                                ui.separator();
                                for (label, value) in QUICK_COMMANDS {
                                    if ui
                                        .small_button(*label)
                                        .on_hover_text("插入到光标处")
                                        .clicked()
                                    {
                                        quick_insert = Some((*value).to_owned());
                                    }
                                }
                            });
                        });
                }
                ui.add_space(10.0);
                // Chat-input style footer: a hint on the left and a round
                // accent send button pinned to the bottom-right corner. The
                // arrow is drawn as vector shapes, so it never depends on an
                // emoji font falling back to a coloured square.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Ctrl + Enter 快捷发送")
                            .size(9.0)
                            .color(p.text3),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                            let hovered = resp.hovered();
                            let bg = if open { p.accent } else { p.card_alt };
                            ui.painter().circle_filled(rect.center(), 16.0, bg);
                            if open && hovered {
                                ui.painter().add(egui::Shape::circle_stroke(
                                    rect.center(),
                                    15.0,
                                    Stroke::new(2.0, Color32::WHITE),
                                ));
                            }
                            let c = rect.center();
                            let fg = if open { Color32::WHITE } else { p.text3 };
                            ui.painter().add(egui::Shape::convex_polygon(
                                vec![
                                    c + Vec2::new(0.0, -8.0),
                                    c + Vec2::new(7.0, 1.0),
                                    c + Vec2::new(-7.0, 1.0),
                                ],
                                fg,
                                egui::epaint::PathStroke::NONE,
                            ));
                            ui.painter().rect_filled(
                                Rect::from_center_size(
                                    c + Vec2::new(0.0, 6.0),
                                    Vec2::new(3.0, 8.0),
                                ),
                                0.0,
                                fg,
                            );
                            let resp = resp.on_hover_text("发送（Ctrl + Enter）");
                            if open && resp.clicked() {
                                send = true;
                            }
                        },
                    );
                });
            });
        if zoom != 0.0 && ui.rect_contains_pointer(tx_frame.response.rect) {
            self.serial_tx_font_size = clamp_font_size(self.serial_tx_font_size, zoom);
        }
        let shortcut = ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
        if clear_tx {
            self.serial_tx_text.clear();
            self.serial_tx_cursor = 0;
        }
        if let Some(text) = quick_insert {
            self.insert_quick_text(&text);
        }
        if self.serial_timer_on && !timer_was_on {
            self.serial_timer_sent = 0;
            self.serial_last_tick = Some(Instant::now());
            self.push_log(
                Level::Info,
                format!("定时发送已启动 · 每 {} ms", self.serial_timer_text.trim()),
            );
        } else if !self.serial_timer_on && timer_was_on {
            self.push_log(
                Level::Info,
                format!("定时发送已停止 · 共 {} 次", self.serial_timer_sent),
            );
        }
        if send || (shortcut && open) {
            self.send_serial(false);
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
        let got_events = self.pump();
        // Periodic sending is driven by frames rather than a second thread.
        // Must run before drawing so the send box and counters are current.
        self.tick_serial_timer();
        // Keep the watcher in sync with UI state: never poll USB while a job
        // owns the debug probe, and honour the settings toggle.
        self.watch.enabled.store(self.auto_detect, Ordering::Relaxed);
        self.watch.busy.store(self.busy, Ordering::Relaxed);
        // A live terminal keeps receiving bytes while the window is idle;
        // without an explicit repaint request egui would never show them.
        if self.busy || got_events || self.serial_open || self.serial_timer_on {
            ctx.request_repaint_after(Duration::from_millis(50));
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
        self.draw_option_bytes_dialog(&ctx, p);
        self.draw_chip_id_dialog(&ctx, p);
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

/// Free function so clipboard / export formatting is unit-testable without
/// constructing a whole `App`. One line per entry, in display order.
fn logs_to_text(logs: &[LogEntry], include_debug: bool) -> String {
    logs.iter()
        .filter(|l| include_debug || l.level != Level::Debug)
        .map(|l| format!("[{}] [{}] {}", l.time, level_label(l.level), l.msg))
        .collect::<Vec<_>>()
        .join("\n")
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
            serial: SerialSettings::default(),
            serial_timer_ms: SERIAL_TIMER_DEFAULT_MS,
            serial_emoji_open: false,
            serial_rx_font_size: SERIAL_FONT_DEFAULT_RX,
            serial_tx_font_size: SERIAL_FONT_DEFAULT_TX,
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

    /// Serial settings written by older versions have no `serial` key.
    #[test]
    fn older_settings_without_serial_config_still_load() {
        let s = Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            recent_chips: vec![],
            serial: SerialSettings::default(),
            serial_timer_ms: SERIAL_TIMER_DEFAULT_MS,
            serial_emoji_open: false,
            serial_rx_font_size: SERIAL_FONT_DEFAULT_RX,
            serial_tx_font_size: SERIAL_FONT_DEFAULT_TX,
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        };
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("serial");
        let old: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(old.serial, SerialSettings::default());
        assert_eq!(old.serial.label(), "115200 8N1");
    }

    #[test]
    fn receive_buffer_is_a_bounded_rolling_window() {
        let mut buf = Vec::new();
        append_rx(&mut buf, b"hello", 8);
        assert_eq!(buf, b"hello".to_vec());
        append_rx(&mut buf, b" world!", 8);
        assert_eq!(buf, b"o world!".to_vec());
        assert_eq!(preview_bytes(b"HELLO"), "48 45 4C 4C 4F ");
        assert!(preview_bytes(&[0u8; 40]).ends_with('…'));
    }

    /// The serial sidebar owns the line settings and the session pane owns
    /// both the receive and the send area.
    #[test]
    fn serial_sidebar_has_line_settings_and_terminal_has_rx_tx_panes() {
        let source = include_str!("app.rs");
        for label in [
            concat!("串口", "参数"),
            concat!("波", "特率"),
            concat!("数", "据位"),
            concat!("校", "验位"),
            concat!("停", "止位"),
            concat!("流", "控"),
            concat!("接", "收区"),
            concat!("发", "送区"),
            concat!("HEX ", "发送"),
            concat!("打开", "串口"),
        ] {
            assert!(source.contains(label), "missing terminal UI element: {label}");
        }
        assert!(source.contains("spawn_serial_session"));
        assert!(source.contains("Event::SerialRx(bytes)"));
        assert!(source.contains("Event::SerialClosed(reason)"));
    }

    /// Configs written before periodic send / quick input existed must still
    /// deserialise, and the sender is never armed on startup.
    #[test]
    fn older_settings_without_timer_or_quick_input_still_load() {
        let s = Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            recent_chips: vec![],
            serial: SerialSettings::default(),
            serial_timer_ms: SERIAL_TIMER_DEFAULT_MS,
            serial_emoji_open: true,
            serial_rx_font_size: SERIAL_FONT_DEFAULT_RX,
            serial_tx_font_size: SERIAL_FONT_DEFAULT_TX,
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        };
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("serial_timer_ms");
        value.as_object_mut().unwrap().remove("serial_emoji_open");
        let old: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(old.serial_timer_ms, SERIAL_TIMER_DEFAULT_MS);
        assert!(!old.serial_emoji_open);
        // The enabled flag is deliberately absent from Settings: a restart
        // must never resume transmitting on its own.
        let source = include_str!("app.rs");
        let settings_body = source
            .split("struct Settings {")
            .nth(1)
            .unwrap()
            .split("\n}")
            .next()
            .unwrap();
        assert!(!settings_body.contains("serial_timer_on"));
    }

    #[test]
    fn periodic_send_period_is_validated() {
        assert_eq!(parse_timer_ms("1000"), Some(1000));
        assert_eq!(parse_timer_ms(" 250 "), Some(250));
        assert_eq!(parse_timer_ms("0"), None);
        assert_eq!(parse_timer_ms("99"), None);
        assert_eq!(parse_timer_ms("-1"), None);
        assert_eq!(parse_timer_ms("abc"), None);
        assert_eq!(parse_timer_ms(""), None);
        assert_eq!(parse_timer_ms("3600000"), Some(SERIAL_TIMER_MAX_MS));
        assert_eq!(parse_timer_ms("3600001"), None);
    }

    /// Quick input inserts real UTF-8 characters at a *character* caret, so
    /// emoji next to CJK text must not corrupt byte boundaries.
    #[test]
    fn quick_input_inserts_utf8_at_the_caret() {
        let mut text = "A中Z".to_owned();
        let mut caret = 2;
        insert_at_caret(&mut text, &mut caret, "😀");
        assert_eq!(text, "A中😀Z");
        assert_eq!(caret, 3);
        assert_eq!(text.chars().count(), 4);
        assert!(text.len() > text.chars().count());
        // An out-of-range caret clamps to the end instead of panicking.
        let mut wild = usize::MAX;
        insert_at_caret(&mut text, &mut wild, "!");
        assert_eq!(text, "A中😀Z!");
        assert_eq!(wild, 5);
    }

    #[test]
    fn quick_input_palette_contains_real_emoji_and_at_commands() {
        assert!(QUICK_EMOJI.len() >= 20);
        for emoji in QUICK_EMOJI {
            assert!(!emoji.is_empty());
            assert!(emoji.chars().all(|c| !c.is_ascii()));
        }
        assert!(QUICK_COMMANDS
            .iter()
            .any(|(label, value)| *label == "CRLF" && *value == "\r\n"));
        assert!(QUICK_COMMANDS.iter().any(|(label, _)| *label == "AT+GMR"));
        let joined: String = QUICK_EMOJI.concat();
        assert!(joined.len() > joined.chars().count());
    }

    #[test]
    fn periodic_send_fires_only_after_the_period_elapses() {
        let now = Instant::now();
        let period = Duration::from_millis(1000);
        assert!(timer_due(None, now, period));
        assert!(!timer_due(Some(now), now, period));
        assert!(!timer_due(Some(now), now + Duration::from_millis(999), period));
        assert!(timer_due(Some(now), now + Duration::from_millis(1000), period));
        assert!(timer_due(Some(now), now + Duration::from_secs(5), period));
        // A clock jump backwards saturates to zero, so it must not fire early.
        assert!(!timer_due(Some(now), now - Duration::from_secs(5), period));
    }

    #[test]
    fn ctrl_wheel_zoom_is_symmetric_and_bounded() {
        assert_eq!(font_zoom_delta(1.0), 0.0);
        assert!(font_zoom_delta(1.1) > 0.0);
        assert!(font_zoom_delta(0.9) < 0.0);
        assert_eq!(font_zoom_delta(f32::NAN), 0.0);
        assert_eq!(font_zoom_delta(0.0), 0.0);
        assert_eq!(clamp_font_size(11.5, 0.0), 11.5);
        assert_eq!(clamp_font_size(11.5, 100.0), SERIAL_FONT_MAX);
        assert_eq!(clamp_font_size(11.5, -100.0), SERIAL_FONT_MIN);
        assert_eq!(clamp_font_size(f32::INFINITY, 1.0), SERIAL_FONT_DEFAULT_RX);
        let mut size = SERIAL_FONT_DEFAULT_RX;
        size = clamp_font_size(size, font_zoom_delta(1.1));
        size = clamp_font_size(size, font_zoom_delta(1.0 / 1.1));
        assert!((size - SERIAL_FONT_DEFAULT_RX).abs() < 1e-4);
    }

    /// Older configs have no font-size keys; both panes must get their
    /// defaults instead of failing to deserialise.
    #[test]
    fn older_settings_without_font_sizes_still_load() {
        let s = Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            recent_chips: vec![],
            serial: SerialSettings::default(),
            serial_timer_ms: SERIAL_TIMER_DEFAULT_MS,
            serial_emoji_open: false,
            serial_rx_font_size: SERIAL_FONT_DEFAULT_RX,
            serial_tx_font_size: SERIAL_FONT_DEFAULT_TX,
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        };
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("serial_rx_font_size");
        value.as_object_mut().unwrap().remove("serial_tx_font_size");
        let old: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(old.serial_rx_font_size, SERIAL_FONT_DEFAULT_RX);
        assert_eq!(old.serial_tx_font_size, SERIAL_FONT_DEFAULT_TX);
    }

    /// egui 0.37 rasterises COLR/CBDT emoji only when epaint's `color_fonts`
    /// feature is on. egui/eframe do not forward that feature, so it must be
    /// enabled directly on the epaint dependency.
    #[test]
    fn colour_emoji_feature_and_font_order_are_configured() {
        let manifest = include_str!("../Cargo.toml");
        assert!(manifest.contains("color_fonts"));
        let theme = include_str!("theme.rs");
        assert!(theme.contains("seguiemj.ttf"));
        assert!(theme.contains("before_cjk"));
        let app = include_str!("app.rs");
        assert!(app.contains("font_zoom_delta"));
        assert!(app.contains("FontId::new(tx_size"));
        assert!(app.contains("rx_size + 7.0"));
    }

    /// A headless `--loopback` command is the quickest way to tell a wiring
    /// problem from a GUI problem, so it must stay wired into the CLI.
    #[test]
    fn loopback_self_check_is_available_from_the_cli() {
        let source = include_str!("main.rs");
        assert!(source.contains("--loopback"));
        assert!(source.contains("serial_loopback"));
    }

    #[test]
    fn periodic_send_and_quick_input_are_wired_into_the_terminal() {
        let source = include_str!("app.rs");
        for needle in [
            concat!("定时", "发送"),
            "tick_serial_timer",
            "QUICK_EMOJI",
            "insert_at_caret",
            "serial_timer_on",
            "serial_emoji_open",
            "send_serial(true)",
            "send_serial(false)",
        ] {
            assert!(source.contains(needle), "missing terminal feature: {needle}");
        }
        let theme = include_str!("theme.rs");
        assert!(theme.contains("seguiemj.ttf"), "emoji font fallback missing");
    }

    /// The OB editor must stay reachable from the workbench sidebar, driven by
    /// the background jobs, and guarded by the risk acknowledgement.
    #[test]
    fn option_bytes_editor_is_wired_into_the_workbench() {
        let app = include_str!("app.rs");
        for needle in [
            concat!("选项", "字节设置"),
            "draw_option_bytes_dialog",
            "Event::OptionBytes(snapshot)",
            "Job::ReadOptionBytes",
            "Job::WriteOptionBytes",
            "ob_acknowledge",
            "补全反码",
            "on_option_bytes",
        ] {
            assert!(app.contains(needle), "missing OB editor wiring: {needle}");
        }
        let backend_src = include_str!("backend.rs");
        assert!(backend_src.contains("option_bytes::validate"));
        assert!(backend_src.contains("PY32_OPT_YAML"));
        assert!(backend_src.contains("connect_with(cfg, tx, true)"));
        let main = include_str!("main.rs");
        assert!(main.contains("mod option_bytes;"));
        assert!(main.contains("--option-bytes"));
    }

    /// A pending OB write must never survive a restart. All editor state is
    /// runtime-only and therefore absent from the persisted Settings struct.
    #[test]
    fn option_bytes_write_state_is_never_persisted() {
        let source = include_str!("app.rs");
        let body = source
            .split("struct Settings {")
            .nth(1)
            .unwrap()
            .split("\n}")
            .next()
            .unwrap();
        for needle in ["ob_open", "ob_edit", "ob_armed", "ob_acknowledge"] {
            assert!(!body.contains(needle), "OB state leaked into Settings: {needle}");
        }
        assert!(source.contains("self.ob_acknowledge = false;"));
    }

    /// Chip ID must be reachable both as a standalone read and as an optional
    /// pre-flash check, and must be wired to the headless CLI as well.
    #[test]
    fn chip_id_reader_is_wired_into_the_workbench_and_cli() {
        let app = include_str!("app.rs");
        for needle in [
            "draw_chip_id_dialog",
            "Event::ChipId(snapshot)",
            "Job::ReadChipId",
            "read_chip_id_before_flash",
            "--id-open",
            "复制全部",
        ] {
            assert!(app.contains(needle), "missing chip-ID UI wiring: {needle}");
        }
        let backend_src = include_str!("backend.rs");
        for needle in [
            "fn collect_chip_id",
            "fn read_dpidr",
            "pub fn chip_id_cli",
            "cfg.read_chip_id_before_flash",
        ] {
            assert!(backend_src.contains(needle), "missing backend wiring: {needle}");
        }
        let main = include_str!("main.rs");
        assert!(main.contains("mod chip_id;"));
        assert!(main.contains("--chip-id"));
        assert!(main.contains("--id-addr"));
    }

    /// A read from a board that has since been unplugged must never be
    /// restored on the next launch, so none of this state lives in Settings.
    #[test]
    fn chip_id_snapshot_is_never_persisted() {
        let source = include_str!("app.rs");
        let body = source
            .split("struct Settings {")
            .nth(1)
            .unwrap()
            .split("\n}")
            .next()
            .unwrap();
        for needle in ["chip_id_open", "chip_id_snapshot", "chip_id_custom"] {
            assert!(!body.contains(needle), "chip-ID state leaked into Settings: {needle}");
        }
    }

    /// A loaded firmware used to be impossible to remove. Both panes now
    /// offer a close action that wipes every bit of loaded state, including
    /// the persisted path (otherwise it would come back on the next launch).
    #[test]
    fn firmware_can_be_unloaded_from_both_panes() {
        let source = include_str!("app.rs");
        let body = source
            .split("fn clear_firmware")
            .nth(1)
            .unwrap()
            .split("fn open_serial")
            .next()
            .unwrap();
        for needle in [
            "self.cfg.firmware.take()",
            "self.firmware = None",
            "self.firmware_view = None",
            "self.hex_source = HexSource::Image",
        ] {
            assert!(body.contains(needle), "clear_firmware must reset {needle}");
        }
        assert!(source.contains("fn has_firmware"));
        // Only a bare ✕ remains: the old text labels must not come back.
        assert!(!source.contains(concat!("✕ ", "移除")));
        assert!(!source.contains(concat!("✕ ", "关闭固件")));
        assert_eq!(source.matches(concat!("self.clear_", "firmware()")).count(), 2);
        // Recent entries are removable too, individually or all at once.
        assert!(source.contains("self.recent.clear()"));
    }

    /// The centre pane keeps only a bare ✕ and pins it to the top-right
    /// corner of the firmware info card, instead of showing a text button
    /// next to the page title.
    #[test]
    fn firmware_close_icon_sits_in_card_top_right() {
        let source = include_str!("app.rs");
        let icon = '\u{2715}';
        let viewer = source
            .split("fn draw_hex_viewer")
            .nth(1)
            .unwrap()
            .split("fn draw_serial_center")
            .next()
            .unwrap();
        let card_start = viewer.find(".fill(p.card)").unwrap();
        let header = &viewer[..card_start];
        let card = &viewer[card_start..];
        assert!(!header.contains(icon), "page header must not keep a close button");
        assert!(card.contains(icon), "close icon must be inside the info card");
        let first_row = card.split("ui.horizontal_wrapped").next().unwrap();
        assert!(
            first_row.contains("Layout::right_to_left"),
            "close icon must be right-aligned on the card's first row"
        );
        assert!(first_row.contains("add_enabled"));
    }

    /// The standalone serial info-strip card is gone: port/counters and the
    /// open / clear actions all moved onto the receive-pane header row.
    #[test]
    fn serial_receive_header_owns_the_info_strip_actions() {
        let source = include_str!("app.rs");
        let body = source
            .split("fn draw_serial_center")
            .nth(1)
            .unwrap()
            .split("\n    fn ")
            .next()
            .unwrap();
        assert!(!body.contains("(\"端口\", name.clone()"), "info strip card still rendered");
        assert!(body.contains("- 195.0 - send_extra"), "receive pane must reclaim the freed rows");
        let rx_start = body.find(".fill(p.console)").unwrap();
        let header = body[rx_start..].split("ui.separator()").next().unwrap();
        for needle in [
            "清空接收",
            "关闭串口",
            "打开串口",
            "RX {rx_total}",
            "serial_rx_hex",
            "Layout::right_to_left",
        ] {
            assert!(header.contains(needle), "receive header missing {needle}");
        }

        // The send pane uses the same layout: its old two action rows are
        // gone and every control is on the header row.
        assert!(!body.contains("horizontal_wrapped"), "old action rows still rendered");
        assert!(!body.contains("ui.add_space(4.0)"), "stray gap from the old send rows");
        let tx_start = body.find("\"发送区\"").unwrap();
        let tx_header = body[tx_start..].split("TextEdit::multiline").next().unwrap();
        for needle in [
            "清空输入",
            "追加 CRLF",
            "HEX 发送",
            "定时发送",
            "快捷输入",
            "Layout::right_to_left",
        ] {
            assert!(tx_header.contains(needle), "send header missing {needle}");
        }
        assert!(
            !tx_header.contains("primary_button"),
            "send must not sit on the header row"
        );
        // Chat-input style: a round vector send button in the bottom-right.
        for needle in [
            "circle_filled",
            "convex_polygon",
            "发送（Ctrl + Enter）",
        ] {
            assert!(body.contains(needle), "round send button missing {needle}");
        }
        assert!(
            body.contains("建议勾选 HEX 显示"),
            "garbled text must point at the HEX view"
        );
    }

    /// The most used action must not require scrolling, and there is exactly
    /// one button: duplicated controls are explicitly unwanted in this UI.
    #[test]
    fn flash_button_is_pinned_above_the_scroll_area() {
        let source = include_str!("app.rs");
        assert_eq!(source.matches(concat!("开始", "烧录")).count(), 1);
        let sidebar = source
            .split("fn sidebar_workbench")
            .nth(1)
            .unwrap()
            .split("fn session_connection")
            .next()
            .unwrap();
        let btn = sidebar.find(concat!("开始", "烧录")).unwrap();
        let scroll = sidebar.find("egui::ScrollArea::vertical()").unwrap();
        assert!(btn < scroll, "flash button must sit outside the scroll area");
        assert!(sidebar.contains("self.start_job(Job::Flash"));
    }

    /// Copied logs are plain, ordered and respect the DEBUG filter, so what
    /// lands in the clipboard matches what the console is showing.
    #[test]
    fn log_clipboard_text_matches_the_visible_console() {
        let logs = vec![
            LogEntry { time: "12:00:00".into(), level: Level::Info, msg: "连接成功".into() },
            LogEntry { time: "12:00:01".into(), level: Level::Debug, msg: "SWD 10000".into() },
            LogEntry { time: "12:00:02".into(), level: Level::Error, msg: "烧录失败".into() },
        ];
        assert_eq!(
            logs_to_text(&logs, false),
            "[12:00:00] [INFO ] 连接成功\n[12:00:02] [ERROR] 烧录失败"
        );
        assert!(logs_to_text(&logs, true).contains("[DEBUG] SWD 10000"));
        assert!(logs_to_text(&[], false).is_empty());
    }

    /// With `default-features = false`, eframe silently falls back to an
    /// app-local clipboard unless `clipboard` is enabled. Then Ctrl+C and
    /// every `ctx.copy_text()` only work *inside* the app, which is exactly
    /// the "can select but cannot copy" bug. Never drop this feature again.
    #[test]
    fn eframe_system_clipboard_feature_stays_enabled() {
        let manifest = include_str!("../Cargo.toml");
        let eframe_line = manifest
            .lines()
            .find(|l| l.starts_with("eframe = "))
            .expect("eframe dependency missing");
        assert!(eframe_line.contains("default-features = false"), "{eframe_line}");
        assert!(eframe_line.contains("\"clipboard\""), "{eframe_line}");
    }

    /// Selecting label text alone is not enough on a Windows-subsystem
    /// window: both log surfaces need explicit clipboard actions.
    #[test]
    fn log_panes_have_explicit_copy_actions() {
        let source = include_str!("app.rs");
        for needle in [
            "fn copy_all_logs",
            "fn logs_to_text",
            "fn log_text",
            "ctx.copy_text(text)",
            "复制整行",
            "只复制消息内容",
            "context_menu(|ui|",
        ] {
            assert!(source.contains(needle), "missing log copy wiring: {needle}");
        }
        assert_eq!(
            source.matches(concat!("self.copy_all_", "logs(ui.ctx())")).count(),
            2
        );
    }
}
