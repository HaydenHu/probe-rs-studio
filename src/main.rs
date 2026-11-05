#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod firmware;
mod icons;
mod theme;

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
        .with_title("ProbeRS Studio · 固件烧录工作台")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([1040.0, 680.0])
        .with_drag_and_drop(true);
    viewport = viewport.with_icon(icons::window_icon());

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ProbeRS Studio",
        options,
        Box::new(|cc| {
            let app = app::App::new(cc);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}
