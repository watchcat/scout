//! A booking that arrived, decided from the chat: Add and Ignore under the
//! line that says it came, instead of a trip to the Trips tab.

use scout_api::Arrival;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// What a button under a nudge asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Add(i64),
    Ignore(i64),
}

/// The callback data a button carries: `arr:add:<id>` / `arr:ign:<id>`.
/// Short by design — Telegram allows 64 bytes — and a shape nothing else
/// on this bot produces, so `parse` can refuse everything else.
pub fn callback_data(decision: Decision) -> String {
    match decision {
        Decision::Add(id) => format!("arr:add:{id}"),
        Decision::Ignore(id) => format!("arr:ign:{id}"),
    }
}

pub fn parse_callback(data: &str) -> Option<Decision> {
    let (verb, id) = data.strip_prefix("arr:")?.split_once(':')?;
    let id: i64 = id.parse().ok()?;
    match verb {
        "add" => Some(Decision::Add(id)),
        "ign" => Some(Decision::Ignore(id)),
        _ => None,
    }
}

/// The mail a queued nudge is about, from the key the worker filed it
/// under (`inbox::nudge` writes `inbox:<mail_id>`). `None` for every other
/// row in the queue — a mirrored turn, a reminder — which get no buttons.
pub fn mail_of_key(turn_key: &str) -> Option<i64> {
    turn_key.strip_prefix("inbox:")?.parse().ok()
}

/// Longest a booking's name is on a button; Telegram trims past its own
/// width anyway, and two of these share a row.
const LABEL_CHARS: usize = 24;

fn short(title: &str) -> String {
    if title.chars().count() > LABEL_CHARS {
        title.chars().take(LABEL_CHARS - 1).chain(['…']).collect()
    } else {
        title.to_string()
    }
}

/// Add and Ignore for each booking still pending on the mail, then the
/// Mini App's "Open <trip>" when there is one to offer. `None` when there
/// is nothing to press: a nudge whose bookings were all decided, or one
/// that says a file held no booking.
///
/// One booking names where Add puts it — the trip it was placed on, or a
/// new one — because that is the question the reader has. Several share
/// a trip, so each names itself instead.
pub fn arrival_markup(arrivals: &[Arrival], open: Option<InlineKeyboardButton>) -> Option<InlineKeyboardMarkup> {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for arrival in arrivals {
        let add = if arrivals.len() == 1 {
            match &arrival.trip_name {
                Some(trip) => format!("Add to {}", short(trip)),
                None => "Add to a new trip".to_string(),
            }
        } else {
            format!("Add {}", short(arrival.title.as_deref().unwrap_or("booking")))
        };
        rows.push(vec![
            InlineKeyboardButton::callback(add, callback_data(Decision::Add(arrival.id))),
            InlineKeyboardButton::callback("Ignore", callback_data(Decision::Ignore(arrival.id))),
        ]);
    }
    if let Some(open) = open {
        rows.push(vec![open]);
    }
    (!rows.is_empty()).then(|| InlineKeyboardMarkup::new(rows))
}

