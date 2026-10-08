//! Opt-in debug logging.
//!
//! When the program is launched with `--debug`/`-d`, every `debug!` invocation
//! is timestamped and appended to `$XDG_STATE_HOME/oebb-monitor/debug.log` (or
//! `~/.local/state/...`, falling back to the OS temp dir). Without the flag the
//! macro compiles to a cheap no-op call that returns early.

use chrono::Local;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

/// Path to the log file: a per-user state dir, so no shared, predictable
/// location in a world-writable directory.
pub fn log_path() -> PathBuf {
    log_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn log_path_from(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    xdg.map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".local/state")))
        .map(|base| base.join("oebb-monitor").join("debug.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("oebb-debug.log"))
}

/// Create/truncate the log file, owner-readable only on Unix.
fn open_log() -> Option<std::fs::File> {
    open_log_at(&log_path())
}

fn open_log_at(path: &std::path::Path) -> Option<std::fs::File> {
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

#[cfg(test)]
mod tests {

    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn path_uses_xdg_state_home_then_home() {
        assert_eq!(
            log_path_from(os("/s"), os("/h")),
            PathBuf::from("/s/oebb-monitor/debug.log")
        );
        assert_eq!(
            log_path_from(None, os("/h")),
            PathBuf::from("/h/.local/state/oebb-monitor/debug.log")
        );
        assert_eq!(
            log_path_from(os(""), os("/h")),
            PathBuf::from("/h/.local/state/oebb-monitor/debug.log")
        );
    }

    #[test]
    fn path_falls_back_to_temp_dir() {
        let p = log_path_from(None, None);
        assert!(p.starts_with(std::env::temp_dir()));
        assert!(p.ends_with("oebb-debug.log"));
    }

    #[test]
    fn disabled_logger_never_opens_a_file() {
        let logger = DebugLogger::new(false);
        assert!(!logger.enabled && logger.file.is_none());
        logger.log("ignored".into());
    }

    #[test]
    fn open_log_creates_dirs_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("debug.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old content").unwrap();
        let mut f = open_log_at(&path).unwrap();
        writeln!(f, "new").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x").join("debug.log");
        let f = open_log_at(&path).unwrap();
        let mode = f.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn enabled_logger_writes_timestamped_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");
        let logger = DebugLogger {
            enabled: true,
            file: open_log_at(&path).map(std::sync::Mutex::new),
        };
        logger.log("hello world".into());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with('[') && content.trim_end().ends_with("] hello world"));
    }
}
