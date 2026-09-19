//! "Where next?" — today's plan from where the traveller is standing.
//!
//! A location sent to the bot is the question; the answer is the items of
//! the trip that is on today, nearest first, each with how far it is and
//! a map link. Places are looked up the first time they are asked about
//! and the answer is written on the item (see `geo`), so a trip costs a
//! few geocoder calls in its life, not one per location.

use crate::core::{blocking, Core};
use crate::geo::{self, Coords};
use crate::store::TripItem;
use crate::trips::Plan;
use chrono::{DateTime, Duration, NaiveDate, Utc};

/// How near is "near": the radius a live location has to come within for
/// the bot to say something. A few streets, not the same neighbourhood.
pub const NEAR_M: f64 = 500.0;

/// Where the traveller is, and when.
#[derive(Debug, Clone, Copy)]
pub struct Here {
    pub coords: Coords,
    pub now: DateTime<Utc>,
}

/// One thing on today's plan, from where the traveller stands.
#[derive(Debug, Clone, PartialEq)]
pub struct Near {
    pub item_id: i64,
    pub title: String,
    /// "14:30", when the item has a clock.
    pub time: Option<String>,
    pub place: Option<String>,
    /// `None` for a flight, and for a place no geocoder knew.
    pub distance_m: Option<f64>,
    pub map: Option<String>,
    pub flight: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WhereNext {
    /// No trip has today in it.
    NoTrip,
    /// A trip is on, and the day has nothing planned; `next` is the first
    /// later day with something on it.
    Free { trip: String, date: NaiveDate, next: Option<NaiveDate> },
    Day { trip: String, date: NaiveDate, items: Vec<Near> },
}

/// The date where the traveller is, from their longitude alone: fifteen
/// degrees an hour. Not a time zone — those need a table this does not
/// carry — but the calendar day it gives is wrong only within about an
/// hour of midnight, and a plan is not consulted then.
pub fn local_date(now: DateTime<Utc>, lng: f64) -> NaiveDate {
    let offset = Duration::seconds((lng / 15.0 * 3600.0).round() as i64);
    (now + offset).date_naive()
}

fn date_of(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// The days an item covers: its date, through the day before `ends_at`
/// for a stay (check-out morning is not a night there, but the item is
/// still where the bags are, so the check-out day counts too).
fn covers(item: &TripItem, date: NaiveDate) -> bool {
    let Some(start) = date_of(&item.date) else { return false };
    let end = item.ends_at.as_deref().and_then(date_of).unwrap_or(start);
    start <= date && date <= end.max(start)
}

/// The trip that has `date` in it: the one whose items span it, kept
/// trips before drafts, and with several, the one updated last (which is
/// how `list_trips` orders them).
pub fn trip_on(plans: &[Plan], date: NaiveDate) -> Option<&Plan> {
    let spans = |p: &Plan| {
        let dates: Vec<NaiveDate> = p
            .trip
            .items
            .iter()
            .flat_map(|i| [date_of(&i.date), i.ends_at.as_deref().and_then(date_of)])
            .flatten()
            .collect();
        match (dates.iter().min(), dates.iter().max()) {
            (Some(first), Some(last)) => *first <= date && date <= *last,
            _ => false,
        }
    };
    plans.iter().filter(|p| p.trip.kept).find(|p| spans(p)).or_else(|| plans.iter().find(|p| spans(p)))
}

fn clock(item: &TripItem) -> Option<String> {
    let at = item.starts_at.as_deref()?;
    let hhmm = at.get(11..16)?;
    (hhmm.len() == 5).then(|| hhmm.to_string())
}

/// A link to the place: the one in the note when the note has one, else
/// a search for the coordinates, else for the place's own words.
pub fn map_link(item: &TripItem) -> Option<String> {
    if let Some(link) = item.notes.as_deref().and_then(first_map_link) {
        return Some(link);
    }
    if let (Some(lat), Some(lng)) = (item.lat, item.lng) {
        return Some(format!("https://www.google.com/maps/search/?api=1&query={lat},{lng}"));
    }
    let place = item.place.as_deref()?;
    let mut q = form_urlencoded_lite(place);
    if q.is_empty() {
        q = form_urlencoded_lite(&item.title);
    }
    Some(format!("https://www.google.com/maps/search/?api=1&query={q}"))
}

fn first_map_link(notes: &str) -> Option<String> {
    notes
        .split_whitespace()
        .find(|w| w.starts_with("https://") && (w.contains("google.") && w.contains("/maps") || w.starts_with("https://maps.app.goo.gl/")))
        .map(|w| w.trim_end_matches([')', '.', ',']).to_string())
}

/// Enough encoding for a query: what a map search needs and no more.
fn form_urlencoded_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.trim().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Today's items from here, nearest first; flights and places with no
/// coordinates after, in the day's order. Pure: coordinates are read off
/// the items, which `where_next` fills in first.
pub fn day_from(plan: &Plan, date: NaiveDate, here: Coords) -> Vec<Near> {
    let mut near: Vec<(Option<f64>, i64, Near)> = plan
        .trip
        .items
        .iter()
        .filter(|i| covers(i, date))
        .map(|i| {
            let coords = i.lat.zip(i.lng).map(|(lat, lng)| Coords { lat, lng });
            let distance_m = coords.map(|c| geo::distance_m(here, c));
            let near = Near {
                item_id: i.id,
                title: if i.is_flight() { format!("flight {}", i.route()) } else { i.title.clone() },
                time: clock(i),
                place: i.place.clone(),
                distance_m,
                map: if i.is_flight() { None } else { map_link(i) },
                flight: i.is_flight(),
            };
            (distance_m, i.position, near)
        })
        .collect();
    near.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    near.into_iter().map(|(_, _, n)| n).collect()
}

/// Where the items of today's plan are, looked up once each. A place the
/// geocoder could not find is remembered as such; a geocoder that could
/// not be reached is not, so the place is asked about next time.
async fn fill_coords(core: &Core, plan: &Plan, date: NaiveDate) -> anyhow::Result<()> {
    for item in plan.trip.items.iter().filter(|i| covers(i, date) && !i.is_flight() && i.lat.is_none() && !i.geocode_tried) {
        let from_note = item.notes.as_deref().and_then(geo::coords_in_text);
        let found = match from_note {
            Some(c) => Some(c),
            None => {
                let query = item.place.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| item.title.clone());
                match core.geocoder.lookup(&query).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::warn!(error = %e, item = item.id, "the geocoder could not be reached; answering without that distance");
                        continue;
                    }
                }
            }
        };
        let store = core.store();
        let id = item.id;
        blocking(move || store.set_item_coords(id, found.map(|c| (c.lat, c.lng)))).await?;
    }
    Ok(())
}

