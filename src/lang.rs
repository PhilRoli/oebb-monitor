//! UI language support: German (default) and English.
//!
//! Only the app's own chrome is translated — the live feed data (station names,
//! destinations, remarks, special notices) is delivered by ÖBB in German and is
//! shown as-is. [`Lang`] selects between two static [`Tr`] tables of strings and
//! provides language-aware labels for wagon types, classes, and amenity icons.

/// The selected UI language.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    De,
    En,
}

impl Lang {
    /// Short code used in the config file.
    pub fn code(&self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
        }
    }

    /// Parse a code (`de`/`en`) back into a [`Lang`].
    pub fn from_code(s: &str) -> Option<Lang> {
        match s.trim().to_lowercase().as_str() {
            "de" => Some(Lang::De),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    /// Switch to the other language.
    pub fn toggle(&self) -> Lang {
        match self {
            Lang::De => Lang::En,
            Lang::En => Lang::De,
        }
    }

    /// Detect the language from locale env vars: an English locale yields
    /// [`Lang::En`], anything else (including unset / `C` / `POSIX`) yields German.
    pub fn detect() -> Lang {
        Self::detect_from(|key| std::env::var_os(key).map(|v| v.to_string_lossy().into_owned()))
    }

    fn detect_from(get: impl Fn(&str) -> Option<String>) -> Lang {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(v) = get(key) {
                let v = v.to_lowercase();
                if !v.is_empty() {
                    return if v.starts_with("en") {
                        Lang::En
                    } else {
                        Lang::De
                    };
                }
            }
        }
        Lang::De
    }

    /// Startup language, by precedence: saved config → locale → German.
    pub fn initial() -> Lang {
        crate::config::load_language().unwrap_or_else(Lang::detect)
    }

    /// The string table for this language.
    pub fn tr(&self) -> &'static Tr {
        match self {
            Lang::De => &DE,
            Lang::En => &EN,
        }
    }

    /// Label for a notable wagon car type (night-train cars, restaurant, …), or
    /// `None` for ordinary/unknown types.
    pub fn car_type_label(&self, types: &[String]) -> Option<&'static str> {
        types.iter().find_map(|t| match (self, t.as_str()) {
            (Lang::De, "sleeper") => Some("🛏️ Schlafwagen"),
            (Lang::En, "sleeper") => Some("🛏️ Sleeper"),
            (Lang::De, "couchette") => Some("🛌 Liegewagen"),
            (Lang::En, "couchette") => Some("🛌 Couchette"),
            (Lang::De, "passenger") => Some("🪑 Sitzwagen"),
            (Lang::En, "passenger") => Some("🪑 Seating car"),
            (Lang::De, "car") => Some("🚗 Autotransport"),
            (Lang::En, "car") => Some("🚗 Car carrier"),
            (_, "restaurant") => Some("🍽️ Restaurant"),
            _ => None,
        })
    }

    /// Decode the passenger class(es) from a wagon's `symbol` code, e.g.
    /// `W_1` → "1. Klasse"/"1st class", `W_1_B` → "… + Business". Returns `None`
    /// for symbols carrying no class (locos, sleepers, couchettes, car-carriers).
    pub fn class_label(&self, symbol: &str) -> Option<String> {
        let (first, second) = match self {
            Lang::De => ("1. Klasse", "2. Klasse"),
            Lang::En => ("1st class", "2nd class"),
        };
        let parts: Vec<&str> = symbol
            .split('_')
            .skip(1) // drop the vehicle prefix (W / TW / L)
            .filter_map(|seg| match seg {
                "1" => Some(first),
                "2" => Some(second),
                "B" => Some("Business"),
                "C" => Some("Comfort"),
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(" + "))
    }

    /// Label for an onboard amenity icon code; unknown codes pass through.
    pub fn icon_label(&self, icon: &str) -> String {
        match (self, icon) {
            (Lang::De, "wlan") => "📶 WLAN",
            (Lang::En, "wlan") => "📶 Wi-Fi",
            (Lang::De, "bicycle") => "🚲 Fahrrad",
            (Lang::En, "bicycle") => "🚲 Bicycle",
            (Lang::De, "disabled") => "♿ Rollstuhl",
            (Lang::En, "disabled") => "♿ Wheelchair",
            (_, "bistro") => "🍽️ Bistro",
            (Lang::De, "motherchild") => "👪 Familie",
            (Lang::En, "motherchild") => "👪 Family",
            (Lang::De, "silence") => "🔇 Ruhe",
            (Lang::En, "silence") => "🔇 Quiet",
            (_, other) => return other.to_string(),
        }
        .to_string()
    }
}

