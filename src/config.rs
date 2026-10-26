//! Tiny persisted config: language, last station and board, recent stations.
//!
//! Stored at `$XDG_CONFIG_HOME/oebb-monitor/config` (or `~/.config/...`) as
//! `key = value` lines. Unknown keys and malformed lines are ignored, so older
//! and newer versions can share a file. Uses only std, no extra dependencies.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::app::ContentType;
use crate::lang::Lang;

/// Everything the app remembers between runs. Every field is optional: an
/// absent value means "use the default".
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    /// Only set once the user has toggled the language; until then the locale
    /// decides on every launch.
    pub language: Option<Lang>,
    pub station: Option<String>,
    pub board: Option<ContentType>,
    /// Recently chosen station ids, most recent first.
    pub recent: Vec<String>,
}

impl Config {
    /// Parse the file contents, keeping whatever is valid.
    fn parse(content: &str) -> Config {
        let mut cfg = Config::default();
        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "language" => cfg.language = Lang::from_code(value),
                "station" if !value.is_empty() => cfg.station = Some(value.to_string()),
                "board" => {
                    cfg.board = match value {
                        "departure" => Some(ContentType::Departure),
                        "arrival" => Some(ContentType::Arrival),
                        _ => None,
                    }
                }
                "recent" => {
                    cfg.recent = value
                        .split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(String::from)
                        .collect()
                }
                _ => {}
            }
        }
        cfg
    }

    /// Serialise to the file format, writing only the values that are set.
    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(lang) = self.language {
            out.push_str(&format!("language = {}\n", lang.code()));
        }
        if let Some(station) = &self.station {
            out.push_str(&format!("station = {station}\n"));
        }
        if let Some(board) = &self.board {
            let board = match board {
                ContentType::Departure => "departure",
                ContentType::Arrival => "arrival",
            };
            out.push_str(&format!("board = {board}\n"));
        }
        if !self.recent.is_empty() {
            out.push_str(&format!("recent = {}\n", self.recent.join(",")));
        }
        out
    }
}

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

/// Load the saved config; a missing or unreadable file yields the defaults.
pub fn load() -> Config {
    config_path().map(|p| load_from(&p)).unwrap_or_default()
}

fn load_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .map(|c| Config::parse(&c))
        .unwrap_or_default()
}

/// Persist the config. A missing config location is not an error (nothing to
/// save to); I/O failures are returned for the caller to report.
#[cfg_attr(test, allow(dead_code))]
pub fn save(cfg: &Config) -> std::io::Result<()> {
    match config_path() {
        Some(path) => save_to(&path, cfg),
        None => Ok(()),
    }
}

/// Write via a temp file + rename so a crash can't leave a truncated config.
fn save_to(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, cfg.render())?;
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

    fn full() -> Config {
        Config {
            language: Some(Lang::En),
            station: Some("8101114".into()),
            board: Some(ContentType::Arrival),
            recent: vec!["8101114".into(), "8101001".into()],
        }
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
        assert_eq!(Config::parse("language = en\n").language, Some(Lang::En));
        assert_eq!(Config::parse("language=DE").language, Some(Lang::De));
        let cfg = Config::parse("# c\nfoo = 1\n  language =  en  \nboard=arrival\n");
        assert_eq!(cfg.language, Some(Lang::En));
        assert_eq!(cfg.board, Some(ContentType::Arrival));
    }

    #[test]
    fn parse_rejects_unknown_or_missing() {
        assert_eq!(Config::parse(""), Config::default());
        assert_eq!(Config::parse("language = fr").language, None);
        assert_eq!(Config::parse("lang = en").language, None);
        assert_eq!(Config::parse("language en").language, None);
        assert_eq!(Config::parse("board = sideways").board, None);
        assert_eq!(Config::parse("station = ").station, None);
    }

    #[test]
    fn parse_recent_skips_blank_entries() {
        let cfg = Config::parse("recent = 1, ,2,,3 \n");
        assert_eq!(cfg.recent, ["1", "2", "3"]);
    }

    #[test]
    fn language_only_files_from_older_versions_still_load() {
        let cfg = Config::parse("language = de\n");
        assert_eq!(
            cfg,
            Config {
                language: Some(Lang::De),
                ..Config::default()
            }
        );
    }

    #[test]
    fn render_writes_only_set_values() {
        assert_eq!(Config::default().render(), "");
        assert_eq!(Config::parse(&full().render()), full());
    }

    #[test]
    fn save_then_load_roundtrip_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("oebb-monitor")
            .join("config");
        assert_eq!(load_from(&path), Config::default());
        save_to(&path, &full()).unwrap();
        assert_eq!(load_from(&path), full());
        let mut changed = full();
        changed.language = Some(Lang::De);
        save_to(&path, &changed).unwrap();
        assert_eq!(load_from(&path), changed);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        save_to(&path, &full()).unwrap();
        save_to(&path, &Config::default()).unwrap();
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
        assert!(save_to(&path, &full()).is_err());
        // ...and the temp file is cleaned up again.
        assert!(!path.with_extension("tmp").exists());
        // A parent that is a file cannot be created either.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(save_to(&file.join("sub").join("config"), &full()).is_err());
    }
}