pub async fn where_next(core: &Core, account_id: i64, here: Here) -> anyhow::Result<WhereNext> {
    let date = local_date(here.now, here.coords.lng);
    let plans = crate::trips::list(core, account_id).await?;
    let Some(plan) = trip_on(&plans, date) else { return Ok(WhereNext::NoTrip) };
    fill_coords(core, plan, date).await?;
    // Read again: the coordinates just written are on the rows, not on
    // this copy.
    let plans = crate::trips::list(core, account_id).await?;
    let Some(plan) = trip_on(&plans, date) else { return Ok(WhereNext::NoTrip) };
    let items = day_from(plan, date, here.coords);
    if items.is_empty() {
        let next = plan.trip.items.iter().filter_map(|i| date_of(&i.date)).filter(|d| *d > date).min();
        return Ok(WhereNext::Free { trip: plan.trip.name.clone(), date, next });
    }
    Ok(WhereNext::Day { trip: plan.trip.name.clone(), date, items })
}

/// The items within `radius_m` that have a distance at all.
pub fn within(items: &[Near], radius_m: f64) -> Vec<&Near> {
    items.iter().filter(|n| n.distance_m.is_some_and(|d| d <= radius_m)).collect()
}

fn far(d: f64) -> String {
    if d < 1000.0 {
        format!("{} m", (d / 10.0).round() as i64 * 10)
    } else {
        format!("{:.1} km", d / 1000.0)
    }
}

/// Minutes on foot at a walking pace, said only for a walkable distance.
fn on_foot(d: f64) -> Option<String> {
    (d <= 3000.0).then(|| format!("~{} min on foot", ((d / 80.0).round() as i64).max(1)))
}

/// The answer, for a chat. Plain text: Telegram makes the links tappable.
pub fn render(answer: &WhereNext) -> String {
    match answer {
        WhereNext::NoTrip => "No trip of yours has today in it, so nothing to point you at. Send a location on a trip day.".to_string(),
        WhereNext::Free { trip, date, next } => {
            let mut s = format!("{}, {trip}: nothing on the plan today.", date.format("%a %-d %b"));
            if let Some(next) = next {
                s.push_str(&format!(" Next is {}.", next.format("%a %-d %b")));
            }
            s
        }
        WhereNext::Day { trip, date, items } => {
            let mut s = format!("{}, {trip} — nearest first:\n", date.format("%a %-d %b"));
            for n in items {
                s.push('\n');
                let time = n.time.as_deref().map(|t| format!("{t} ")).unwrap_or_default();
                match n.distance_m {
                    Some(d) => {
                        s.push_str(&format!("• {} · {time}{}", far(d), n.title));
                        if let Some(walk) = on_foot(d) {
                            s.push_str(&format!(" — {walk}"));
                        }
                    }
                    None if n.flight => s.push_str(&format!("• {time}{}", n.title)),
                    None => s.push_str(&format!("• {time}{} — distance unknown", n.title)),
                }
                if let Some(map) = &n.map {
                    s.push_str(&format!("\n  {map}"));
                }
            }
            s
        }
    }
}