/// A table of translatable UI strings. Static instances [`DE`] and [`EN`] hold
/// the two languages.
pub struct Tr {
    // Main board
    pub departures: &'static str,
    pub arrivals: &'static str,
    pub col_time: &'static str,
    pub col_actual: &'static str,
    pub col_delay: &'static str,
    pub col_train: &'static str,
    pub col_line: &'static str,
    pub col_dest: &'static str,
    pub col_from: &'static str,
    pub col_track: &'static str,
    pub col_sector: &'static str,
    pub col_remarks: &'static str,
    pub block_trains: &'static str,
    pub block_notices: &'static str,
    pub block_status: &'static str,
    pub last_update: &'static str,
    pub connecting: &'static str,
    pub connected: &'static str,
    pub connection_failed: &'static str,
    pub error_label: &'static str,
    pub err_connect: &'static str,
    pub err_closed: &'static str,
    pub err_timeout: &'static str,
    pub err_parse: &'static str,
    // Station picker
    pub select_station: &'static str,
    pub search: &'static str,
    pub stations: &'static str,
    pub of: &'static str,
    pub shown: &'static str,
    pub station_help: &'static str,
    // Train detail
    pub train: &'static str,
    pub departure: &'static str,
    pub arrival: &'static str,
    pub delay: &'static str,
    pub min: &'static str,
    pub track: &'static str,
    pub sector: &'static str,
    pub operator: &'static str,
    pub remarks: &'static str,
    pub major_stops: &'static str,
    pub all_stops: &'static str,
    pub formation: &'static str,
    pub locomotive: &'static str,
    pub coach: &'static str,
    pub closed: &'static str,
    pub details: &'static str,
    pub detail_help: &'static str,
    pub no_train: &'static str,
    // Main key hints (bracketed key + suffix, e.g. "[A]" + "nk")
    pub detail: &'static str,
    pub hint_arr: &'static str,
    pub hint_dep: &'static str,
    pub hint_stn: &'static str,
    pub hint_ref: &'static str,
    pub hint_lang: &'static str,
    pub hint_quit: &'static str,
}

static DE: Tr = Tr {
    departures: "ABFAHRTEN",
    arrivals: "ANKÜNFTE",
    col_time: "ZEIT",
    col_actual: "IST",
    col_delay: "VERSP.",
    col_train: "ZUG",
    col_line: "LINIE",
    col_dest: "ZIEL",
    col_from: "VON",
    col_track: "GLEIS",
    col_sector: "SEKTOR",
    col_remarks: "BEMERKUNGEN",
    block_trains: "Züge",
    block_notices: "Hinweise",
    block_status: "Status",
    last_update: "Letzte Aktualisierung: ",
    connecting: "Verbinde...",
    connected: "Verbunden",
    connection_failed: "Verbindung fehlgeschlagen",
    error_label: "Fehler: ",
    err_connect: "Verbindung nicht möglich",
    err_closed: "Verbindung getrennt",
    err_timeout: "Keine Daten empfangen",
    err_parse: "Ungültige Serverdaten",
    select_station: "Station wählen",
    search: "Suche: ",
    stations: "Stationen",
    of: "von",
    shown: "angezeigt",
    station_help: "↑↓: Navigieren | Enter: Auswählen | Esc: Abbrechen",
    train: "Zug",
    departure: "Abfahrt",
    arrival: "Ankunft",
    delay: "Verspätung",
    min: "Min",
    track: "Gleis",
    sector: "Sektor",
    operator: "Betreiber",
    remarks: "Bemerkungen",
    major_stops: "Wichtige Halte",
    all_stops: "Alle Zwischenhalte",
    formation: "Zugformation",
    locomotive: "Lokomotive",
    coach: "Wagen",
    closed: "(geschlossen)",
    details: "Details",
    detail_help: "↑↓: Züge | PgUp/PgDn: Scrollen | Esc/Q: Schließen",
    no_train: "Kein Zug ausgewählt",
    detail: "Detail",
    hint_arr: "nk",
    hint_dep: "ep",
    hint_stn: "tn",
    hint_ref: "ef",
    hint_lang: "ang",
    hint_quit: "uit",
};

