#[cfg(all(target_os = "linux", feature = "bridge"))]
mod avrcp;
#[cfg(all(target_os = "linux", feature = "bridge"))]
mod bluetooth;
#[cfg(all(target_os = "linux", feature = "bridge"))]
mod buttons;
mod cli;
mod config;
mod run;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::Config;
#[cfg(all(target_os = "linux", feature = "bridge"))]
use liba2::AirPlayClient;
use liba2::{Device, Discovery, ServiceBrowser};
use tracing::warn;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let file_config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Doctor(args) => doctor(args.airplay_match.or(file_config.airplay_match)).await,
        Command::Discover(args) => discover(args.timeout_secs).await,
        Command::Run(args) => {
            let run_config = file_config.merge_run_args(args);
            run::run(run_config).await
        }
    }
}

fn init_logging(verbose: u8) {
    let max_level = match verbose {
        0 => LevelFilter::INFO,
        1 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };

    let filter = Targets::default()
        .with_default(max_level)
        .with_target("mdns_sd", LevelFilter::ERROR);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact(),
        )
        .with(filter)
        .init();
}

async fn discover(timeout_secs: u64) -> Result<()> {
    let browser = ServiceBrowser::new().context("create AirPlay service browser")?;
    let devices = browser
        .scan(Duration::from_secs(timeout_secs))
        .await
        .context("discover AirPlay devices")?;

    if devices.is_empty() {
        println!("No AirPlay devices found.");
        return Ok(());
    }

    for device in devices {
        let addr = device
            .socket_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<no address>".to_string());
        let protocol = if device.supports_airplay2() {
            "AirPlay2"
        } else if device.supports_raop() {
            "RAOP"
        } else {
            "unknown"
        };
        println!("{}\t{}\t{}\t{}", device.name, device.model, addr, protocol);
    }

    Ok(())
}

async fn doctor(airplay_match: Option<String>) -> Result<()> {
    println!("a2a2 doctor");
    println!("target OS: {}", std::env::consts::OS);

    #[cfg(all(target_os = "linux", feature = "bridge"))]
    {
        use std::process::Command;

        use crate::bluetooth::{ComponentStatus, SystemSetup};

        let status = SystemSetup::check();
        println!("BlueZ:   {}", component_status(&status.bluez));
        println!("BlueALSA: {}", component_status(&status.bluealsa));

        if status.issues.is_empty() {
            println!("Bluetooth audio stack: ready");
        } else {
            println!("Bluetooth audio stack: not ready");
            for issue in status.issues {
                if let Some(fix) = issue.fix_command {
                    println!("  - {} ({})", issue.description, fix);
                } else {
                    println!("  - {}", issue.description);
                }
            }
        }

        match Command::new("bluetoothctl").arg("show").output() {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let controller = text
                    .lines()
                    .find(|line| line.trim_start().starts_with("Controller "))
                    .map(str::trim)
                    .unwrap_or("Controller <unknown>");
                let alias = text
                    .lines()
                    .find_map(|line| line.trim_start().strip_prefix("Alias: "))
                    .unwrap_or("<unknown>");
                println!("Bluetooth adapter: {controller} alias={alias}");
            }
            Ok(output) => {
                let error = String::from_utf8_lossy(&output.stderr);
                println!("Bluetooth adapter: {}", error.trim());
            }
            Err(error) => println!("Bluetooth adapter: {}", error),
        }

        fn component_status(status: &ComponentStatus) -> &'static str {
            match status {
                ComponentStatus::Ok => "ok",
                ComponentStatus::NotRunning => "installed, not running",
                ComponentStatus::NotInstalled => "not installed",
                ComponentStatus::Unknown => "unknown",
            }
        }
    }

    #[cfg(all(target_os = "linux", not(feature = "bridge")))]
    println!("Bluetooth bridge checks are disabled in this build; rebuild with --features bridge.");

    #[cfg(not(target_os = "linux"))]
    println!("Bluetooth bridge runtime is Linux-only; build/run it on the Raspberry Pi.");

    let browser = ServiceBrowser::new().context("create AirPlay service browser")?;
    match browser.scan(Duration::from_secs(3)).await {
        Ok(devices) => {
            if devices.is_empty() {
                println!("AirPlay discovery: no devices found");
            } else {
                println!("AirPlay discovery:");
                for device in &devices {
                    let selected = airplay_match
                        .as_ref()
                        .map(|m| device_matches(device, m))
                        .unwrap_or(false);
                    println!(
                        "  - {} ({}){}",
                        device.name,
                        device
                            .socket_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|| "no address".into()),
                        if selected { " [match]" } else { "" }
                    );
                }
            }
        }
        Err(error) => warn!(%error, "AirPlay discovery failed"),
    }

    Ok(())
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
pub(crate) enum AirPlayTarget {
    Single(Device),
    Group(Vec<Device>),
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
pub(crate) async fn select_airplay_target(
    client: &AirPlayClient,
    target: &str,
    timeout: Duration,
) -> Result<AirPlayTarget> {
    let devices = client
        .discover(timeout)
        .await
        .context("discover AirPlay devices")?;

    let mut matches = devices
        .into_iter()
        .filter(|device| device_matches(device, target))
        .collect::<Vec<_>>();

    match matches.len() {
        0 => Err(anyhow::anyhow!("no AirPlay device matched '{target}'")),
        1 => Ok(AirPlayTarget::Single(matches.remove(0))),
        _ => {
            if let Some(group) = collapse_group_matches(&matches) {
                return Ok(AirPlayTarget::Group(group));
            }

            let names = matches
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "AirPlay match '{target}' is ambiguous: {names}"
            ))
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn collapse_group_matches(devices: &[Device]) -> Option<Vec<Device>> {
    let group_id = devices.first()?.group_id?;
    if devices.len() < 2
        || devices
            .iter()
            .any(|device| device.group_id != Some(group_id))
    {
        return None;
    }

    let mut group = devices.to_vec();
    group.sort_by(|a, b| {
        b.is_group_leader
            .cmp(&a.is_group_leader)
            .then_with(|| a.name.cmp(&b.name))
    });
    Some(group)
}

fn device_matches(device: &Device, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    let addr_match = device
        .addresses
        .iter()
        .any(|addr| addr.to_string().contains(&needle));

    device.name.to_ascii_lowercase().contains(&needle)
        || device.model.to_ascii_lowercase().contains(&needle)
        || device
            .group_public_name
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains(&needle)
        || device
            .id
            .to_mac_string()
            .to_ascii_lowercase()
            .contains(&needle)
        || addr_match
}