/// The line appended to the nudge once a button was pressed, so the
/// message reads as a record of what happened rather than a request that
/// is still open.
pub fn decided_line(decision: Decision, title: Option<&str>, trip: Option<&str>) -> String {
    let what = title.unwrap_or("The booking");
    match (decision, trip) {
        (Decision::Add(_), Some(trip)) => format!("✓ {what} added to {trip}."),
        (Decision::Add(_), None) => format!("✓ {what} added."),
        (Decision::Ignore(_), _) => format!("✗ {what} ignored. It is under Other mail for thirty days."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::InlineKeyboardButtonKind;

    fn arrival(id: i64, title: &str, trip: Option<&str>) -> Arrival {
        Arrival {
            id,
            mail_id: 9,
            booking: true,
            kind: Some("stay".into()),
            title: Some(title.into()),
            place: None,
            origin: None,
            destination: None,
            airline: None,
            flight_number: None,
            stops: vec![],
            date: Some("2026-10-13".into()),
            starts_at: None,
            ends_at: None,
            confirmation_code: None,
            price: None,
            currency: None,
            confidence: None,
            summary: title.into(),
            trip_id: trip.map(|_| 3),
            trip_name: trip.map(str::to_string),
            status: "pending".into(),
            received_at: "2026-09-19T10:00:00Z".into(),
            attachments: vec![],
        }
    }

    fn labels(m: &InlineKeyboardMarkup) -> Vec<Vec<(String, String)>> {
        m.inline_keyboard
            .iter()
            .map(|row| {
                row.iter()
                    .map(|b| {
                        let data = match &b.kind {
                            InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
                            InlineKeyboardButtonKind::WebApp(w) => w.url.to_string(),
                            other => format!("{other:?}"),
                        };
                        (b.text.clone(), data)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_button_says_what_it_asks_and_nothing_else_parses() {
        assert_eq!(parse_callback(&callback_data(Decision::Add(42))), Some(Decision::Add(42)));
        assert_eq!(parse_callback(&callback_data(Decision::Ignore(42))), Some(Decision::Ignore(42)));
        assert_eq!(parse_callback("arr:add:x"), None);
        assert_eq!(parse_callback("arr:keep:1"), None);
        assert_eq!(parse_callback("go"), None);
        assert!(callback_data(Decision::Add(i64::MAX)).len() <= 64, "Telegram's limit on callback data");
        assert_eq!(mail_of_key("inbox:17"), Some(17));
        assert_eq!(mail_of_key("turn:17"), None);
    }

    #[test]
    fn one_booking_names_where_add_puts_it_and_several_name_themselves() {
        let one = arrival_markup(&[arrival(1, "Harbour View Rooms", Some("Hong Kong, September"))], None).unwrap();
        assert_eq!(labels(&one), vec![vec![("Add to Hong Kong, September".to_string(), "arr:add:1".to_string()), ("Ignore".to_string(), "arr:ign:1".to_string())]]);

        let draft = arrival_markup(&[arrival(1, "Harbour View Rooms", None)], None).unwrap();
        assert_eq!(labels(&draft)[0][0].0, "Add to a new trip");

        let two = arrival_markup(&[arrival(1, "AMS → HKG", Some("HK")), arrival(2, "HKG → AMS", Some("HK"))], None).unwrap();
        assert_eq!(labels(&two).iter().map(|r| r[0].0.clone()).collect::<Vec<_>>(), vec!["Add AMS → HKG", "Add HKG → AMS"]);
        assert_eq!(labels(&two)[1][1].1, "arr:ign:2");
    }

    #[test]
    fn the_open_trip_button_comes_last_and_nothing_to_press_is_no_keyboard() {
        let launch = crate::mini_app::launch_url("https://goodscout.fyi").unwrap();
        let open = crate::mini_app::open_trip_button(&launch, "HK");
        let m = arrival_markup(&[arrival(1, "Harbour View Rooms", Some("HK"))], Some(open.clone())).unwrap();
        assert_eq!(labels(&m).last().unwrap()[0].0, "Open HK");
        // Decided, but the trip is still worth opening.
        let m = arrival_markup(&[], Some(open)).unwrap();
        assert_eq!(labels(&m), vec![vec![("Open HK".to_string(), "https://goodscout.fyi/tg?trip=HK".to_string())]]);
        assert!(arrival_markup(&[], None).is_none());
    }

    #[test]
    fn a_long_name_is_cut_so_two_buttons_still_share_a_row() {
        let m = arrival_markup(&[arrival(1, "x", Some(&"Hong Kong and Macau and Shenzhen".repeat(2)))], None).unwrap();
        assert_eq!(labels(&m)[0][0].0.chars().count(), "Add to ".len() + LABEL_CHARS);
    }

    #[test]
    fn the_decided_line_records_what_happened() {
        assert_eq!(decided_line(Decision::Add(1), Some("Harbour View Rooms"), Some("HK")), "✓ Harbour View Rooms added to HK.");
        assert_eq!(decided_line(Decision::Add(1), None, None), "✓ The booking added.");
        assert!(decided_line(Decision::Ignore(1), Some("Lunch"), None).starts_with("✗ Lunch ignored."));
    }
}
