//! Bluetooth audio capture support for `a2a2`.
//!
//! The bridge relies on the system BlueZ tools for pairing/control and BlueALSA
//! for PCM capture. This module intentionally keeps only the pieces used by that
//! workflow.
//!
//! ## Requirements
//!
//! This module is Linux-only and requires:
//! - BlueZ daemon (bluetooth service)
//! - BlueALSA daemon (bluez-alsa-utils package)
//!
//! Use `SystemSetup::check()` to verify requirements are met.
//!
#![cfg(target_os = "linux")]

pub mod alsa_capture;
pub mod error;
pub mod setup;

pub use alsa_capture::{
    calculate_rms, start_capture, AudioFormat, CaptureConfig, CaptureHandle, CapturedFrame,
    SAMPLE_RATE_HD,
};
pub use error::{BluetoothError, Result};
pub use setup::{ComponentStatus, SetupIssue, SetupStatus, SystemSetup};
