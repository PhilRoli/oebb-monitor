//! Tiny persisted config: just the chosen UI language for now.
//!
//! Stored at `$XDG_CONFIG_HOME/oebb-monitor/config` (or `~/.config/...`) as a
//! single `language = de|en` line. Uses only std, no extra dependencies.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::lang::Lang;

/// Path to the config file, honoring `$XDG_CONFIG_HOME` then `$HOME/.config`.
fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn config_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("oebb-monitor").join("config"))
}

/// Extract the language from the config file's contents.
fn parse_language(content: &str) -> Option<Lang> {
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix("language")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        Lang::from_code(value)
    })
}

/// Load the saved language, or `None` if unset / unreadable.
pub fn load_language() -> Option<Lang> {
    load_language_from(&config_path()?)
}

fn load_language_from(path: &Path) -> Option<Lang> {
    parse_language(&std::fs::read_to_string(path).ok()?)
}

/// Persist the chosen language. A missing config location is not an error
/// (nothing to save to); I/O failures are returned for the caller to report.
#[cfg_attr(test, allow(dead_code))]
pub fn save_language(lang: Lang) -> std::io::Result<()> {
    match config_path() {
        Some(path) => save_language_to(&path, lang),
        None => Ok(()),
    }
}

/// Write via a temp file + rename so a crash can't leave a truncated config.
fn save_language_to(path: &Path, lang: Lang) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("language = {}\n", lang.code()))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn path_prefers_xdg_over_home() {
        let p = config_path_from(os("/x"), os("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/x/oebb-monitor/config"));
    }

    #[test]
    fn path_falls_back_to_home_dot_config() {
        let p = config_path_from(None, os("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/oebb-monitor/config"));
    }

    #[test]
    fn empty_xdg_is_ignored() {
        let p = config_path_from(os(""), os("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/oebb-monitor/config"));
    }

    #[test]
    fn path_none_without_any_env() {
        assert!(config_path_from(None, None).is_none());
    }

    #[test]
    fn parse_accepts_spacing_and_case_and_extra_lines() {
        assert_eq!(parse_language("language = en\n"), Some(Lang::En));
        assert_eq!(parse_language("language=DE"), Some(Lang::De));
        assert_eq!(
            parse_language("# c\nfoo = 1\n  language =  en  \n"),
            Some(Lang::En)
        );
    }

    #[test]
    fn parse_rejects_unknown_or_missing() {
        assert_eq!(parse_language(""), None);
        assert_eq!(parse_language("language = fr"), None);
        assert_eq!(parse_language("lang = en"), None);
        assert_eq!(parse_language("language en"), None);
    }

    #[test]
    fn save_then_load_roundtrip_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("oebb-monitor")
            .join("config");
        assert_eq!(load_language_from(&path), None);
        save_language_to(&path, Lang::En).unwrap();
        assert_eq!(load_language_from(&path), Some(Lang::En));
        save_language_to(&path, Lang::De).unwrap();
        assert_eq!(load_language_from(&path), Some(Lang::De));
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        save_language_to(&path, Lang::En).unwrap();
        save_language_to(&path, Lang::De).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config"]);
    }

    #[test]
    fn save_reports_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        // The target path is a directory, so the rename must fail...
        let path = dir.path().join("config");
        std::fs::create_dir(&path).unwrap();
        assert!(save_language_to(&path, Lang::En).is_err());
        // ...and the temp file is cleaned up again.
        assert!(!path.with_extension("tmp").exists());
        // A parent that is a file cannot be created either.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(save_language_to(&file.join("sub").join("config"), Lang::En).is_err());
    }
}
