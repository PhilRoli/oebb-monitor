//! Application state and the pure logic that operates on it.
//!
//! [`App`] is the single source of truth shared (behind a mutex) between the UI
//! loop and the WebSocket task. This module also holds small, side-effect-free
//! helpers ([`format_time`], [`calculate_delay`], [`build_ws_url`]) that are
//! unit-tested in isolation.

use chrono::{DateTime, Local};
use ratatui::widgets::{ListState, TableState};
use std::collections::HashMap;

use crate::config::Config;
use crate::lang::Lang;
use crate::model::{SpecialNotice, TrainItem};

/// Which board to display: departures or arrivals.
#[derive(Clone, PartialEq, Debug)]
pub enum ContentType {
    Departure,
    Arrival,
}

/// The active screen, which determines both rendering and key handling.
#[derive(Clone, PartialEq, Debug)]
pub enum AppMode {
    Normal,
    /// The main board with the filter input focused.
    Filter,
    StationSelect,
    TrainDetail,
}

/// Live-connection status, surfaced in the status bar.
#[derive(Clone, PartialEq, Debug)]
pub enum ConnectionState {
    Connecting,
    Connected,
    /// All page connections failed; the UI renders a translated message.
    Failed,
}

/// Why the live connection last had trouble, shown next to the status.
#[derive(Clone, PartialEq, Debug)]
pub enum LinkError {
    /// The socket could not be opened (carries the underlying reason).
    Connect(String),
    /// The socket closed or errored after connecting.
    Closed(String),
    /// The socket went silent for too long.
    Timeout,
    /// A message from the server could not be parsed.
    Parse,
}

/// How many stations the picker lists at once.
const STATION_LIST_LEN: usize = 20;
/// How many recently chosen stations are remembered.
const RECENT_LEN: usize = 8;

/// The complete application state.
pub struct App {
    pub content_type: ContentType,
    pub station_id: String,
    pub station_name: String,
    pub items: Vec<TrainItem>,
    pub special_notices: Vec<SpecialNotice>,
    pub last_update: Option<DateTime<Local>>,
    pub connection: ConnectionState,
    /// Most recent connection problem; cleared by the next good update.
    pub last_error: Option<LinkError>,
    pub mode: AppMode,
    pub stations: HashMap<String, String>,
    pub all_stations_sorted: Vec<(String, String)>,
    /// [`normalize`]d names, parallel to [`Self::all_stations_sorted`].
    search_keys: Vec<String>,
    pub filtered_stations: Vec<(String, String)>,
    pub total_filtered_count: usize,
    pub station_search: String,
    pub station_list_state: ListState,
    /// Recently chosen station ids, most recent first.
    pub recent_stations: Vec<String>,
    pub max_pages: usize,
    /// Index into [`Self::visible_items`], not into [`Self::items`].
    pub selected_train_index: Option<usize>,
    pub selected_train_id: Option<String>,
    /// Scroll offset of the main board, kept so the selected row stays visible.
    pub board_state: TableState,
    pub detail_scroll: u16,
    /// Board filter text; empty shows every train.
    pub filter: String,
    /// Whether the key help overlay is open (on top of the current mode).
    pub show_help: bool,
    pub lang: Lang,
    /// Whether the user picked [`Self::lang`] (so it is saved) rather than it
    /// coming from the locale (so it follows the locale on the next launch).
    pub lang_chosen: bool,
}

