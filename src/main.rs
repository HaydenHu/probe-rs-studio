#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod firmware;
mod icons;
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
        let mut cfg = backend::FlashConfig::default();
        cfg.firmware = Some(std::path::PathBuf::from(firmware));
        if let Some(chip) = args.get(pos + 2).filter(|s| !s.starts_with("--")) {
            cfg.chip = chip.clone();
        }
        cfg.backup_path = std::path::PathBuf::from("backup_selftest.bin");
        let write = args.iter().any(|a| a == "--write");
        println!(
            "自检: {} · {} · {} · {}",
            cfg.firmware.as_ref().unwrap().display(),
            cfg.chip,
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
