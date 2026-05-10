#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy)]
pub enum MediaCommand {
    Play,
    Pause,
    Stop,
    Volume(f32),
}

pub fn spawn_monitor(tx: mpsc::UnboundedSender<MediaCommand>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = monitor_dbus(tx).await {
            warn!(%error, "AVRCP monitor stopped");
        }
    })
}

async fn monitor_dbus(tx: mpsc::UnboundedSender<MediaCommand>) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("dbus-monitor")
        .args([
            "--system",
            "type='signal',sender='org.bluez',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let Some(stdout) = child.stdout.take() else {
        return Ok(());
    };

    info!("listening for BlueZ AVRCP/media property changes");

    let mut parser = DbusMonitorParser::default();
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(command) = parser.push_line(line.trim()) {
            debug!(?command, "media command");
            let _ = tx.send(command);
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct DbusMonitorParser {
    interface: Option<Interface>,
    pending_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interface {
    MediaTransport,
    MediaPlayer,
}

impl DbusMonitorParser {
    fn push_line(&mut self, line: &str) -> Option<MediaCommand> {
        if line.starts_with("signal ") {
            self.interface = None;
            self.pending_key = None;
            return None;
        }

        if let Some(value) = dbus_string(line) {
            match value {
                "org.bluez.MediaTransport1" => {
                    self.interface = Some(Interface::MediaTransport);
                    self.pending_key = None;
                    return None;
                }
                "org.bluez.MediaPlayer1" => {
                    self.interface = Some(Interface::MediaPlayer);
                    self.pending_key = None;
                    return None;
                }
                "Volume" | "State" | "Status" => {
                    self.pending_key = Some(value.to_string());
                    return None;
                }
                _ => {
                    if let Some(key) = self.pending_key.take() {
                        return self.command_from_string(&key, value);
                    }
                }
            }
        }

        if let Some(key) = self.pending_key.take() {
            if key == "Volume" {
                if let Some(volume) = dbus_integer(line) {
                    let volume = (volume as f32 / 127.0).clamp(0.0, 1.0);
                    return Some(MediaCommand::Volume(volume));
                }
            } else {
                self.pending_key = Some(key);
            }
        }

        None
    }

    fn command_from_string(&self, key: &str, value: &str) -> Option<MediaCommand> {
        match (self.interface, key, value) {
            (Some(Interface::MediaTransport), "State", "active") => Some(MediaCommand::Play),
            (Some(Interface::MediaTransport), "State", "idle") => Some(MediaCommand::Pause),
            (Some(Interface::MediaPlayer), "Status", "playing") => Some(MediaCommand::Play),
            (Some(Interface::MediaPlayer), "Status", "paused") => Some(MediaCommand::Pause),
            (Some(Interface::MediaPlayer), "Status", "stopped") => Some(MediaCommand::Stop),
            _ => None,
        }
    }
}

fn dbus_string(line: &str) -> Option<&str> {
    let offset = line.find("string \"")? + "string \"".len();
    let value = &line[offset..];
    value.strip_suffix('"')
}

fn dbus_integer(line: &str) -> Option<u32> {
    let mut parts = line.split_whitespace();
    while let Some(kind) = parts.next() {
        match kind {
            "byte" | "uint16" | "uint32" | "int16" | "int32" => {
                return parts.next()?.parse().ok();
            }
            _ => {}
        }
    }
    None
}
