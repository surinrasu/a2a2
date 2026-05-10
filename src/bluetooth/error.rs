//! Error types for Bluetooth operations.

use thiserror::Error;

/// Bluetooth-specific error types.
#[derive(Error, Debug)]
pub enum BluetoothError {
    /// ALSA error during audio capture.
    #[error("ALSA error: {0}")]
    Alsa(String),

    /// Audio capture channel closed.
    #[error("Audio capture stopped")]
    CaptureStopped,

    /// System setup issue.
    #[error("System setup error: {0}")]
    Setup(String),

    /// Operation timed out.
    #[error("Operation timed out")]
    Timeout,

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience Result type for Bluetooth operations.
pub type Result<T> = std::result::Result<T, BluetoothError>;
