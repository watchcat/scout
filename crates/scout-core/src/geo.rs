//! Where a place is, and how far away.
//!
//! One geocoder, Nominatim, asked once per place and never again: the
//! answer is written on the item, and so is a lookup that found nothing.
//! Nominatim's usage policy asks for an identifying User-Agent and at most
//! one request a second; this keeps to both, and the volume — a few
//! lookups per trip, the first time a location asks about its items — is
//! nowhere near what would need more.

use std::time::{Duration, Instant};

/// The spacing between requests Nominatim asks for.
const PACE: Duration = Duration::from_millis(1100);
/// A lookup that takes longer than this is one the reader is waiting on
/// from a phone; better to answer without the distance.
const BUDGET: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coords {
    pub lat: f64,
    pub lng: f64,
}

pub struct Geocoder {
    http: reqwest::Client,
    base: String,
    /// When the last request went, so the next waits its turn.
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl Geocoder {
    pub fn new(http: reqwest::Client, base: &str, contact: Option<&str>) -> Self {
        // Nominatim wants to know who is asking; a bare reqwest agent is
        // refused. The contact is whoever set the deployment up.
        let agent = match contact {
            Some(email) => format!("Scout/0.1 (https://github.com/watchcat/scout; {email})"),
            None => "Scout/0.1 (https://github.com/watchcat/scout)".to_string(),
        };
        let http = reqwest::Client::builder()
            .user_agent(agent)
            .timeout(BUDGET)
            .build()
            .unwrap_or(http);
        Self { http, base: base.trim_end_matches('/').to_string(), last: tokio::sync::Mutex::new(None) }
    }

    /// Where `place` is, or `None` when the geocoder does not know — which
    /// is an answer, and is written down as one by the caller.
    ///
    /// `Err` is the geocoder being unreachable, which is not an answer and
    /// must not be written down: the place is asked about again next time.
    pub async fn lookup(&self, place: &str) -> anyhow::Result<Option<Coords>> {
        let place = place.trim();
        if place.is_empty() {
            return Ok(None);
        }
        // One request at a time, a second apart, across every caller.
        let mut last = self.last.lock().await;
        if let Some(at) = *last {
            let since = at.elapsed();
            if since < PACE {
                tokio::time::sleep(PACE - since).await;
            }
        }
        let res = self
            .http
            .get(format!("{}/search", self.base))
            .query(&[("q", place), ("format", "jsonv2"), ("limit", "1")])
            .send()
            .await;
        *last = Some(Instant::now());
        drop(last);
        let res = res?.error_for_status()?;
        let hits: Vec<serde_json::Value> = res.json().await?;
        Ok(hits.first().and_then(|hit| {
            let lat = hit.get("lat")?.as_str()?.parse().ok()?;
            let lng = hit.get("lon")?.as_str()?.parse().ok()?;
            Some(Coords { lat, lng })
        }))
    }
}

/// Coordinates a note already carries, so a place with a map link pasted
/// on it needs no lookup at all. Reads the two shapes Google Maps links
/// take — `query=22.31,114.22` and `@22.31,114.22,17z` — and nothing
/// else: an address in the query is a string for the geocoder.
pub fn coords_in_text(text: &str) -> Option<Coords> {
    for (i, _) in text.match_indices(['@', '=']) {
        let rest = &text[i + 1..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | ',' | '-'))).unwrap_or(rest.len());
        // `api=1` and the like: not a pair, on to the next sign.
        let Some((lat, lng)) = rest[..end].split_once(',') else { continue };
        let Some(lng) = lng.split(',').next() else { continue };
        if let (Ok(lat), Ok(lng)) = (lat.parse::<f64>(), lng.parse::<f64>()) {
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lng) && lat != 0.0 {
                return Some(Coords { lat, lng });
            }
        }
    }
    None
}

/// Great-circle distance in metres. The haversine form, because places on
/// a trip are minutes to hours apart and nothing here needs an ellipsoid.
pub fn distance_m(a: Coords, b: Coords) -> f64 {
    const R: f64 = 6_371_000.0;
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlng = (b.lng - a.lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * R * h.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn a_map_link_with_coordinates_needs_no_lookup_and_an_address_does() {
        let q = coords_in_text("https://www.google.com/maps/search/?api=1&query=22.3123,114.2254");
        assert_eq!(q, Some(Coords { lat: 22.3123, lng: 114.2254 }));
        let at = coords_in_text("see https://www.google.com/maps/@22.3123,114.2254,17z/data=x");
        assert_eq!(at, Some(Coords { lat: 22.3123, lng: 114.2254 }));
        assert_eq!(coords_in_text("https://www.google.com/maps/search/?api=1&query=APM+418+Kwun+Tong+Road"), None);
        assert_eq!(coords_in_text("Confirmed with Stanley via WhatsApp"), None);
        assert_eq!(coords_in_text("query=0,0"), None, "null island is a missing value, not a place");
    }

    #[test]
    fn distances_are_about_right() {
        let apm = Coords { lat: 22.3123, lng: 114.2254 };
        let north_point = Coords { lat: 22.2915, lng: 114.2003 };
        let d = distance_m(apm, north_point);
        assert!((3_300.0..3_700.0).contains(&d), "{d}");
        assert_eq!(distance_m(apm, apm), 0.0);
    }

    #[tokio::test]
    async fn a_place_is_looked_up_once_with_an_identifying_agent_and_none_is_an_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "APM, 418 Kwun Tong Road"))
            .and(query_param("format", "jsonv2"))
            .and(wiremock::matchers::header_regex("user-agent", r"^Scout/0\.1 \(.*ops@example\.com\)$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"lat": "22.3123", "lon": "114.2254", "display_name": "APM"}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "nowhere at all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let geo = Geocoder::new(reqwest::Client::new(), &server.uri(), Some("ops@example.com"));
        assert_eq!(geo.lookup("APM, 418 Kwun Tong Road").await.unwrap(), Some(Coords { lat: 22.3123, lng: 114.2254 }));
        assert_eq!(geo.lookup("nowhere at all").await.unwrap(), None);
        assert_eq!(geo.lookup("  ").await.unwrap(), None, "nothing to ask about");
    }

    #[tokio::test]
    async fn an_unreachable_geocoder_is_an_error_not_a_no() {
        let geo = Geocoder::new(reqwest::Client::new(), "http://127.0.0.1:1", None);
        assert!(geo.lookup("APM").await.is_err());
    }

    // Real time, not tokio's paused clock: the client's own timeout is a
    // tokio timer too, and a paused clock jumps past it while the socket
    // is still being read.
    #[tokio::test]
    async fn two_lookups_are_a_second_apart() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let geo = Geocoder::new(reqwest::Client::new(), &server.uri(), None);
        let started = tokio::time::Instant::now();
        geo.lookup("a").await.unwrap();
        geo.lookup("b").await.unwrap();
        assert!(started.elapsed() >= PACE, "{:?}", started.elapsed());
    }
}