static EN: Tr = Tr {
    departures: "DEPARTURES",
    arrivals: "ARRIVALS",
    col_time: "TIME",
    col_actual: "ACT.",
    col_delay: "DELAY",
    col_train: "TRAIN",
    col_line: "LINE",
    col_dest: "DEST.",
    col_from: "FROM",
    col_track: "TRACK",
    col_sector: "SECTOR",
    col_remarks: "REMARKS",
    block_trains: "Trains",
    block_notices: "Notices",
    block_status: "Status",
    last_update: "Last update: ",
    connecting: "Connecting...",
    connected: "Connected",
    connection_failed: "Connection failed",
    error_label: "Error: ",
    err_connect: "Cannot connect",
    err_closed: "Connection lost",
    err_timeout: "No data received",
    err_parse: "Invalid server data",
    select_station: "Select station",
    search: "Search: ",
    stations: "Stations",
    of: "of",
    shown: "shown",
    station_help: "↑↓: Navigate | Enter: Select | Esc: Cancel",
    train: "Train",
    departure: "Departure",
    arrival: "Arrival",
    delay: "Delay",
    min: "min",
    track: "Track",
    sector: "Sector",
    operator: "Operator",
    remarks: "Remarks",
    major_stops: "Major stops",
    all_stops: "All stops",
    formation: "Train formation",
    locomotive: "Locomotive",
    coach: "Coach",
    closed: "(closed)",
    details: "Details",
    detail_help: "↑↓: Trains | PgUp/PgDn: Scroll | Esc/Q: Close",
    no_train: "No train selected",
    detail: "Detail",
    hint_arr: "rr",
    hint_dep: "ep",
    hint_stn: "tn",
    hint_ref: "ef",
    hint_lang: "ang",
    hint_quit: "uit",
};

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn code_roundtrip() {
        assert_eq!(Lang::from_code("de"), Some(Lang::De));
        assert_eq!(Lang::from_code("EN"), Some(Lang::En));
        assert_eq!(Lang::from_code("fr"), None);
        assert_eq!(Lang::De.code(), "de");
        assert_eq!(Lang::En.code(), "en");
        assert_eq!(Lang::De.toggle(), Lang::En);
    }

    #[test]
    fn car_types_localized() {
        let t = |l: Lang, s: &str| l.car_type_label(&[s.to_string()]);
        assert_eq!(t(Lang::De, "sleeper"), Some("🛏️ Schlafwagen"));
        assert_eq!(t(Lang::En, "sleeper"), Some("🛏️ Sleeper"));
        assert_eq!(t(Lang::En, "car"), Some("🚗 Car carrier"));
        assert_eq!(t(Lang::De, "unknown"), None);
    }

    #[test]
    fn class_localized() {
        assert_eq!(Lang::De.class_label("W_1").as_deref(), Some("1. Klasse"));
        assert_eq!(Lang::En.class_label("W_1").as_deref(), Some("1st class"));
        assert_eq!(
            Lang::En.class_label("W_C_1").as_deref(),
            Some("Comfort + 1st class")
        );
        assert_eq!(Lang::En.class_label("W_Schlaf"), None);
    }

    #[test]
    fn icons_localized() {
        assert_eq!(Lang::De.icon_label("wlan"), "📶 WLAN");
        assert_eq!(Lang::En.icon_label("wlan"), "📶 Wi-Fi");
        assert_eq!(Lang::En.icon_label("unknown_code"), "unknown_code");
    }

    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let pairs = pairs.to_vec();
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detect_english_locales() {
        assert_eq!(Lang::detect_from(env(&[("LANG", "en_US.UTF-8")])), Lang::En);
        assert_eq!(Lang::detect_from(env(&[("LC_ALL", "EN_GB")])), Lang::En);
    }

    #[test]
    fn detect_defaults_to_german() {
        assert_eq!(Lang::detect_from(env(&[])), Lang::De);
        assert_eq!(Lang::detect_from(env(&[("LANG", "C")])), Lang::De);
        assert_eq!(Lang::detect_from(env(&[("LANG", "de_AT.UTF-8")])), Lang::De);
        assert_eq!(Lang::detect_from(env(&[("LANG", "")])), Lang::De);
    }

    #[test]
    fn detect_precedence_lc_all_over_lang() {
        let e = env(&[("LC_ALL", "de_DE"), ("LANG", "en_US")]);
        assert_eq!(Lang::detect_from(e), Lang::De);
        let e = env(&[("LC_ALL", ""), ("LANG", "en_US")]);
        assert_eq!(Lang::detect_from(e), Lang::En);
    }

    #[test]
    fn toggle_is_an_involution() {
        for l in [Lang::De, Lang::En] {
            assert_eq!(l.toggle().toggle(), l);
            assert_ne!(l.toggle(), l);
        }
    }

    #[test]
    fn string_tables_match_language_and_differ() {
        assert!(std::ptr::eq(Lang::De.tr(), &DE));
        assert!(std::ptr::eq(Lang::En.tr(), &EN));
        assert_ne!(DE.departures, EN.departures);
        assert!(!DE.departures.is_empty() && !EN.departures.is_empty());
        assert!(!DE.connection_failed.is_empty() && !EN.connection_failed.is_empty());
        for tr in [&DE, &EN] {
            for s in [
                tr.error_label,
                tr.err_connect,
                tr.err_closed,
                tr.err_timeout,
                tr.err_parse,
            ] {
                assert!(!s.is_empty());
            }
        }
        assert_ne!(DE.err_timeout, EN.err_timeout);
    }

    #[test]
    fn class_label_combinations() {
        assert_eq!(Lang::De.class_label("W_2").as_deref(), Some("2. Klasse"));
        assert_eq!(
            Lang::En.class_label("W_1_B").as_deref(),
            Some("1st class + Business")
        );
        assert_eq!(
            Lang::En.class_label("TW_B_1").as_deref(),
            Some("Business + 1st class")
        );
        assert_eq!(Lang::En.class_label("L"), None);
        assert_eq!(Lang::En.class_label(""), None);
    }

    #[test]
    fn car_type_first_notable_wins() {
        let types = vec![
            "unknown".to_string(),
            "couchette".to_string(),
            "sleeper".to_string(),
        ];
        assert_eq!(Lang::En.car_type_label(&types), Some("🛌 Couchette"));
        assert_eq!(Lang::En.car_type_label(&[]), None);
        assert_eq!(
            Lang::De.car_type_label(&["restaurant".to_string()]),
            Lang::En.car_type_label(&["restaurant".to_string()])
        );
    }
}