impl App {
    /// Build the initial state, loading the embedded station list and defaulting
    /// to departures at Wien Westbahnhof in the locale's language.
    pub fn new() -> Self {
        let mut app = Self {
            content_type: ContentType::Departure,
            station_id: "8101001".to_string(),
            station_name: "Wien Westbahnhof".to_string(),
            items: Vec::new(),
            special_notices: Vec::new(),
            last_update: None,
            connection: ConnectionState::Connecting,
            last_error: None,
            mode: AppMode::Normal,
            stations: HashMap::new(),
            all_stations_sorted: Vec::new(),
            search_keys: Vec::new(),
            filtered_stations: Vec::new(),
            total_filtered_count: 0,
            station_search: String::new(),
            station_list_state: ListState::default(),
            recent_stations: Vec::new(),
            max_pages: 5,
            selected_train_index: None,
            selected_train_id: None,
            board_state: TableState::default(),
            detail_scroll: 0,
            filter: String::new(),
            show_help: false,
            lang: Lang::detect(),
            lang_chosen: false,
        };

        const STATIONS_JSON: &str = include_str!("../stations.json");

        if let Ok(stations) = serde_json::from_str::<HashMap<String, String>>(STATIONS_JSON) {
            app.stations = stations
                .into_iter()
                .map(|(k, v)| (k.trim().to_string(), v))
                .collect();
            debug!("Loaded {} stations from embedded data", app.stations.len());
        } else {
            debug!("Failed to parse embedded stations.json");
        }

        // Show the list's spelling of the default station, if present.
        if let Some(name) = app.stations.get(&app.station_id) {
            app.station_name = name.clone();
        }

        let mut sorted: Vec<(String, String)> = app
            .stations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        sorted.sort_by(|a, b| a.1.cmp(&b.1));
        app.search_keys = sorted.iter().map(|(_, name)| normalize(name)).collect();
        app.all_stations_sorted = sorted;

        app
    }

    /// Apply saved settings. Values that no longer make sense (e.g. a station
    /// id missing from the embedded list) are skipped.
    pub fn restore(&mut self, cfg: &Config) {
        if let Some(lang) = cfg.language {
            self.lang = lang;
            self.lang_chosen = true;
        }
        if let Some(id) = &cfg.station {
            self.set_station(id);
        }
        if let Some(board) = &cfg.board {
            self.content_type = board.clone();
        }
        self.recent_stations = cfg
            .recent
            .iter()
            .filter(|id| self.stations.contains_key(*id))
            .take(RECENT_LEN)
            .cloned()
            .collect();
    }

    /// The settings worth remembering for the next launch.
    pub fn to_config(&self) -> Config {
        Config {
            language: self.lang_chosen.then_some(self.lang),
            station: Some(self.station_id.clone()),
            board: Some(self.content_type.clone()),
            recent: self.recent_stations.clone(),
        }
    }

    /// Switch to the station with this id, if it is known. Clears the board
    /// filter, which was meant for the previous station's trains.
    pub fn set_station(&mut self, id: &str) -> bool {
        let Some(name) = self.stations.get(id) else {
            return false;
        };
        self.station_id = id.to_string();
        self.station_name = name.clone();
        self.filter.clear();
        true
    }

    /// Find a station by id, by exact name, or failing that the best search
    /// match — for picking a station from the command line.
    pub fn resolve_station(&self, query: &str) -> Option<String> {
        let query = query.trim();
        if self.stations.contains_key(query) {
            return Some(query.to_string());
        }
        let key = normalize(query);
        if key.is_empty() {
            return None;
        }
        self.search_keys
            .iter()
            .position(|k| *k == key)
            .map(|i| self.all_stations_sorted[i].0.clone())
            .or_else(|| {
                self.search_stations(query)
                    .first()
                    .map(|(id, _)| id.clone())
            })
    }

    /// Every station matching `query`, best first: names starting with it,
    /// then names with a word starting with it, then any other match. Within
    /// each group main stations (Hbf) lead, so `linz` finds Linz Hbf before
    /// Linz Ebelsberg; otherwise alphabetical.
    fn search_stations(&self, query: &str) -> Vec<(String, String)> {
        let query = normalize(query);
        let mut matches: Vec<(u8, usize)> = self
            .search_keys
            .iter()
            .enumerate()
            .filter_map(|(i, key)| match_rank(key, &query).map(|rank| (rank, i)))
            .collect();
        // Stable, so the alphabetical order survives within each group.
        matches.sort_by_key(|&(rank, i)| (rank, !self.search_keys[i].ends_with(" hbf")));
        matches
            .into_iter()
            .map(|(_, i)| self.all_stations_sorted[i].clone())
            .collect()
    }

