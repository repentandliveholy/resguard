use std::process;

use resguard::config::{self, CliAction};
use resguard::{service, updater};

fn main() {
    match config::parse_args_and_load() {
        Ok(CliAction::ExitOk) => {}
        Ok(CliAction::Run(cfg)) | Ok(CliAction::RunTui(cfg)) => {
            if let Err(e) = resguard::tui::run_tui(cfg) {
                eprintln!("rs-tui error: {}", e);
                process::exit(1);
            }
        }
        Ok(CliAction::CheckUpdate) => match updater::check_latest_version() {
            Ok(info) => {
                if info.update_available {
                    println!(
                        "Update available: {} -> {} (run `resguard --update`)",
                        info.current_version, info.latest_version
                    );
                } else {
                    println!("rs-tui {} is up to date.", info.current_version);
                }
            }
            Err(e) => {
                eprintln!("Update check failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::SelfUpdate { force }) => match updater::perform_self_update(force) {
            Ok(updater::UpdateOutcome::UpToDate(v)) => println!("Up to date ({})", v),
            Ok(updater::UpdateOutcome::Updated { from, to }) => {
                println!("Updated {} -> {}", from, to)
            }
            Err(e) => {
                eprintln!("Update failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::InstallService) => match service::install_service() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service install failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::UninstallService) => match service::uninstall_service() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service uninstall failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::ServiceStatus) => match service::service_status() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service status failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::Restore) => match resguard::runtime::restore_cli() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Restore failed: {}", e);
                process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(2);
        }
    }
}
