#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod firmware;
mod icons;
mod option_bytes;
mod pack;
mod serial;
mod theme;

use std::path::PathBuf;

/// OS window title (taskbar / Alt-Tab). The in-app top bar deliberately does
/// not repeat this string — see `Nav::title`.
pub const WINDOW_TITLE: &str = "ProbeRS Studio";

fn main() -> eframe::Result<()> {
    // Headless self-check: exercises the exact same library calls as the GUI.
    //   probe-rs-studio.exe --selftest <firmware.hex> [chip] [--write]
    // Without --write it only enumerates / connects / reads back / verifies.
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Unified headless entry: `--load <file>` mirrors the single GUI button.
    // Normalise the switch by file type *before* dispatch, so pack and FLM
    // keep sharing all of their existing conversion/import logic.
    if let Some(pos) = args.iter().position(|a| a == "--load") {
        let Some(file) = args.get(pos + 1).cloned() else {
            eprintln!(
                "用法: probe-rs-studio.exe --load <file.pack|file.pack.zip|file.FLM> [--out <yaml>]"
            );
            std::process::exit(2);
        };
        args[pos] = match pack::classify_source(std::path::Path::new(&file)) {
            Some(pack::ChipSource::Flm) => "--flm",
            Some(pack::ChipSource::Pack) => "--pack",
            None => {
                eprintln!(
                    "不是芯片支持文件: {file}（需要 .pack / .pack.zip / .FLM，或已解压的 pack 目录）"
                );
                std::process::exit(2);
            }
        }
        .to_owned();
    }

    if let Some(pos) = args.iter().position(|a| a == "--selftest") {
        let Some(firmware) = args.get(pos + 1) else {
            eprintln!("用法: probe-rs-studio.exe --selftest <固件路径> [芯片型号] [--write]");
            std::process::exit(2);
        };
        // Reuse the GUI's saved link settings (stable probe identity + SWD
        // speed). A headless run with the bare 1000 kHz default would attach
        // under different conditions and cannot reproduce UI flash failures.
        let mut cfg = saved_flash_config().unwrap_or_default();
        cfg.firmware = Some(std::path::PathBuf::from(firmware));
        if let Some(chip) = args.get(pos + 2).filter(|s| !s.starts_with("--")) {
            cfg.chip = chip.clone();
        }
        if let Some(speed) = cli_opt(&args, "--speed").and_then(|s| s.parse::<u32>().ok()) {
            cfg.speed_khz = speed;
        }
        cfg.backup_path = std::path::PathBuf::from("backup_selftest.bin");
        let write = args.iter().any(|a| a == "--write");
        println!(
            "自检: {} · {} · {} kHz · {} · {}",
            cfg.firmware.as_ref().unwrap().display(),
            cfg.chip,
            cfg.speed_khz,
            if write { "含写入" } else { "只读" },
            cfg.backup_path.display()
        );
        match backend::selftest(&cfg, write) {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                eprintln!("自检失败: {err:#}");
                std::process::exit(1);
            }
        }
    }

    // Headless chip-library check (no probe required).
    if args.iter().any(|a| a == "--chips") {
        match backend::chip_library() {
            Ok(summary) => {
                println!("[PASS] {summary}");
                std::process::exit(0);
            }
            Err(err) => {
                eprintln!("芯片库加载失败: {err:#}");
                std::process::exit(1);
            }
        }
    }

    // Headless serial-port listing (no probe required): --ports
    if args.iter().any(|a| a == "--ports") {
        let ports = serial::enumerate();
        for port in &ports {
            println!(
                "{:<8} {:<14} VID:PID={:<11} IF={:<5} {} · {}",
                port.name,
                port.kind.label(),
                port.vid_pid().unwrap_or_else(|| "—".to_owned()),
                port.interface
                    .map(|i| format!("MI_{i:02}"))
                    .unwrap_or_else(|| "—".to_owned()),
                if port.openable { "可打开" } else { "被占用/不可用" },
                port.description
            );
        }
        println!(
            "共 {} 个在线串口 · 调试器虚拟串口 {} 个",
            ports.len(),
            ports.iter().filter(|p| p.is_debug_vcp()).count()
        );
        std::process::exit(0);
    }

    // Headless, READ-ONLY option-byte inspection:
    //   --option-bytes [chip] [--ob-base 0x1FFF0E80]
    if let Some(pos) = args.iter().position(|a| a == "--option-bytes") {
        // Reuse the GUI's saved link settings (stable probe identity + SWD
        // speed). The bare defaults can pick a different probe/speed and make
        // a headless read behave differently from the UI.
        let mut cfg = saved_flash_config().unwrap_or_default();
        if let Some(chip) = args.get(pos + 1).filter(|s| !s.starts_with("--")) {
            cfg.chip = chip.clone();
        }
        if let Some(speed) = cli_opt(&args, "--speed").and_then(|s| s.parse::<u32>().ok()) {
            cfg.speed_khz = speed;
        }
        let profile = option_bytes::profile_for_chip(&cfg.chip);
        let base = cli_opt(&args, "--ob-base")
            .and_then(|s| option_bytes::parse_base(&s))
            .unwrap_or(profile.base);
        println!(
            "选项字节只读自检: {} · {} · 0x{base:08X}",
            cfg.chip, profile.label
        );
        backend::option_bytes_cli(&cfg, base);
        std::process::exit(0);
    }

    // Headless TX/RX loopback check: --loopback <COMx> [--baud N] [--text ...]
    // Uses the very same Port/read/write path as the GUI serial terminal, so
    // it separates "wire/driver is fine" from "GUI wiring is broken".
    if let Some(pos) = args.iter().position(|a| a == "--loopback") {
        let Some(name) = args.get(pos + 1) else {
            eprintln!("用法: probe-rs-studio.exe --loopback <COMx> [--baud N] [--text 内容]");
            std::process::exit(2);
        };
        let baud = cli_opt(&args, "--baud")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(115200);
        let text = cli_opt(&args, "--text")
            .unwrap_or_else(|| "Hello\\r\\n".to_owned())
            .replace("\\r", "\r")
            .replace("\\n", "\n")
            .replace("\\0", "\0");
        let cfg = serial::SerialSettings {
            baud,
            ..Default::default()
        };
        std::process::exit(serial_loopback(name, cfg, text.as_bytes()));
    }

    // Headless CMSIS-Pack conversion: probe-rs-studio.exe --load|--pack <file> [--out <yaml>]
    if let Some(pos) = args.iter().position(|a| a == "--pack") {
        let Some(file) = args.get(pos + 1) else {
            eprintln!("用法: probe-rs-studio.exe --load|--pack <file.pack|file.pack.zip> [--out <yaml>]");
            std::process::exit(2);
        };
        let path = std::path::PathBuf::from(file);
        match pack::convert(&path) {
            Ok(converted) => {
                println!("系列: {}", converted.family);
                if !converted.release.is_empty() {
                    println!("版本: {}", converted.release);
                }
                println!(
                    "型号 ({}): {}",
                    converted.variants.len(),
                    converted.variants.join(", ")
                );
                println!(
                    "Flash 算法 ({}): {}",
                    converted.algorithms.len(),
                    converted.algorithms.join(", ")
                );
                let out = args
                    .iter()
                    .position(|a| a == "--out")
                    .and_then(|i| args.get(i + 1))
                    .map(std::path::PathBuf::from);
                let dest = match out {
                    Some(p) => match std::fs::write(&p, &converted.yaml) {
                        Ok(()) => p,
                        Err(err) => {
                            eprintln!("写出 YAML 失败: {err}");
                            std::process::exit(1);
                        }
                    },
                    None => {
                        // Exercise the exact same import path as the GUI button
                        // (validate with probe-rs, persist, refresh library).
                        match backend::import_pack(&path) {
                            Ok(summary) => println!("导入结果: {summary}"),
                            Err(err) => {
                                eprintln!("导入芯片包失败: {err:#}");
                                std::process::exit(1);
                            }
                        }
                        println!("YAML: {} 字节", converted.yaml.len());
                        std::process::exit(0);
                    }
                };
                println!("芯片描述: {}", dest.display());
                println!("YAML: {} 字节", converted.yaml.len());
                std::process::exit(0);
            }
            Err(err) => {
                eprintln!("CMSIS-Pack 转换失败: {err:#}");
                std::process::exit(1);
            }
        }
    }

    // Headless standalone .FLM import:
    //   --flm <file> [--chip NAME] [--family NAME] [--core M4]
    //                [--flash-base 0x08000000] [--flash-size 0x40000]
    //                [--ram-base 0x20000000] [--ram-size 0x10000]
    //                [--load-address 0x20000400] [--ap 0] [--out <yaml>]
    if let Some(pos) = args.iter().position(|a| a == "--flm") {
        match flm_cli(&args, pos) {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                eprintln!("导入 .FLM 失败: {err}");
                std::process::exit(1);
            }
        }
    }

    // Headless hot-plug diagnostic: probe-rs-studio.exe --watch <seconds>
    if let Some(pos) = args.iter().position(|a| a == "--watch") {
        let secs = args
            .get(pos + 1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(8);
        backend::watch_cli(std::time::Duration::from_secs(secs));
        std::process::exit(0);
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([1040.0, 680.0])
        .with_drag_and_drop(true);
    viewport = viewport.with_icon(icons::window_icon());

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| {
            let app = app::App::new(cc);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

/// `--loopback <COMx>`: write a short payload and read it back through the
/// real serial I/O path. Short the port's TX and RX pins before running this.
fn serial_loopback(name: &str, cfg: serial::SerialSettings, payload: &[u8]) -> i32 {
    use std::time::{Duration, Instant};
    println!("回环自检: {name} · {} · {} 字节", cfg.label(), payload.len());
    let port = match serial::open_port(name, cfg) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("打开失败: {err:#}");
            return 1;
        }
    };
    println!("打开成功 · 初始队列 {:?}", port.queue_status());
    match port.write(payload) {
        Ok(n) => println!("TX {n} 字节: {payload:02X?}"),
        Err(err) => {
            eprintln!("写入失败: {err:#}");
            return 1;
        }
    }
    let mut rx = Vec::new();
    let started = Instant::now();
    let mut polls = 0u32;
    let mut max_queued = 0u32;
    while started.elapsed() < Duration::from_millis(1500) {
        polls += 1;
        if let Ok((_, in_que, _)) = port.queue_status() {
            max_queued = max_queued.max(in_que);
        }
        match port.read() {
            Ok(bytes) if !bytes.is_empty() => {
                println!(
                    "  +{:>4}ms  RX {} 字节 {bytes:02X?}",
                    started.elapsed().as_millis(),
                    bytes.len()
                );
                rx.extend_from_slice(&bytes);
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("读取失败: {err:#}");
                return 1;
            }
        }
        if rx.len() >= payload.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    println!(
        "轮询 {polls} 次 · 驱动最大 cbInQue={max_queued} · 共收到 {} 字节: {:?}",
        rx.len(),
        String::from_utf8_lossy(&rx)
    );
    println!("末次队列 {:?}", port.queue_status());
    if rx == payload {
        println!("[PASS] 回环数据与发送内容一致");
        0
    } else {
        println!("[FAIL] 未收到完整回环数据（检查 TX/RX 短接、所选端口与波特率）");
        1
    }
}

/// Load the GUI's persisted link settings next to the executable, so headless
/// commands attach to exactly the same probe with the same SWD speed.
fn saved_flash_config() -> Option<backend::FlashConfig> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("probe-rs-studio.json");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    serde_json::from_value(value.get("config")?.clone()).ok()
}

/// Fetch a `--key value` pair from the argument list.
fn cli_opt(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

fn cli_u64(args: &[String], key: &str) -> Result<Option<u64>, String> {
    match cli_opt(args, key) {
        None => Ok(None),
        Some(v) => pack::parse_number(&v)
            .map(Some)
            .ok_or_else(|| format!("{key} 不是有效的数字: {v}")),
    }
}

/// `--flm <file>`: build a chip family from a bare Flash algorithm. Everything
/// the FLM declares is taken from the file; the chip name, core and RAM layout
/// come from CLI options (with conventional defaults).
fn flm_cli(args: &[String], pos: usize) -> Result<(), String> {
    const USAGE: &str = "用法: probe-rs-studio.exe --flm <file.FLM> [--chip NAME] [--family NAME] \
        [--core M4] [--flash-base 0x08000000] [--flash-size 0x40000] \
        [--ram-base 0x20000000] [--ram-size 0x10000] [--out <yaml>]";
    let file = args.get(pos + 1).ok_or_else(|| USAGE.to_owned())?;
    let path = PathBuf::from(file);
    let info = pack::inspect_flm(&path).map_err(|e| format!("{e:#}"))?;
    println!("算法参数: {}", info.summary());
    println!(
        "算法链接地址: {:#x}（位置无关算法可忽略）",
        info.code_load_address
    );

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("FLM_CHIP")
        .to_owned();
    let chips: Vec<String> = cli_opt(args, "--chip")
        .unwrap_or_else(|| stem.clone())
        .split([',', ';'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let ap = match cli_u64(args, "--ap")? {
        Some(v) if v <= u8::MAX as u64 => v as u8,
        Some(v) => return Err(format!("--ap 超出 0..255: {v}")),
        None => 0,
    };
    let spec = pack::FlmSpec {
        family: cli_opt(args, "--family").unwrap_or_else(|| format!("{stem} Series")),
        chips,
        core: cli_opt(args, "--core").unwrap_or_else(|| "armv7em".to_owned()),
        flash_base: cli_u64(args, "--flash-base")?,
        flash_size: cli_u64(args, "--flash-size")?,
        ram_base: cli_u64(args, "--ram-base")?.unwrap_or(0x2000_0000),
        ram_size: cli_u64(args, "--ram-size")?.unwrap_or(0x0001_0000),
        load_address: cli_u64(args, "--load-address")?,
        ap,
    };

    // Convert once for the summary; `--out` writes exactly this document,
    // otherwise the GUI-equivalent import path validates + installs it.
    let converted = pack::convert_flm(&path, &spec).map_err(|e| format!("{e:#}"))?;
    println!(
        "生成 {} 个型号 / 算法 {}: {}",
        converted.variants.len(),
        converted.algorithm,
        converted.variants.join(", ")
    );
    match cli_opt(args, "--out") {
        Some(out) => {
            std::fs::write(&out, &converted.yaml)
                .map_err(|e| format!("写出 {out} 失败: {e}"))?;
            println!("芯片描述: {out}");
        }
        None => {
            let summary = backend::import_flm(&path, &spec).map_err(|e| format!("{e:#}"))?;
            println!("导入结果: {summary}");
        }
    }
    println!("YAML: {} 字节", converted.yaml.len());
    Ok(())
}
