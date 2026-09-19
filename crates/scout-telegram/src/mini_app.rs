//! The ways into the trip page's Mini App from the chat: the button beside
//! the input, and the one under a reply that changed a trip.

use scout_api::TraceFrame;
use std::collections::HashMap;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MenuButton, WebAppInfo};
use url::Url;

/// The tools that change a trip, by the name the trace carries.
///
/// A reply that only *talked* about a trip gets no button: `show_trip`
/// is left out, as is `delete_trip` — a button to open a trip that just
/// went would open the first of the others instead.
const TRIP_WRITERS: &[&str] = &[
    "add_trip_segment",
    "add_trip_item",
    "add_trip_option",
    "choose_trip_option",
    "update_trip_segment",
    "update_trip_item",
    "note_trip_item",
    "drop_trip_segment",
    "finalise_trip",
    "keep_trip",
];

/// Which trips a run changed, read off its trace as it streams past.
///
/// The trace rather than the answer's words or the trips' timestamps: it
/// is the one record of what the run actually did, it names the trip in
/// the tool's own argument, and a call only counts once it finished
/// without failing — a refused `add_trip_item` changed nothing.
#[derive(Debug, Default)]
pub struct TouchedTrips {
    open: HashMap<i64, String>,
    touched: Vec<String>,
}

impl TouchedTrips {
    pub fn see(&mut self, frame: &TraceFrame) {
        match frame {
            TraceFrame::Started { seq, tool, args, .. } if TRIP_WRITERS.contains(&tool.as_str()) => {
                if let Some(trip) = args.get("trip").and_then(|t| t.as_str()).filter(|t| !t.trim().is_empty()) {
                    self.open.insert(*seq, trip.trim().to_string());
                }
            }
            TraceFrame::Finished { seq, status, .. } => {
                if let Some(trip) = self.open.remove(seq) {
                    if status == "ok" {
                        self.touched.retain(|t| !t.eq_ignore_ascii_case(&trip));
                        self.touched.push(trip);
                    }
                }
            }
            _ => {}
        }
    }

    /// The trip the reply should open: the last one changed, when a run
    /// touched several — it is the one the answer ends on.
    pub fn last(&self) -> Option<&str> {
        self.touched.last().map(String::as_str)
    }
}

/// Where the Mini App starts, from the site's own address. Only https:
/// Telegram refuses anything else, and a local run with an http base URL
/// simply has no Mini App.
pub fn launch_url(base_url: &str) -> Option<Url> {
    let base = Url::parse(base_url.trim()).ok()?;
    if base.scheme() != "https" {
        return None;
    }
    base.join("/tg").ok()
}

/// What Telegram draws on a button, which it cuts at its own width anyway;
/// this keeps a long trip name from being the whole of it.
const BUTTON_NAME_CHARS: usize = 28;

/// "Open <trip>", opening the Mini App on that trip.
pub fn open_trip_markup(launch: &Url, trip: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![open_trip_button(launch, trip)]])
}

/// The button itself, for a keyboard that has other rows.
pub fn open_trip_button(launch: &Url, trip: &str) -> InlineKeyboardButton {
    let mut url = launch.clone();
    url.query_pairs_mut().clear().append_pair("trip", trip);
    let name: String = if trip.chars().count() > BUTTON_NAME_CHARS {
        trip.chars().take(BUTTON_NAME_CHARS - 1).chain(['…']).collect()
    } else {
        trip.to_string()
    };
    InlineKeyboardButton::web_app(format!("Open {name}"), WebAppInfo { url })
}

/// The button beside the input, for every private chat with the bot.
pub fn menu_button(launch: &Url) -> MenuButton {
    MenuButton::WebApp { text: "Trips".to_string(), web_app: WebAppInfo { url: launch.clone() } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::InlineKeyboardButtonKind;

    fn started(seq: i64, tool: &str, trip: &str) -> TraceFrame {
        TraceFrame::Started { seq, tool: tool.into(), args: serde_json::json!({ "trip": trip }), nested: false }
    }

    fn finished(seq: i64, status: &str) -> TraceFrame {
        TraceFrame::Finished { seq, duration_ms: 1, status: status.into(), detail: None }
    }

    #[test]
    fn a_trip_counts_once_a_tool_that_changes_it_has_finished() {
        let mut t = TouchedTrips::default();
        t.see(&started(1, "add_trip_item", "Hong Kong"));
        assert_eq!(t.last(), None, "started is not done");
        t.see(&finished(1, "ok"));
        assert_eq!(t.last(), Some("Hong Kong"));
    }

    #[test]
    fn a_failed_call_a_read_and_a_delete_leave_no_button() {
        let mut t = TouchedTrips::default();
        t.see(&started(1, "add_trip_item", "Lisbon"));
        t.see(&finished(1, "failed"));
        t.see(&started(2, "show_trip", "Lisbon"));
        t.see(&finished(2, "ok"));
        t.see(&started(3, "delete_trip", "Lisbon"));
        t.see(&finished(3, "ok"));
        t.see(&started(4, "search_flights", "Lisbon"));
        t.see(&finished(4, "ok"));
        assert_eq!(t.last(), None);
    }

    #[test]
    fn the_last_trip_changed_is_the_one_the_button_opens() {
        let mut t = TouchedTrips::default();
        for (seq, trip) in [(1, "Lisbon"), (2, "Hong Kong"), (3, "lisbon")] {
            t.see(&started(seq, "note_trip_item", trip));
            t.see(&finished(seq, "ok"));
        }
        assert_eq!(t.last(), Some("lisbon"));
    }

    #[test]
    fn the_mini_app_lives_on_the_site_and_only_over_https() {
        assert_eq!(launch_url("https://goodscout.fyi").unwrap().as_str(), "https://goodscout.fyi/tg");
        assert_eq!(launch_url("https://goodscout.fyi/").unwrap().as_str(), "https://goodscout.fyi/tg");
        assert!(launch_url("http://localhost:8080").is_none());
        assert!(launch_url("").is_none());
    }

    #[test]
    fn the_button_opens_the_mini_app_on_its_trip() {
        let launch = launch_url("https://goodscout.fyi").unwrap();
        let markup = open_trip_markup(&launch, "Hong Kong & Macau");
        let button = &markup.inline_keyboard[0][0];
        assert_eq!(button.text, "Open Hong Kong & Macau");
        let InlineKeyboardButtonKind::WebApp(info) = &button.kind else { panic!("not a web app button") };
        assert_eq!(info.url.as_str(), "https://goodscout.fyi/tg?trip=Hong+Kong+%26+Macau");

        let long = open_trip_markup(&launch, &"Very long trip name ".repeat(4));
        assert_eq!(long.inline_keyboard[0][0].text.chars().count(), "Open ".len() + BUTTON_NAME_CHARS);
    }
}
