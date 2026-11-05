#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod firmware;
mod icons;
mod pack;
mod theme;

/// OS window title (taskbar / Alt-Tab). The in-app top bar deliberately does
/// not repeat this string — see `Nav::title`.
pub const WINDOW_TITLE: &str = "ProbeRS Studio";

fn main() -> eframe::Result<()> {
    // Headless self-check: exercises the exact same library calls as the GUI.
    //   probe-rs-studio.exe --selftest <firmware.hex> [chip] [--write]
    // Without --write it only enumerates / connects / reads back / verifies.
    let args: Vec<String> = std::env::args().skip(1).collect();
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

    // Headless CMSIS-Pack conversion: probe-rs-studio.exe --pack <file> [--out <yaml>]
    if let Some(pos) = args.iter().position(|a| a == "--pack") {
        let Some(file) = args.get(pos + 1) else {
            eprintln!("用法: probe-rs-studio.exe --pack <file.pack|file.pack.zip> [--out <yaml>]");
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
