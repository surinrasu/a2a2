//! System setup verification for Bluetooth audio.
//!
//! Checks that required system components (BlueZ, BlueALSA) are installed and running.

use std::process::Command;

/// Status of a system component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentStatus {
    /// Component is installed and running.
    Ok,
    /// Component is installed but not running.
    NotRunning,
    /// Component is not installed.
    NotInstalled,
    /// Unable to determine status.
    Unknown,
}

/// A single setup issue with suggested fix.
#[derive(Debug, Clone)]
pub struct SetupIssue {
    /// Description of the issue.
    pub description: String,
    /// Suggested command to fix the issue.
    pub fix_command: Option<String>,
}

/// Overall system setup status.
#[derive(Debug, Clone)]
pub struct SetupStatus {
    /// BlueZ daemon status.
    pub bluez: ComponentStatus,
    /// BlueALSA daemon status.
    pub bluealsa: ComponentStatus,
    /// List of issues found.
    pub issues: Vec<SetupIssue>,
    /// Whether the system is ready for Bluetooth audio.
    pub ready: bool,
}

impl SetupStatus {
    /// Get a summary message for the status.
    pub fn summary(&self) -> String {
        if self.ready {
            "System is ready for Bluetooth audio".to_string()
        } else {
            format!("{} issue(s) found", self.issues.len())
        }
    }
}

/// System setup verification and auto-installation.
pub struct SystemSetup;

impl SystemSetup {
    /// Check system setup status.
    ///
    /// Returns a `SetupStatus` with the current state of required components.
    pub fn check() -> SetupStatus {
        let mut issues = Vec::new();

        // Check BlueZ
        let bluez = Self::check_bluez();
        if bluez != ComponentStatus::Ok {
            issues.push(SetupIssue {
                description: match bluez {
                    ComponentStatus::NotInstalled => "BlueZ is not installed".to_string(),
                    ComponentStatus::NotRunning => "Bluetooth service is not running".to_string(),
                    _ => "BlueZ status unknown".to_string(),
                },
                fix_command: Some(match bluez {
                    ComponentStatus::NotInstalled => "sudo apt install bluez".to_string(),
                    ComponentStatus::NotRunning => "sudo systemctl start bluetooth".to_string(),
                    _ => "sudo systemctl status bluetooth".to_string(),
                }),
            });
        }

        // Check BlueALSA
        let bluealsa = Self::check_bluealsa();
        if bluealsa != ComponentStatus::Ok {
            issues.push(SetupIssue {
                description: match bluealsa {
                    ComponentStatus::NotInstalled => "BlueALSA is not installed".to_string(),
                    ComponentStatus::NotRunning => "BlueALSA service is not running".to_string(),
                    _ => "BlueALSA status unknown".to_string(),
                },
                fix_command: Some(match bluealsa {
                    ComponentStatus::NotInstalled => {
                        "sudo apt install bluez-alsa-utils".to_string()
                    }
                    ComponentStatus::NotRunning => "sudo systemctl start bluealsa-a2a2".to_string(),
                    _ => "sudo systemctl status bluealsa-a2a2".to_string(),
                }),
            });
        }

        let ready = bluez == ComponentStatus::Ok && bluealsa == ComponentStatus::Ok;

        SetupStatus {
            bluez,
            bluealsa,
            issues,
            ready,
        }
    }

    /// Check BlueZ daemon status.
    fn check_bluez() -> ComponentStatus {
        // Check if bluetoothctl exists (indicates BlueZ is installed)
        let installed = Command::new("which")
            .arg("bluetoothctl")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !installed {
            return ComponentStatus::NotInstalled;
        }

        // Check if bluetooth service is running
        let running = Command::new("systemctl")
            .args(["is-active", "--quiet", "bluetooth"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if running {
            ComponentStatus::Ok
        } else {
            ComponentStatus::NotRunning
        }
    }

    /// Check BlueALSA daemon status.
    fn check_bluealsa() -> ComponentStatus {
        // Debian/Raspberry Pi OS bookworm ships `bluealsa` and `bluealsa-aplay`
        // in bluez-alsa-utils; some newer builds also ship `bluealsactl`.
        let installed = ["bluealsa", "bluealsactl", "bluealsa-aplay"]
            .iter()
            .any(|cmd| {
                Command::new("which")
                    .arg(cmd)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            });

        if !installed {
            return ComponentStatus::NotInstalled;
        }

        // Accept either the distro service or a2a2's profile-specific unit.
        let running = ["bluealsa", "bluealsa-a2a2"].iter().any(|unit| {
            Command::new("systemctl")
                .args(["is-active", "--quiet", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }) || Command::new("pgrep")
            .args(["-x", "bluealsa"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if running {
            ComponentStatus::Ok
        } else {
            ComponentStatus::NotRunning
        }
    }
}
