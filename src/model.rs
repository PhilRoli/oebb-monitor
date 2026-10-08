//! Deserialisation types mirroring the ÖBB WebSocket JSON payloads.
//!
//! Only the fields the UI consumes are modelled; everything else in the feed is
//! ignored. Most fields are optional because the upstream data is inconsistent
//! between trains, stations, and departures vs. arrivals.

use serde::Deserialize;

/// A localisable display string. The feed nests human-readable text under a
/// `default` key (alongside other locales we don't use).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Destination {
    pub default: String,
}

/// A single departure or arrival in the board.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrainItem {
    pub id: String,
    pub train: String,
    pub line: Option<String>,
    pub product: Option<String>,
    pub scheduled: String,
    pub expected: Option<String>,
    pub destination: Option<Destination>,
    pub origin: Option<Destination>,
    pub track: Option<String>,
    pub sector: Option<String>,
    pub remarks: Option<Vec<Remark>>,
    pub via: Option<Destination>,
    #[serde(rename = "prioritizedVias")]
    pub prioritized_vias: Option<Vec<String>>,
    pub operator: Option<String>,
    pub formation: Option<Vec<Formation>>,
}

/// One wagon (or the locomotive) in a train's physical composition, including
/// its position sector, onboard amenity icons, and car type (which matters for
/// night trains: sleeper vs. couchette vs. seated).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Formation {
    /// `None` marks the locomotive; otherwise the printed wagon number.
    #[serde(rename = "wagonNumber")]
    pub wagon_number: Option<String>,
    pub icons: Option<Vec<String>>,
    pub sector: Option<String>,
    pub destination: Option<String>,
    /// Car category, e.g. `engine`, `sleeper`, `couchette`, `passenger`,
    /// `car` (car-carrier), `restaurant`.
    #[serde(rename = "type")]
    pub car_type: Option<Vec<String>>,
    /// Whether the wagon is closed / not boardable.
    pub closed: Option<bool>,
    /// Layout code encoding the passenger class, e.g. `W_1`, `W_2`, `W_1_B`
    /// (1st + Business), `W_C_1` (Comfort + 1st), `TW_B_1`. Decoded by the UI.
    pub symbol: Option<String>,
}

/// A free-text remark attached to a train (delays, cancellations, etc.).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Remark {
    pub text: Destination,
}

/// Station-wide notices share the exact shape of a [`Remark`].
pub type SpecialNotice = Remark;

/// The payload body of an `update` message: the current board plus notices.
#[derive(Debug, Clone, Deserialize)]
pub struct TrainData {
    pub departures: Option<Vec<TrainItem>>,
    pub arrivals: Option<Vec<TrainItem>>,
    #[serde(rename = "specialNotices")]
    pub special_notices: Option<Vec<SpecialNotice>>,
}

/// Wrapper around the data carried by an `update` message.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateParams {
    pub data: TrainData,
}

/// A top-level WebSocket message. Only `method == "update"` carries board data;
/// the server also sends others (`loadUrl`, `keepAlive`, ...) whose `params`
/// have different shapes, so `params` stays untyped until the method is known.
#[derive(Debug, Clone, Deserialize)]
pub struct WsMessage {
    pub method: Option<String>,
    pub params: Option<serde_json::Value>,
}

