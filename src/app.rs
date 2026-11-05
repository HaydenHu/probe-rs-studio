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
    theme::{self, Palette, ThemeMode},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    Workbench,
    Devices,
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
            Nav::Devices => "调试探针",
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Settings {
    config: FlashConfig,
    theme: ThemeMode,
    #[serde(default = "default_true")]
    auto_detect: bool,
    recent: Vec<PathBuf>,
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
    chip_search: String,
    chip_details: String,
    firmware: Option<FirmwareInfo>,
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
        let s = saved.unwrap_or_else(|| Settings {
            config: FlashConfig::default(),
            theme: ThemeMode::Dark,
            auto_detect: true,
            recent: vec![],
            addr_text: "0x08000000".to_owned(),
            size_text: "4096".to_owned(),
            bin_base_text: "0x08000000".to_owned(),
            speed_text: "1000".to_owned(),
            chip_search: String::new(),
        });
        theme::apply(&cc.egui_ctx, s.theme);

        let (tx, rx) = channel();
        let watch = Arc::new(WatcherControl::new(s.auto_detect));
        let repaint_ctx = cc.egui_ctx.clone();
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
            chip_search: s.chip_search,
            chip_details: String::new(),
            firmware: None,
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
            watch,
            Duration::from_millis(1500),
            move || repaint_ctx.request_repaint(),
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

    fn probe_ready(&self) -> bool {
        self.cfg
            .probe_id
            .as_deref()
            .is_some_and(|id| self.probes.iter().any(|x| x.id == id))
    }

    fn can_run(&self) -> bool {
        !self.busy && self.probe_ready()
    }

    fn select_probe(&mut self, i: usize) {
        if let Some(item) = self.probes.get(i) {
            self.cfg.probe_index = i;
            self.cfg.probe_id = Some(item.id.clone());
            self.connection = None;
        }
    }

    fn snapshot(&self) -> Settings {
        Settings {
            config: self.cfg.clone(),
            theme: self.theme,
            auto_detect: self.auto_detect,
            recent: self.recent.clone(),
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

    fn start_job(&mut self, job: Job, label: &str) {
        if self.busy {
            self.push_log(Level::Warn, "上一个任务尚未结束，请稍候");
            return;
        }
        self.sync_config();
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
        match firmware::inspect(&path, format, self.cfg.bin_base) {
            Ok(info) => {
                self.push_log(
                    Level::Ok,
                    format!("固件已载入: {} ({} 字节)", path.display(), info.size),
                );
                self.firmware = Some(info);
            }
            Err(e) => {
                self.push_log(Level::Error, format!("解析固件失败: {e:#}"));
                self.firmware = None;
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
                        (Nav::Devices, IconKind::Devices, "设备 / 调试探针"),
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
                Nav::Devices => self.sidebar_devices(ui, p),
                Nav::Chips => self.sidebar_chips(ui, p),
                Nav::Logs => self.sidebar_logs(ui, p),
                Nav::Settings => self.sidebar_settings(ui, p),
            });
    }

    fn sidebar_workbench(&mut self, ui: &mut Ui, p: Palette) {
        ui.label(RichText::new("当前会话").size(13.0).strong().color(p.text));
        ui.add_space(10.0);
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.label(RichText::new("调试链路").size(11.0).color(p.text3));
                ui.add_space(6.0);
                let probe = self
                    .probes
                    .get(self.cfg.probe_index)
                    .map(|x| x.label.as_str())
                    .unwrap_or("尚未检测到探针");
                ui.label(RichText::new(probe).size(12.0).color(p.text));
                ui.label(
                    RichText::new(format!("{} · {} · {} kHz", self.cfg.chip, proto_label(self.cfg.protocol), self.cfg.speed_khz))
                        .size(11.0)
                        .color(p.text2),
                );
                ui.add_space(6.0);
                let (ok, color) = match &self.connection {
                    Some(_) => ("已连接", p.success),
                    None => ("未连接", p.text3),
                };
                ui.label(RichText::new(format!("● {ok}")).size(11.0).color(color));
            });
        ui.add_space(16.0);
        ui.label(RichText::new("最近固件").size(13.0).strong().color(p.text));
        ui.add_space(8.0);
        if self.recent.is_empty() {
            ui.label(RichText::new("暂无记录").size(11.0).color(p.text3));
        }
        let recent = self.recent.clone();
        for path in recent {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ui
                .add(egui::Button::new(RichText::new(name).color(p.text2)).fill(Color32::TRANSPARENT))
                .on_hover_text(path.display().to_string())
                .clicked()
            {
                self.set_firmware(path);
            }
        }
        ui.add_space(18.0);
        ui.label(RichText::new("快捷操作").size(13.0).strong().color(p.text));
        ui.add_space(8.0);
        if ui.button("刷新探针").clicked() {
            self.start_job(Job::RefreshProbes, "刷新探针");
        }
        if ui.button("查看日志").clicked() {
            self.nav = Nav::Logs;
        }
    }

    fn sidebar_devices(&mut self, ui: &mut Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("调试探针").size(13.0).strong().color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    self.start_job(Job::RefreshProbes, "刷新探针");
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::hover());
            ui.painter().circle_filled(
                dot.center(),
                3.5,
                if self.auto_detect { p.success } else { p.text3 },
            );
            ui.label(
                RichText::new(if self.auto_detect {
                    "自动检测中 · 插入/拔出自动刷新 (1.5s)"
                } else {
                    "自动检测已关闭 · 请手动刷新"
                })
                .size(10.0)
                .color(p.text3),
            );
        });
        if let Some(t) = &self.last_hotplug {
            ui.label(RichText::new(format!("上次变化 {t}")).size(10.0).color(p.text3));
        }
        ui.add_space(10.0);
        if self.probes.is_empty() {
            ui.label(RichText::new("未发现探针，请检查 USB 连接").size(11.0).color(p.text3));
        } else if !self.probe_ready() {
            ui.label(
                RichText::new("所选探针已断开，请在下方重新选择")
                    .size(11.0)
                    .color(p.warn),
            );
        }
        let probes = self.probes.clone();
        for (i, item) in probes.iter().enumerate() {
            let selected = self.cfg.probe_id.as_deref() == Some(item.id.as_str());
            Frame::new()
                .fill(if selected { p.accent_soft } else { p.card })
                .stroke(Stroke::new(1.0, if selected { p.accent } else { p.border }))
                .corner_radius(9.0)
                .inner_margin(10.0)
                .show(ui, |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new(&item.label).color(p.text).size(12.0)).fill(Color32::TRANSPARENT))
                        .clicked()
                    {
                        self.select_probe(i);
                    }
                    ui.label(RichText::new(&item.detail).size(10.0).color(p.text3));
                });
            ui.add_space(6.0);
        }
        ui.add_space(16.0);
        ui.label(RichText::new("故障提示").size(12.0).strong().color(p.text));
        ui.label(
            RichText::new("若所有 DAP 命令超时，多为调试器固件卡死：物理拔插 USB 后重试。")
                .size(11.0)
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
        let needle = self.chip_search.trim().to_ascii_lowercase();
        let filtered: Vec<&ChipItem> = if needle.is_empty() {
            self.chips.iter().collect()
        } else {
            self.chips
                .iter()
                .filter(|c| c.name.to_ascii_lowercase().contains(&needle))
                .collect()
        };
        ui.label(
            RichText::new(format!("匹配 {} / {} 个型号", filtered.len(), self.chips.len()))
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
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, 34.0, filtered.len(), |ui, range| {
                for i in range {
                    let c = filtered[i];
                    let selected = self.cfg.chip.eq_ignore_ascii_case(&c.name);
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
                            let family = if c.external {
                                format!("{} · 扩展包", c.family)
                            } else {
                                c.family.clone()
                            };
                            ui.label(
                                RichText::new(family)
                                    .size(9.0)
                                    .color(if c.external { p.accent } else { p.text3 }),
                            );
                            if r.clicked() {
                                self.cfg.chip = c.name.clone();
                                self.chip_details.clear();
                                let tx = self.tx.clone();
                                backend::spawn(
                                    Job::ChipInfo {
                                        chip: c.name.clone(),
                                        cfg: Box::new(self.cfg.clone()),
                                    },
                                    tx,
                                );
                            }
                        });
                }
            });
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
        ui.checkbox(&mut self.auto_detect, "自动检测调试探针插入 / 拔出");
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
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new("连接与烧录")
                                .size(22.0)
                                .strong()
                                .color(p.text),
                        );
                        ui.label(
                            RichText::new("选择探针与目标芯片，载入固件，然后一键写入并校验")
                                .size(12.0)
                                .color(p.text3),
                        );
                        ui.add_space(16.0);

                        // --- Connection card ---
                        Self::card(ui, p, "连接配置", |ui| {
                            egui::Grid::new("conn_grid")
                                .num_columns(4)
                                .spacing([12.0, 10.0])
                                .show(ui, |ui| {
                                    ui.label(RichText::new("调试探针").size(11.0).color(p.text3));
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
                                    egui::ComboBox::from_id_salt("probe_combo")
                                        .selected_text(RichText::new(selected).color(p.text))
                                        .width(260.0)
                                        .show_ui(ui, |ui| {
                                            for (i, item) in self.probes.clone().iter().enumerate() {
                                                let prev = self.cfg.probe_index;
                                                ui.selectable_value(
                                                    &mut self.cfg.probe_index,
                                                    i,
                                                    item.label.clone(),
                                                );
                                                if self.cfg.probe_index != prev
                                                    && self.cfg.probe_index == i
                                                {
                                                    self.cfg.probe_id = Some(item.id.clone());
                                                    self.connection = None;
                                                }
                                            }
                                        });
                                    ui.label(RichText::new("协议").size(11.0).color(p.text3));
                                    egui::ComboBox::from_id_salt("proto_combo")
                                        .selected_text(proto_label(self.cfg.protocol))
                                        .width(90.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut self.cfg.protocol, Protocol::Swd, "SWD");
                                            ui.selectable_value(&mut self.cfg.protocol, Protocol::Jtag, "JTAG");
                                        });
                                    ui.end_row();

                                    ui.label(RichText::new("目标芯片").size(11.0).color(p.text3));
                                    ui.add_sized(
                                        [260.0, 26.0],
                                        egui::TextEdit::singleline(&mut self.cfg.chip)
                                            .hint_text("如 PY32F003x8"),
                                    );
                                    ui.label(RichText::new("速度 (kHz)").size(11.0).color(p.text3));
                                    ui.add_sized(
                                        [90.0, 26.0],
                                        egui::TextEdit::singleline(&mut self.speed_text),
                                    );
                                    ui.end_row();
                                });
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.cfg.connect_under_reset, "复位下连接");
                                ui.add_space(12.0);
                                if Self::ghost_button(ui, p, "连接测试", self.can_run()).clicked() {
                                    self.start_job(Job::Connect(Box::new(self.cfg.clone())), "连接测试");
                                }
                                if Self::ghost_button(ui, p, "芯片详情", !self.busy).clicked() {
                                    let chip = self.cfg.chip.clone();
                                    backend::spawn(
                                        Job::ChipInfo {
                                            chip,
                                            cfg: Box::new(self.cfg.clone()),
                                        },
                                        self.tx.clone(),
                                    );
                                    self.nav = Nav::Chips;
                                }
                                if !self.chip_details.is_empty() {
                                    ui.label(RichText::new("已读取描述").size(10.0).color(p.success));
                                }
                            });
                            if !self.chip_details.is_empty() {
                                ui.add_space(10.0);
                                Frame::new()
                                    .fill(p.card_alt)
                                    .corner_radius(8.0)
                                    .inner_margin(10.0)
                                    .show(ui, |ui| {
                                        for line in self.chip_details.lines() {
                                            ui.label(
                                                RichText::new(line).monospace().size(10.5).color(p.text2),
                                            );
                                        }
                                    });
                            }
                        });
                        ui.add_space(14.0);

                        // --- Firmware card ---
                        Self::card(ui, p, "固件文件", |ui| {
                            let (_, drop_rect) =
                                ui.allocate_space(Vec2::new(ui.available_width(), 64.0));
                            ui.painter().rect_filled(
                                drop_rect,
                                CornerRadius::same(10),
                                p.card_alt,
                            );
                            let text = self
                                .cfg
                                .firmware
                                .as_ref()
                                .map(|x| x.display().to_string())
                                .unwrap_or_else(|| "拖拽 .hex / .elf / .bin 到此处，或点击「浏览」".to_owned());
                            ui.painter().text(
                                drop_rect.center(),
                                Align2::CENTER_CENTER,
                                text,
                                FontId::proportional(12.0),
                                p.text2,
                            );
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                if Self::ghost_button(ui, p, "浏览…", true).clicked() {
                                    if let Some(path) = with_all_files(rfd::FileDialog::new())
                                        .add_filter("固件", &["hex", "ihex", "elf", "axf", "out", "bin"])
                                        .pick_file()
                                    {
                                        self.set_firmware(path);
                                    }
                                }
                                ui.label(RichText::new("格式").size(11.0).color(p.text3));
                                let before = self.cfg.format;
                                egui::ComboBox::from_id_salt("format_combo")
                                    .selected_text(self.cfg.format.label())
                                    .width(120.0)
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
                                ui.add_space(8.0);
                                ui.label(RichText::new("BIN 基址").size(11.0).color(p.text3));
                                ui.add_sized(
                                    [110.0, 26.0],
                                    egui::TextEdit::singleline(&mut self.bin_base_text),
                                );
                            });
                            if let Some(info) = &self.firmware {
                                ui.add_space(10.0);
                                Frame::new()
                                    .fill(p.card_alt)
                                    .corner_radius(8.0)
                                    .inner_margin(10.0)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {} 字节 · {} 条记录",
                                                info.format.label(),
                                                info.size,
                                                info.records
                                            ))
                                            .monospace()
                                            .size(11.0)
                                            .color(p.text2),
                                        );
                                        ui.label(
                                            RichText::new(&info.note).monospace().size(10.5).color(p.text3),
                                        );
                                        ui.label(
                                            RichText::new(format!("SHA256  {}", info.sha256))
                                                .monospace()
                                                .size(10.0)
                                                .color(p.text3),
                                        );
                                    });
                            }
                        });
                        ui.add_space(14.0);

                        // --- Options + actions card ---
                        Self::card(ui, p, "烧录选项与操作", |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.checkbox(&mut self.cfg.verify, "写入后校验");
                                ui.checkbox(&mut self.cfg.restore_unwritten, "保留未写入字节");
                                ui.checkbox(&mut self.cfg.verify_readback, "独立读回比对");
                                ui.checkbox(&mut self.cfg.reset_after, "完成后复位运行");
                                ui.checkbox(&mut self.cfg.preverify, "预校验跳过相同区域");
                                ui.checkbox(&mut self.cfg.chip_erase, "整片擦除");
                            });
                            ui.add_space(12.0);
                            ui.horizontal_wrapped(|ui| {
                                if Self::primary_button(ui, p, "开始烧录", self.can_run() && self.firmware.is_some()).clicked()
                                {
                                    self.start_job(Job::Flash(Box::new(self.cfg.clone())), "烧录固件");
                                }
                                if Self::ghost_button(ui, p, "独立校验", self.can_run() && self.firmware.is_some()).clicked()
                                {
                                    self.start_job(
                                        Job::VerifyFirmware(Box::new(self.cfg.clone())),
                                        "读回校验",
                                    );
                                }
                                if Self::ghost_button(ui, p, "读回备份", self.can_run()).clicked() {
                                    self.start_job(Job::ReadBack(Box::new(self.cfg.clone())), "读回备份");
                                }
                                if Self::ghost_button(ui, p, "复位运行", self.can_run()).clicked() {
                                    self.start_job(Job::Reset(Box::new(self.cfg.clone())), "复位目标");
                                }
                                let erase_text = if self.erase_armed { "确认擦除？" } else { "整片擦除" };
                                if Self::ghost_button(ui, p, erase_text, self.can_run()).clicked() {
                                    if self.erase_armed {
                                        self.erase_armed = false;
                                        self.start_job(Job::EraseAll(Box::new(self.cfg.clone())), "整片擦除");
                                    } else {
                                        self.erase_armed = true;
                                        self.push_log(Level::Warn, "整片擦除需二次点击确认，会清除芯片全部 Flash");
                                    }
                                }
                            });
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("读回地址").size(11.0).color(p.text3));
                                ui.add_sized([110.0, 26.0], egui::TextEdit::singleline(&mut self.addr_text));
                                ui.add_space(10.0);
                                ui.label(RichText::new("长度 (字节)").size(11.0).color(p.text3));
                                ui.add_sized([90.0, 26.0], egui::TextEdit::singleline(&mut self.size_text));
                                ui.add_space(10.0);
                                ui.label(
                                    RichText::new(format!("→ {}", self.cfg.backup_path.display()))
                                        .size(10.0)
                                        .color(p.text3),
                                );
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
                        ui.add_space(14.0);

                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("运行日志固定在底部面板，可拖拽上边缘调整高度")
                                .size(10.0)
                                .color(p.text3),
                        );
                        ui.add_space(8.0);
                    });
            });
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

    /// Regression guard: the OS title bar and the in-app top bar used to both
    /// print "ProbeRS Studio · 固件烧录工作台", producing a duplicate heading.
    #[test]
    fn top_bar_never_repeats_the_window_title() {
        let pages = [
            Nav::Workbench,
            Nav::Devices,
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
        // One helper definition + chip support, chip description, firmware,
        // log export and backup path = 6 occurrences.
        assert_eq!(source.matches(helper).count(), 6);
        let open_first = concat!("with_all_", "files(rfd::FileDialog::new())");
        assert_eq!(source.matches(open_first).count(), 3);
        assert!(source.contains(concat!("全部文件 (*.", "*)")));
    }
}
