//! Opt-in debug logging.
//!
//! When the program is launched with `--debug`/`-d`, every `debug!` invocation
//! is timestamped and appended to `$XDG_STATE_HOME/oebb-monitor/debug.log` (or
//! `~/.local/state/...`, falling back to the OS temp dir). Without the flag the
//! macro compiles to a cheap no-op call that returns early.

use chrono::Local;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

/// Path to the log file: a per-user state dir, so no shared, predictable
/// location in a world-writable directory.
pub fn log_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|base| base.join("oebb-monitor").join("debug.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("oebb-debug.log"))
}

/// Create/truncate the log file, owner-readable only on Unix.
fn open_log() -> Option<std::fs::File> {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).ok()
}

/// A lazily-initialised logger that writes to a temp file when enabled.
pub struct DebugLogger {
    /// Whether `--debug`/`-d` was passed on the command line.
    pub enabled: bool,
    file: Option<std::sync::Mutex<std::fs::File>>,
}

impl DebugLogger {
    fn new(enabled: bool) -> Self {
        let file = if enabled {
            open_log().map(std::sync::Mutex::new)
        } else {
            None
        };
        Self { enabled, file }
    }

    /// Append a timestamped line to the log file. No-op when disabled.
    pub fn log(&self, msg: String) {
        if !self.enabled {
            return;
        }
        if let Some(ref file) = self.file {
            if let Ok(mut f) = file.lock() {
                let timestamp = Local::now().format("%H:%M:%S%.3f");
                let _ = writeln!(f, "[{}] {}", timestamp, msg);
                let _ = f.flush();
            }
        }
    }
}

/// The process-wide logger, initialised from the command line on first use.
pub static DEBUG: LazyLock<DebugLogger> = LazyLock::new(|| {
    let enabled = std::env::args().any(|arg| arg == "--debug" || arg == "-d");
    DebugLogger::new(enabled)
});

/// Format and log a message through [`DEBUG`]. Usable from any module.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::debug::DEBUG.log(format!($($arg)*))
    };
}
