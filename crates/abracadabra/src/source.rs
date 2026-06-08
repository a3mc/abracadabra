//! Log source: file path or systemd journal unit.

use std::path::PathBuf;

/// Where to read log lines from.
#[derive(Debug, Clone)]
pub enum LogSource {
    /// A plain file (or symlink to one).
    File(PathBuf),
    /// A systemd unit streamed via `journalctl`.
    Journal { unit: String },
}

impl LogSource {
    /// A `PathBuf` suitable for display in headers and status lines.
    pub fn as_display_path(&self) -> PathBuf {
        match self {
            Self::File(p) => p.clone(),
            Self::Journal { unit } => PathBuf::from(format!("journal:{unit}")),
        }
    }
}

impl Default for LogSource {
    fn default() -> Self {
        Self::File(PathBuf::new())
    }
}
