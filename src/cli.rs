use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[arg(short, long)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Doctor(DoctorArgs),
    Discover(DiscoverArgs),
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long, env = "A2A2_AIRPLAY_MATCH")]
    pub airplay_match: Option<String>,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,
}

#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    #[arg(long, env = "A2A2_AIRPLAY_MATCH")]
    pub airplay_match: Option<String>,

    #[arg(long, env = "A2A2_AIRPLAY_PIN")]
    pub pin: Option<String>,

    #[arg(long, env = "A2A2_BT_ALIAS")]
    pub bt_alias: Option<String>,

    #[arg(long, env = "A2A2_BT_CLASS", value_parser = parse_bt_class)]
    pub bt_class: Option<u32>,

    #[arg(long)]
    pub discoverable: bool,

    #[arg(long)]
    pub no_discoverable: bool,

    #[arg(long)]
    pub hd_audio: bool,

    #[arg(long)]
    pub standard_audio: bool,

    #[arg(long)]
    pub render_delay_ms: Option<u32>,

    #[arg(long)]
    pub airplay_discovery_secs: Option<u64>,

    #[arg(long)]
    pub bluetooth_wait_secs: Option<u64>,
}

fn parse_bt_class(value: &str) -> Result<u32, String> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|error| format!("invalid Bluetooth class: {error}"))
    } else {
        value
            .parse()
            .map_err(|error| format!("invalid Bluetooth class: {error}"))
    }
}