    /// Open the station picker with a cleared search field.
    pub fn enter_station_select(&mut self) {
        self.mode = AppMode::StationSelect;
        self.station_search.clear();
        self.update_filtered_stations();
        self.station_list_state.select(Some(0));
    }

    /// Close the station picker, returning to the main board.
    pub fn exit_station_select(&mut self) {
        self.mode = AppMode::Normal;
    }

    /// Recompute [`Self::filtered_stations`] from the current search string,
    /// capping the visible list while tracking the total match count for the
    /// header. An empty search lists recent stations first.
    pub fn update_filtered_stations(&mut self) {
        let mut matches = if self.station_search.is_empty() {
            let recent = self
                .recent_stations
                .iter()
                .filter_map(|id| Some((id.clone(), self.stations.get(id)?.clone())));
            let rest = self
                .all_stations_sorted
                .iter()
                .filter(|(id, _)| !self.recent_stations.contains(id))
                .cloned();
            recent.chain(rest).take(STATION_LIST_LEN).collect()
        } else {
            self.search_stations(&self.station_search)
        };
        self.total_filtered_count = if self.station_search.is_empty() {
            self.all_stations_sorted.len()
        } else {
            matches.len()
        };
        matches.truncate(STATION_LIST_LEN);
        self.filtered_stations = matches;

        if !self.filtered_stations.is_empty() {
            self.station_list_state.select(Some(0));
        }
    }

    /// Commit the highlighted station as the active one and remember it as
    /// recent. Returns `true` if a selection was made (so the caller can
    /// trigger a reconnect).
    pub fn select_station(&mut self) -> bool {
        let Some(id) = self
            .station_list_state
            .selected()
            .and_then(|i| self.filtered_stations.get(i))
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        self.set_station(&id);
        self.recent_stations.retain(|r| *r != id);
        self.recent_stations.insert(0, id);
        self.recent_stations.truncate(RECENT_LEN);
        self.exit_station_select();
        true
    }

    /// The trains currently shown on the board: all of them, or those matching
    /// [`Self::filter`] by train, line, destination/origin, or stop.
    pub fn visible_items(&self) -> Vec<&TrainItem> {
        let query = normalize(&self.filter);
        if query.is_empty() {
            return self.items.iter().collect();
        }
        self.items
            .iter()
            .filter(|item| {
                let mut fields = vec![item.train.as_str()];
                fields.extend(item.line.as_deref());
                fields.extend(item.product.as_deref());
                fields.extend(item.destination.as_ref().map(|d| d.default.as_str()));
                fields.extend(item.origin.as_ref().map(|d| d.default.as_str()));
                fields.extend(item.via.as_ref().map(|d| d.default.as_str()));
                fields.extend(item.major_stops());
                fields.iter().any(|f| normalize(f).contains(&query))
            })
            .collect()
    }

    /// The selected train, if it is still on the (filtered) board.
    pub fn selected_train(&self) -> Option<&TrainItem> {
        self.visible_items()
            .get(self.selected_train_index?)
            .copied()
    }

    /// Point the selection at the tracked train's current row, e.g. after the
    /// board re-sorted or the filter changed. Loses the selection if the train
    /// is no longer shown.
    pub fn resync_selection(&mut self) {
        if let Some(id) = &self.selected_train_id {
            self.selected_train_index = self.visible_items().iter().position(|i| i.id == *id);
        }
    }

