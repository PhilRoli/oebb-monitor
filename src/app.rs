//! Application state and the pure logic that operates on it.
//!
//! [`App`] is the single source of truth shared (behind a mutex) between the UI
//! loop and the WebSocket task. This module also holds small, side-effect-free
//! helpers ([`format_time`], [`calculate_delay`], [`build_ws_url`]) that are
//! unit-tested in isolation.

use chrono::{DateTime, Local};
use ratatui::widgets::ListState;
use std::collections::HashMap;

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

/// The complete application state.
pub struct App {
    pub content_type: ContentType,
    pub station_id: String,
    pub station_name: String,
    pub items: Vec<TrainItem>,
    pub special_notices: Vec<SpecialNotice>,
    pub last_update: Option<DateTime<Local>>,
    pub connection: ConnectionState,
    pub mode: AppMode,
    pub stations: HashMap<String, String>,
    pub all_stations_sorted: Vec<(String, String)>,
    pub filtered_stations: Vec<(String, String)>,
    pub total_filtered_count: usize,
    pub station_search: String,
    pub station_list_state: ListState,
    pub max_pages: usize,
    pub selected_train_index: Option<usize>,
    pub selected_train_id: Option<String>,
    pub detail_scroll: u16,
    pub lang: Lang,
}

impl App {
    /// Build the initial state, loading the embedded station list and defaulting
    /// to departures at Wien Westbahnhof.
    pub fn new() -> Self {
        let mut app = Self {
            content_type: ContentType::Departure,
            station_id: "8101001".to_string(),
            station_name: "Wien Westbahnhof".to_string(),
            items: Vec::new(),
            special_notices: Vec::new(),
            last_update: None,
            connection: ConnectionState::Connecting,
            mode: AppMode::Normal,
            stations: HashMap::new(),
            all_stations_sorted: Vec::new(),
            filtered_stations: Vec::new(),
            total_filtered_count: 0,
            station_search: String::new(),
            station_list_state: ListState::default(),
            max_pages: 5,
            selected_train_index: None,
            selected_train_id: None,
            detail_scroll: 0,
            lang: Lang::initial(),
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

        let mut sorted: Vec<(String, String)> = app
            .stations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        sorted.sort_by(|a, b| a.1.cmp(&b.1));
        app.all_stations_sorted = sorted;

        app
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
    /// capping the visible list at 20 entries while tracking the total match
    /// count for the header.
    pub fn update_filtered_stations(&mut self) {
        if self.station_search.is_empty() {
            self.total_filtered_count = self.all_stations_sorted.len();
            let take = self.all_stations_sorted.len().min(20);
            self.filtered_stations = self.all_stations_sorted[..take].to_vec();
        } else {
            let search_lower = self.station_search.to_lowercase();
            let mut matches: Vec<(String, String)> = self
                .stations
                .iter()
                .filter(|(_, name)| name.to_lowercase().contains(&search_lower))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            matches.sort_by(|a, b| a.1.cmp(&b.1));
            self.total_filtered_count = matches.len();
            matches.truncate(20);
            self.filtered_stations = matches;
        }

        if !self.filtered_stations.is_empty() {
            self.station_list_state.select(Some(0));
        }
    }

    /// Commit the highlighted station as the active one. Returns `true` if a
    /// selection was made (so the caller can trigger a reconnect).
    pub fn select_station(&mut self) -> bool {
        if let Some(selected) = self.station_list_state.selected() {
            if selected < self.filtered_stations.len() {
                let (id, name) = &self.filtered_stations[selected];
                self.station_id = id.clone();
                self.station_name = name.clone();
                self.exit_station_select();
                return true;
            }
        }
        false
    }

    /// Move the train selection by `delta`, clamped to the bounds of the list,
    /// keeping the tracked id in sync and resetting the detail scroll. Unifies
    /// the up/down handling for both the main list and the detail view.
    pub fn select_relative(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let new = match self.selected_train_index {
            Some(idx) => (idx as i32 + delta).clamp(0, self.items.len() as i32 - 1) as usize,
            None => 0,
        };
        self.selected_train_index = Some(new);
        self.selected_train_id = self.items.get(new).map(|t| t.id.clone());
        self.detail_scroll = 0;
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
        station_id, content, page
    )
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
}
