mod app;
mod charts;
mod engine;
mod icon;
mod theme;

use std::env;
use std::path::PathBuf;
use std::process;

use resguard::config;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut config_path: Option<PathBuf> = None;
    let mut start_active = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!(
                    "rs-dash — Resguard Dashboard\n\n\
                     USAGE:\n    rs-dash [OPTIONS]\n\n\
                     OPTIONS:\n    \
                     --active           Start in Active Guard mode (if no external daemon is running)\n    \
                     --config <PATH>    Load custom config.toml\n    \
                     -V, --version      Print version\n    \
                     -h, --help         Print help"
                );
                return;
            }
            "-V" | "--version" => {
                println!("rs-dash {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--export-icon-tga" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    eprintln!("--export-icon-tga requires a path");
                    process::exit(2);
                };
                if let Err(e) = icon::write_icon_tga(std::path::Path::new(p), 512) {
                    eprintln!("Failed to write icon: {}", e);
                    process::exit(1);
                }
                return;
            }
            "--active" => start_active = true,
            "--config" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    config_path = Some(PathBuf::from(p));
                } else {
                    eprintln!("--config requires a path");
                    process::exit(2);
                }
            }
            other => {
                eprintln!("Unknown argument: {} (try --help)", other);
                process::exit(2);
            }
        }
        i += 1;
    }

    let cfg = match config::load_config_or_default(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {}", e);
            process::exit(2);
        }
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("RS Dash — Resguard Dashboard")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([720.0, 520.0])
        .with_icon(icon::build_app_icon());

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "RS Dash",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::RsDashApp::new(cc, cfg, start_active)))),
    ) {
        eprintln!("Failed to launch RS Dash: {}", e);
        process::exit(1);
    }
}