    /// Select the visible train at `index`, clamped to the board, keeping the
    /// tracked id in sync and resetting the detail scroll.
    pub fn select_index(&mut self, index: usize) {
        let visible = self.visible_items();
        let Some(last) = visible.len().checked_sub(1) else {
            return;
        };
        let index = index.min(last);
        let id = visible[index].id.clone();
        self.selected_train_index = Some(index);
        self.selected_train_id = Some(id);
        self.detail_scroll = 0;
    }

    /// Move the train selection by `delta`, clamped to the bounds of the list.
    /// Unifies the up/down handling for both the main list and the detail view;
    /// the first move from no selection lands on the first train.
    pub fn select_relative(&mut self, delta: i32) {
        let index = match self.selected_train_index {
            Some(idx) => (idx as i64 + delta as i64).max(0) as usize,
            None => 0,
        };
        self.select_index(index);
    }

    /// Select the last visible train.
    pub fn select_last(&mut self) {
        self.select_index(usize::MAX);
    }
}

/// Fold a name or query into a canonical form for matching: lowercase, umlauts
/// and their ae/oe/ue spellings folded to the plain vowel, ß to ss, punctuation
/// to single spaces, and common abbreviations unified (Sankt → st, Hauptbahnhof
/// → hbf, Bahnhof → bf), so `st polten`, `Sankt Poelten` and `St.Pölten` agree.
pub fn normalize(s: &str) -> String {
    let mut folded = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        match c {
            'ä' => folded.push('a'),
            'ö' => folded.push('o'),
            'ü' => folded.push('u'),
            'ß' => folded.push_str("ss"),
            c if c.is_alphanumeric() => folded.push(c),
            _ => folded.push(' '),
        }
    }
    let folded = folded
        .replace("ae", "a")
        .replace("oe", "o")
        .replace("ue", "u");
    folded
        .split_whitespace()
        .map(|word| match word {
            "sankt" => "st",
            "hauptbahnhof" => "hbf",
            "bahnhof" => "bf",
            word => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How well a normalised name matches a normalised query (lower is better),
/// or `None` if it doesn't match at all.
fn match_rank(name: &str, query: &str) -> Option<u8> {
    if name.starts_with(query) {
        Some(0)
    } else if name.contains(&format!(" {query}")) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else {
        None
    }
}

/// Build the WebSocket URL for one page of a station's board.
pub fn build_ws_url(station_id: &str, content_type: &ContentType, page: usize) -> String {
    let content = match content_type {
        ContentType::Departure => "departure",
        ContentType::Arrival => "arrival",
    };
    format!(
        "wss://meine.oebb.at/abfahrtankunft/webdisplay/web_client/ws/?stationId={}&contentType={}&staticLayout=false&page={}&offset=0&ignoreIncident=false&expandAll=false",
        encode_query_value(station_id),
        content,
        page
    )
}

/// Percent-encode everything except RFC 3986 unreserved characters, so a
/// station id can never inject extra query parameters.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Format an RFC 3339 timestamp as local `HH:MM`, or `"-"` if unparseable.
pub fn format_time(iso_time: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso_time) {
        let local: DateTime<Local> = dt.into();
        local.format("%H:%M").to_string()
    } else {
        "-".to_string()
    }
}

/// Compute a train's delay in whole minutes (negative if early), returning
/// `None` when there is no expected time, the timestamps don't parse, or the
/// train is exactly on time.
pub fn calculate_delay(item: &TrainItem) -> Option<i64> {
    let expected = item.expected.as_ref()?;
    let scheduled = DateTime::parse_from_rfc3339(&item.scheduled).ok()?;
    let exp = DateTime::parse_from_rfc3339(expected).ok()?;
    let delay = (exp - scheduled).num_minutes();
    (delay != 0).then_some(delay)
}

#[cfg(test)]
mod tests {

    use super::*;

    fn item(scheduled: &str, expected: Option<&str>) -> TrainItem {
        TrainItem {
            scheduled: scheduled.to_string(),
            expected: expected.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn delay_positive() {
        let i = item(
            "2024-01-01T10:00:00+01:00",
            Some("2024-01-01T10:07:00+01:00"),
        );
        assert_eq!(calculate_delay(&i), Some(7));
    }

    #[test]
    fn delay_negative() {
        let i = item(
            "2024-01-01T10:05:00+01:00",
            Some("2024-01-01T10:03:00+01:00"),
        );
        assert_eq!(calculate_delay(&i), Some(-2));
    }

    #[test]
    fn delay_none_when_on_time() {
        let i = item(
            "2024-01-01T10:00:00+01:00",
            Some("2024-01-01T10:00:00+01:00"),
        );
        assert_eq!(calculate_delay(&i), None);
    }

    #[test]
    fn delay_none_without_expected() {
        let i = item("2024-01-01T10:00:00+01:00", None);
        assert_eq!(calculate_delay(&i), None);
    }

    #[test]
    fn format_time_invalid_is_dash() {
        assert_eq!(format_time("not-a-time"), "-");
    }

    #[test]
    fn format_time_valid_is_hh_mm() {
        let formatted = format_time("2024-01-01T10:05:00+00:00");
        assert_eq!(formatted.len(), 5);
        assert_eq!(formatted.as_bytes()[2], b':');
    }

    #[test]
    fn station_filter_matches_query() {
        let mut app = App::new();
        app.station_search = "wien".to_string();
        app.update_filtered_stations();
        assert!(!app.filtered_stations.is_empty());
        assert!(app
            .filtered_stations
            .iter()
            .all(|(_, name)| name.to_lowercase().contains("wien")));
    }

    #[test]
    fn station_filter_empty_shows_some() {
        let mut app = App::new();
        app.station_search.clear();
        app.update_filtered_stations();
        assert!(!app.filtered_stations.is_empty());
        assert_eq!(app.total_filtered_count, app.all_stations_sorted.len());
    }

    fn train(id: &str) -> TrainItem {
        TrainItem {
            id: id.to_string(),
            train: id.to_string(),
            scheduled: "2024-01-01T10:00:00+01:00".to_string(),
            ..Default::default()
        }
    }

    fn app_with_trains(n: usize) -> App {
        let mut app = App::new();
        app.items = (0..n).map(|i| train(&format!("t{i}"))).collect();
        app
    }

    #[test]
    fn embedded_station_list_is_valid() {
        let app = App::new();
        assert!(app.stations.len() > 100, "station list suspiciously small");
        assert_eq!(app.stations.len(), app.all_stations_sorted.len());
        assert!(app
            .stations
            .iter()
            .all(|(id, name)| !id.is_empty() && id == id.trim() && !name.is_empty()));
        assert!(app.all_stations_sorted.windows(2).all(|w| w[0].1 <= w[1].1));
    }

    #[test]
    fn default_station_exists_in_station_list() {
        let app = App::new();
        assert!(app.stations.contains_key(&app.station_id));
        assert_eq!(app.content_type, ContentType::Departure);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn ws_url_departure_and_arrival() {
        let d = build_ws_url("8101001", &ContentType::Departure, 2);
        assert!(d.starts_with("wss://meine.oebb.at/abfahrtankunft/webdisplay/web_client/ws/?"));
        assert!(d.contains("stationId=8101001"));
        assert!(d.contains("contentType=departure"));
        assert!(d.contains("page=2"));
        let a = build_ws_url("1", &ContentType::Arrival, 5);
        assert!(a.contains("contentType=arrival") && a.contains("page=5"));
    }

    #[test]
    fn station_filter_is_case_insensitive_and_capped() {
        let mut app = App::new();
        app.station_search = "WIEN".to_string();
        app.update_filtered_stations();
        assert!(app.filtered_stations.len() <= 20);
        assert!(app.total_filtered_count >= app.filtered_stations.len());
        let mut lower = App::new();
        lower.station_search = "wien".to_string();
        lower.update_filtered_stations();
        assert_eq!(app.filtered_stations, lower.filtered_stations);
    }

    #[test]
    fn station_filter_no_match_is_empty() {
        let mut app = App::new();
        app.station_search = "zzzz-no-such-station".to_string();
        app.update_filtered_stations();
        assert!(app.filtered_stations.is_empty());
        assert_eq!(app.total_filtered_count, 0);
        assert!(!app.select_station());
    }

    #[test]
    fn enter_and_exit_station_select() {
        let mut app = App::new();
        app.station_search = "stale".to_string();
        app.enter_station_select();
        assert_eq!(app.mode, AppMode::StationSelect);
        assert!(app.station_search.is_empty());
        assert_eq!(app.station_list_state.selected(), Some(0));
        app.exit_station_select();
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn select_station_commits_highlighted_entry() {
        let mut app = App::new();
        app.enter_station_select();
        app.station_list_state.select(Some(1));
        let (id, name) = app.filtered_stations[1].clone();
        assert!(app.select_station());
        assert_eq!(
            (app.station_id.clone(), app.station_name.clone()),
            (id, name)
        );
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn select_station_out_of_range_is_rejected() {
        let mut app = App::new();
        app.enter_station_select();
        app.station_list_state.select(Some(9999));
        let before = app.station_id.clone();
        assert!(!app.select_station());
        assert_eq!(app.station_id, before);
        assert_eq!(app.mode, AppMode::StationSelect);
    }

    #[test]
    fn select_relative_noop_on_empty() {
        let mut app = App::new();
        app.select_relative(1);
        assert_eq!(app.selected_train_index, None);
        assert_eq!(app.selected_train_id, None);
    }

    #[test]
    fn select_relative_starts_at_first_regardless_of_direction() {
        let mut app = app_with_trains(3);
        app.select_relative(-1);
        assert_eq!(app.selected_train_index, Some(0));
        assert_eq!(app.selected_train_id.as_deref(), Some("t0"));
    }

    #[test]
    fn select_relative_clamps_and_tracks_id_and_resets_scroll() {
        let mut app = app_with_trains(3);
        app.select_relative(1);
        app.detail_scroll = 9;
        app.select_relative(1);
        assert_eq!(app.selected_train_index, Some(1));
        assert_eq!(app.detail_scroll, 0);
        app.select_relative(10);
        assert_eq!(app.selected_train_index, Some(2));
        assert_eq!(app.selected_train_id.as_deref(), Some("t2"));
        app.select_relative(-10);
        assert_eq!(app.selected_train_index, Some(0));
    }

    #[test]
    fn delay_handles_timezone_offsets_and_bad_input() {
        let mut i = train("x");
        i.scheduled = "2024-01-01T10:00:00+01:00".into();
        i.expected = Some("2024-01-01T09:15:00+00:00".into()); // 10:15 local => +15
        assert_eq!(calculate_delay(&i), Some(15));
        i.expected = Some("garbage".into());
        assert_eq!(calculate_delay(&i), None);
        i.expected = Some("2024-01-01T10:05:00+01:00".into());
        i.scheduled = "garbage".into();
        assert_eq!(calculate_delay(&i), None);
    }

    #[test]
    fn delay_truncates_to_whole_minutes() {
        let mut i = train("x");
        i.expected = Some("2024-01-01T10:00:59+01:00".into());
        assert_eq!(calculate_delay(&i), None); // 59s => 0 whole minutes
    }

    #[test]
    fn format_time_is_hh_mm_in_local_time() {
        use chrono::Timelike;
        let formatted = format_time("2024-06-01T12:34:56+00:00");
        let local: DateTime<Local> = DateTime::parse_from_rfc3339("2024-06-01T12:34:56+00:00")
            .unwrap()
            .into();
        assert_eq!(
            formatted,
            format!("{:02}:{:02}", local.hour(), local.minute())
        );
    }

    #[test]
    fn ws_url_encodes_station_id() {
        let url = build_ws_url("1&contentType=arrival #x", &ContentType::Departure, 1);
        assert!(url.contains("stationId=1%26contentType%3Darrival%20%23x&contentType=departure"));
        assert_eq!(
            url.matches("contentType=").count(),
            1,
            "no injected parameter"
        );
    }

    #[test]
    fn default_station_name_matches_station_list() {
        let app = App::new();
        assert_eq!(app.stations.get(&app.station_id), Some(&app.station_name));
    }

    #[test]
    fn starts_without_error() {
        assert_eq!(App::new().last_error, None);
    }

    #[test]
    fn normalize_folds_umlauts_punctuation_and_abbreviations() {
        assert_eq!(normalize("St.Pölten Hbf"), "st polten hbf");
        assert_eq!(normalize("Sankt Poelten Hauptbahnhof"), "st polten hbf");
        assert_eq!(normalize("Linz/Donau"), "linz donau");
        assert_eq!(normalize("Gießhübl"), "giesshubl");
        assert_eq!(normalize("  Wien   Westbahnhof "), "wien westbahnhof");
        assert_eq!(normalize("Wien Bahnhof"), "wien bf");
        assert_eq!(normalize("..."), "");
    }

    fn search(query: &str) -> Vec<String> {
        let mut app = App::new();
        app.station_search = query.to_string();
        app.update_filtered_stations();
        app.filtered_stations.into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn search_ignores_umlauts_and_spelling_variants() {
        assert_eq!(
            search("modling").first().map(String::as_str),
            Some("Mödling")
        );
        assert_eq!(
            search("moedling").first().map(String::as_str),
            Some("Mödling")
        );
        assert!(search("sankt polten").iter().any(|n| n == "St.Pölten Hbf"));
        assert!(search("st. pölten").iter().any(|n| n == "St.Pölten Hbf"));
    }

    #[test]
    fn search_ranks_prefix_then_word_start_then_substring() {
        let results = search("graz");
        assert_eq!(results[0], "Graz Hbf", "main station first");
        let pos = |name: &str| results.iter().position(|n| n == name).unwrap();
        assert!(
            pos("Graz Puntigam") < pos("Hart bei Graz"),
            "prefix before word"
        );
        assert!(pos("Flughafen Graz Feldkirchen") < results.len());

        let results = search("linz");
        assert_eq!(results[0], "Linz Hbf");
        assert_eq!(results.last().map(String::as_str), Some("Glinzendorf"));
    }

    #[test]
    fn empty_search_lists_recent_stations_first() {
        let mut app = App::new();
        app.recent_stations = vec!["8101114".into(), "8103172".into()];
        app.enter_station_select();
        assert_eq!(app.filtered_stations[0].1, "Salzburg Hbf");
        assert_eq!(app.filtered_stations[1].1, "Graz Hbf");
        assert_eq!(app.filtered_stations.len(), STATION_LIST_LEN);
        let ids: Vec<_> = app.filtered_stations.iter().map(|(id, _)| id).collect();
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "recents are not listed twice");
    }

    #[test]
    fn selecting_stations_keeps_a_short_deduplicated_recent_list() {
        let mut app = App::new();
        for query in ["graz hbf", "linz hbf", "graz hbf"] {
            app.enter_station_select();
            app.station_search = query.into();
            app.update_filtered_stations();
            assert!(app.select_station());
        }
        let names: Vec<_> = app
            .recent_stations
            .iter()
            .map(|id| app.stations[id].as_str())
            .collect();
        assert_eq!(names, ["Graz Hbf", "Linz Hbf"]);

        let ids: Vec<String> = app.stations.keys().take(20).cloned().collect();
        for id in ids {
            app.filtered_stations = vec![(id, String::new())];
            app.station_list_state.select(Some(0));
            assert!(app.select_station());
        }
        assert_eq!(app.recent_stations.len(), RECENT_LEN);
    }

    #[test]
    fn restore_applies_saved_settings_and_skips_unknown_stations() {
        let mut app = App::new();
        app.restore(&Config {
            language: Some(Lang::En),
            station: Some("8101114".into()),
            board: Some(ContentType::Arrival),
            recent: vec!["nope".into(), "8103172".into()],
        });
        assert_eq!(app.lang, Lang::En);
        assert!(app.lang_chosen);
        assert_eq!(app.station_name, "Salzburg Hbf");
        assert_eq!(app.content_type, ContentType::Arrival);
        assert_eq!(app.recent_stations, ["8103172"]);

        let mut app = App::new();
        let before = app.station_id.clone();
        app.restore(&Config {
            station: Some("nope".into()),
            ..Config::default()
        });
        assert_eq!(app.station_id, before);
        assert!(!app.lang_chosen);
    }

    #[test]
    fn to_config_saves_language_only_once_chosen() {
        let mut app = App::new();
        assert_eq!(app.to_config().language, None);
        assert_eq!(app.to_config().station.as_deref(), Some("8101001"));
        assert_eq!(app.to_config().board, Some(ContentType::Departure));
        app.lang_chosen = true;
        assert_eq!(app.to_config().language, Some(app.lang));
    }

    #[test]
    fn resolve_station_by_id_name_or_best_match() {
        let app = App::new();
        assert_eq!(app.resolve_station("8101114").as_deref(), Some("8101114"));
        assert_eq!(
            app.resolve_station(" salzburg hbf ").as_deref(),
            Some("8101114")
        );
        let linz = app.resolve_station("linz").unwrap();
        assert_eq!(app.stations[&linz], "Linz Hbf");
        assert_eq!(app.resolve_station("zzzz-nowhere"), None);
        assert_eq!(app.resolve_station("..."), None);
    }

    #[test]
    fn set_station_clears_filter_and_rejects_unknown_ids() {
        let mut app = App::new();
        app.filter = "rjx".into();
        assert!(!app.set_station("nope"));
        assert_eq!(app.filter, "rjx");
        assert!(app.set_station("8101114"));
        assert!(app.filter.is_empty());
    }

    #[test]
    fn filter_matches_line_destination_and_stops() {
        let mut app = App::new();
        let mut a = train("a");
        a.line = Some("RJX".into());
        a.destination = Some(crate::model::Destination {
            default: "Salzburg Hbf".into(),
        });
        let mut b = train("b");
        b.via = Some(crate::model::Destination {
            default: "St. Pölten Hbf ~ Linz Hbf".into(),
        });
        let mut c = train("c");
        c.prioritized_vias = Some(vec!["Mödling".into()]);
        app.items = vec![a, b, c];
        let ids = |app: &App| -> Vec<String> {
            app.visible_items().iter().map(|t| t.id.clone()).collect()
        };

        assert_eq!(ids(&app), ["a", "b", "c"]);
        app.filter = "rjx".into();
        assert_eq!(ids(&app), ["a"]);
        app.filter = "salzburg".into();
        assert_eq!(ids(&app), ["a"]);
        app.filter = "polten".into();
        assert_eq!(ids(&app), ["b"]);
        app.filter = "modling".into();
        assert_eq!(ids(&app), ["c"]);
        app.filter = "  ".into();
        assert_eq!(ids(&app).len(), 3, "blank filter shows everything");
    }

    #[test]
    fn selection_indexes_the_filtered_board() {
        let mut app = app_with_trains(5);
        app.filter = "t3".into();
        app.select_index(0);
        assert_eq!(app.selected_train_id.as_deref(), Some("t3"));
        assert_eq!(app.selected_train().map(|t| t.id.as_str()), Some("t3"));
        app.filter.clear();
        app.resync_selection();
        assert_eq!(app.selected_train_index, Some(3));
        app.select_last();
        assert_eq!(app.selected_train_index, Some(4));
    }
}