/// The one line a live location earns when it comes within reach.
pub fn nudge_line(n: &Near) -> String {
    let d = n.distance_m.map(far).unwrap_or_default();
    let time = n.time.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
    let mut s = format!("You're {d} from {}{time}.", n.title);
    if let Some(map) = &n.map {
        s.push_str(&format!("\n{map}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewItem;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_local_day_follows_the_longitude() {
        // 23:30 UTC is already the 23rd in Hong Kong (UTC+8 by longitude)
        // and still the 22nd in Lisbon.
        let now = at("2026-09-22T23:30:00Z");
        assert_eq!(local_date(now, 114.2), NaiveDate::from_ymd_opt(2026, 9, 23).unwrap());
        assert_eq!(local_date(now, -9.1), NaiveDate::from_ymd_opt(2026, 9, 22).unwrap());
    }

    fn item(id: i64, kind: &str, title: &str, date: &str, ends: Option<&str>, coords: Option<(f64, f64)>) -> TripItem {
        TripItem {
            id,
            position: id,
            kind: kind.into(),
            title: title.into(),
            place: Some(format!("{title} St")),
            origin: (kind == "flight").then(|| "HKG".into()),
            destination: (kind == "flight").then(|| "AMS".into()),
            date: date.into(),
            starts_at: (kind != "stay").then(|| format!("{date}T14:30:00")),
            ends_at: ends.map(str::to_string),
            booked: true,
            confirmation_code: None,
            price: None,
            currency: None,
            notes: None,
            arrival_id: None,
            candidates: vec![],
            attachments: vec![],
            lat: coords.map(|c| c.0),
            lng: coords.map(|c| c.1),
            geocode_tried: coords.is_some(),
        }
    }

    fn plan(name: &str, kept: bool, items: Vec<TripItem>) -> Plan {
        Plan {
            trip: crate::store::Trip { id: 1, name: name.into(), adults: 1, cabin_class: None, status: "planning".into(), items, kept },
            readiness: crate::tools::trips::Readiness::NoFlights,
            not_ready: None,
            to_book: vec![],
            notes: vec![],
            chat: None,
        }
    }

    #[test]
    fn the_trip_on_a_day_is_the_one_that_spans_it_kept_first() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let draft = plan("Draft", false, vec![item(1, "activity", "a", "2026-09-23", None, None)]);
        let hk = plan("Hong Kong", true, vec![
            item(2, "flight", "out", "2026-09-21", None, None),
            item(3, "stay", "Hotel", "2026-09-22", Some("2026-09-29"), None),
        ]);
        let plans = vec![draft, hk];
        assert_eq!(trip_on(&plans, d("2026-09-23")).map(|p| p.trip.name.as_str()), Some("Hong Kong"));
        assert_eq!(trip_on(&plans, d("2026-09-29")).map(|p| p.trip.name.as_str()), Some("Hong Kong"), "the stay's last day is in the trip");
        assert_eq!(trip_on(&plans, d("2026-10-05")), None);
    }

    #[test]
    fn todays_items_come_nearest_first_with_flights_and_unknowns_after() {
        let here = Coords { lat: 22.2915, lng: 114.2003 }; // North Point
        let p = plan("HK", true, vec![
            item(1, "activity", "Moomin", "2026-09-23", None, Some((22.3123, 114.2254))),
            item(2, "activity", "Lunch", "2026-09-23", None, Some((22.2920, 114.2010))),
            item(3, "activity", "Unknown place", "2026-09-23", None, None),
            item(4, "flight", "home", "2026-09-23", None, None),
            item(5, "stay", "Hotel", "2026-09-22", Some("2026-09-25"), Some((22.30, 114.17))),
            item(6, "activity", "Tomorrow", "2026-09-24", None, Some((22.2915, 114.2003))),
        ]);
        let day = day_from(&p, NaiveDate::from_ymd_opt(2026, 9, 23).unwrap(), here);
        let titles: Vec<&str> = day.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["Lunch", "Hotel", "Moomin", "Unknown place", "flight HKG→AMS"]);
        assert!(day[0].distance_m.unwrap() < 200.0);
        assert_eq!(day[0].time.as_deref(), Some("14:30"));
        assert_eq!(day[1].time, None, "a stay has no clock");
        assert!(day[4].map.is_none(), "a flight gets no map link");
        assert!(day[3].map.as_deref().unwrap().contains("query=Unknown+place+St"));
        assert!(day[0].map.as_deref().unwrap().contains("query=22.292,114.201"));

        let close: Vec<&str> = within(&day, NEAR_M).iter().map(|n| n.title.as_str()).collect();
        assert_eq!(close, vec!["Lunch"]);

        let text = render(&WhereNext::Day { trip: "HK".into(), date: NaiveDate::from_ymd_opt(2026, 9, 23).unwrap(), items: day.clone() });
        assert!(text.starts_with("Wed 23 Sep, HK — nearest first:"), "{text}");
        assert!(text.contains("• 90 m · 14:30 Lunch — ~1 min on foot"), "{text}");
        assert!(text.contains(" km · 14:30 Moomin"), "{text}");
        assert!(text.contains("• 14:30 Unknown place — distance unknown"), "{text}");
        assert!(text.ends_with("• 14:30 flight HKG→AMS"), "{text}");
        assert_eq!(nudge_line(&day[0]), "You're 90 m from Lunch (14:30).\nhttps://www.google.com/maps/search/?api=1&query=22.292,114.201");
    }

    #[test]
    fn a_note_with_a_map_link_is_the_map_link() {
        let mut i = item(1, "activity", "Lunch", "2026-09-23", None, Some((1.0, 2.0)));
        i.notes = Some("Confirmed via WhatsApp. Maps: https://www.google.com/maps/search/?api=1&query=Queen%27s+Cafe.".into());
        assert_eq!(map_link(&i).as_deref(), Some("https://www.google.com/maps/search/?api=1&query=Queen%27s+Cafe"));
        assert!(render(&WhereNext::NoTrip).contains("No trip"));
        let free = WhereNext::Free { trip: "HK".into(), date: NaiveDate::from_ymd_opt(2026, 9, 27).unwrap(), next: NaiveDate::from_ymd_opt(2026, 9, 29) };
        assert_eq!(render(&free), "Sun 27 Sep, HK: nothing on the plan today. Next is Tue 29 Sep.");
    }

    #[tokio::test]
    async fn a_place_is_looked_up_once_and_a_link_in_the_note_needs_no_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "Shop L1-9a, 418 Kwun Tong Road"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{"lat": "22.3123", "lon": "114.2254"}])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "somewhere nominatim has never heard of"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("near.duckdb").to_str().unwrap().to_string();
        let core = Core::start(crate::config::Config::for_test_with(&db, "NOMINATIM_BASE_URL", &server.uri()), None).unwrap();
        let store = core.store();
        let account_id = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account_id, "Hong Kong", None, None, None).unwrap();
        let new = |title: &str, place: Option<&str>, notes: Option<&str>| NewItem {
            kind: "activity".into(),
            title: title.into(),
            place: place.map(str::to_string),
            date: "2026-09-23".into(),
            starts_at: None,
            ends_at: None,
            notes: notes.map(str::to_string),
            booked: false,
            confirmation_code: None,
            price: None,
            currency: None,
            arrival_id: None,
        };
        store.add_item(trip.id, new("Moomin", Some("Shop L1-9a, 418 Kwun Tong Road"), None)).unwrap();
        store.add_item(trip.id, new("Lunch", None, Some("https://www.google.com/maps/@22.2915,114.2003,17z"))).unwrap();
        store.add_item(trip.id, new("Mystery", Some("somewhere nominatim has never heard of"), None)).unwrap();
        store.keep_trip(account_id, "Hong Kong").unwrap();

        let here = Here { coords: Coords { lat: 22.2915, lng: 114.2003 }, now: at("2026-09-23T02:00:00Z") };
        let first = where_next(&core, account_id, here).await.unwrap();
        let WhereNext::Day { items, .. } = &first else { panic!("{first:?}") };
        let by_title = |t: &str| items.iter().find(|n| n.title == t).unwrap().distance_m;
        assert!(by_title("Lunch").unwrap() < 1.0, "from the note's link, no lookup");
        assert!((3_000.0..4_000.0).contains(&by_title("Moomin").unwrap()));
        assert_eq!(by_title("Mystery"), None);

        // Asked again: nothing is looked up twice, the `.expect(1)`s above
        // are what say so, and the unknown place stays unknown quietly.
        let again = where_next(&core, account_id, here).await.unwrap();
        assert_eq!(again, first);
    }
}