impl WsMessage {
    /// `None` for every method other than `update`; for `update`, the typed
    /// payload or the error explaining why it didn't match the expected shape.
    pub fn into_update(self) -> Option<Result<UpdateParams, serde_json::Error>> {
        if self.method.as_deref() != Some("update") {
            return None;
        }
        let params = self.params.unwrap_or(serde_json::Value::Null);
        Some(serde_json::from_value(params))
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    const DEPARTURES: &str = include_str!("../tests/fixtures/update_departures.json");
    const ARRIVALS: &str = include_str!("../tests/fixtures/update_arrivals.json");
    const NON_UPDATE: &str = include_str!("../tests/fixtures/non_update.json");

    #[test]
    fn parses_full_departure_payload() {
        let msg: WsMessage = serde_json::from_str(DEPARTURES).unwrap();
        assert_eq!(msg.method.as_deref(), Some("update"));
        let data = msg.into_update().unwrap().unwrap().data;
        assert!(data.arrivals.is_none());
        let deps = data.departures.unwrap();
        assert_eq!(deps.len(), 2);

        let t = &deps[0];
        assert_eq!(t.id, "rj-65-20240101-1000");
        assert_eq!(t.train, "RJX 65");
        assert_eq!(t.line.as_deref(), Some("RJX"));
        assert_eq!(t.destination.as_ref().unwrap().default, "Salzburg Hbf");
        assert_eq!(t.track.as_deref(), Some("7"));
        assert_eq!(t.prioritized_vias.as_ref().unwrap().len(), 2);
        assert_eq!(t.remarks.as_ref().unwrap()[0].text.default, "Verspätung");

        let formation = t.formation.as_ref().unwrap();
        assert_eq!(formation.len(), 2);
        assert!(formation[0].wagon_number.is_none(), "loco has no number");
        assert_eq!(
            formation[0].car_type.as_deref(),
            Some(&["engine".to_string()][..])
        );
        assert_eq!(formation[1].wagon_number.as_deref(), Some("21"));
        assert_eq!(formation[1].closed, Some(false));
        assert_eq!(formation[1].symbol.as_deref(), Some("W_1"));

        let notices = data.special_notices.unwrap();
        assert_eq!(notices[0].text.default, "Bauarbeiten");
    }

    #[test]
    fn minimal_train_only_needs_id_train_scheduled() {
        let msg: WsMessage = serde_json::from_str(DEPARTURES).unwrap();
        let deps = msg.into_update().unwrap().unwrap().data.departures.unwrap();
        let t = &deps[1];
        assert_eq!(t.train, "REX 1");
        assert!(t.expected.is_none() && t.destination.is_none() && t.formation.is_none());
    }

    #[test]
    fn parses_arrival_payload() {
        let msg: WsMessage = serde_json::from_str(ARRIVALS).unwrap();
        let data = msg.into_update().unwrap().unwrap().data;
        assert!(data.departures.is_none());
        assert!(data.special_notices.is_none());
        let arr = data.arrivals.unwrap();
        assert_eq!(arr[0].origin.as_ref().unwrap().default, "Graz Hbf");
    }

    #[test]
    fn non_update_message_has_no_params() {
        let msg: WsMessage = serde_json::from_str(NON_UPDATE).unwrap();
        assert_eq!(msg.method.as_deref(), Some("hello"));
        assert!(msg.params.is_none());
        assert!(msg.into_update().is_none());
    }

    #[test]
    fn real_non_update_messages_are_ignored_not_errors() {
        // Captured from the live endpoint.
        for raw in [
            r#"{"jsonrpc":"2.0","method":"loadUrl","id":1,"params":{"urls":["departure-mobile"]}}"#,
            r#"{"jsonrpc":"2.0","method":"keepAlive","params":{"timestamp":"2026-10-08T10:54:33.132Z"}}"#,
        ] {
            let msg: WsMessage = serde_json::from_str(raw).unwrap();
            assert!(msg.into_update().is_none(), "{raw}");
        }
    }

    #[test]
    fn update_with_wrong_shape_is_an_error() {
        for raw in [
            r#"{"method":"update"}"#,
            r#"{"method":"update","params":{"urls":[]}}"#,
            r#"{"method":"update","params":{"data":{"departures":"nope"}}}"#,
        ] {
            let msg: WsMessage = serde_json::from_str(raw).unwrap();
            assert!(matches!(msg.into_update(), Some(Err(_))), "{raw}");
        }
    }

    #[test]
    fn update_ignores_jsonrpc_envelope_and_unknown_item_fields() {
        let raw = r#"{"jsonrpc":"2.0","method":"update","id":2,"params":{"data":{"departures":[
            {"id":"1634-PB","train":"1634","class":"S","availableAt":"-PT30M",
             "scheduled":"2026-10-08T10:53:00Z","prioritizedVias":[]}]}}}"#;
        let msg: WsMessage = serde_json::from_str(raw).unwrap();
        let deps = msg.into_update().unwrap().unwrap().data.departures.unwrap();
        assert_eq!(deps[0].id, "1634-PB");
    }

    #[test]
    fn missing_required_fields_is_an_error() {
        let bad = r#"{"method":"update","params":{"data":{"departures":[{"train":"X"}]}}}"#;
        let msg: WsMessage = serde_json::from_str(bad).unwrap();
        assert!(matches!(msg.into_update(), Some(Err(_))));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(serde_json::from_str::<WsMessage>("not json").is_err());
    }
}
