//! Fare search through the Ignav API.
//!
//! A different kind of answer from Duffel's, and the difference is the
//! whole reason this module keeps its own labels. Duffel sells a live offer
//! payable at the price it states. Ignav sells fare *data* with links out
//! to whoever is selling it, and its own production checklist says so:
//!
//! > Display prices as approximate with phrasing like "from $299" and
//! > always link to a booking page where users see the live price.
//!
//! So every flight from here is marked approximate (or unconfirmed, for
//! `status: "unverified"`), and the ranking in `duffel::rank` says loudly
//! when one of them is undercutting a price somebody could actually pay.
//!
//! What it is better at: it costs $0.002 a search against Duffel's $0.005,
//! has no rate limits, states times in UTC as well as local, and surfaces
//! self-transfer itineraries that Duffel never shows.

use crate::tools::duffel::{Flight, Leg, PriceStatus, Source};

pub const IGNAV_API_BASE: &str = "https://ignav.com/api";

#[derive(Debug, thiserror::Error)]
pub enum IgnavError {
    #[error("ignav request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ignav api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("ignav returned an unexpected response: {detail}")]
    Decode { detail: String },
}

/// Ignav fare search. Search only — the booking-links endpoint is a
/// separate call and is not made here.
#[derive(Clone)]
pub struct IgnavClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Two-letter country whose currency and locale the fares come back
    /// in. Ignav defaults to US, which answers in dollars — merged against
    /// Duffel's euros, the currency guard then drops every row here and
    /// the provider silently contributes nothing.
    market: String,
}

impl IgnavClient {
    pub fn new(http: reqwest::Client, api_key: String, base_url: String) -> Self {
        Self { http, api_key, base_url, market: "US".to_string() }
    }

    /// Sets the market, e.g. `NL`. Uppercased because Ignav wants a
    /// two-letter country code and the caller may have it from a profile
    /// fact typed in any case.
    pub fn with_market(mut self, market: &str) -> Self {
        let market = market.trim().to_ascii_uppercase();
        if market.len() == 2 {
            self.market = market;
        }
        self
    }

    /// Fares for one journey. Round trips go to a different endpoint than
    /// one-ways, which is Ignav's shape rather than ours.
    pub async fn search(
        &self,
        query: &crate::tools::duffel::FlightQuery,
    ) -> Result<Vec<Flight>, IgnavError> {
        // Rejected before the request for the same reason as Duffel: a
        // search costs money whether or not the codes were real.
        query.validate().map_err(|e| IgnavError::Decode { detail: e.to_string() })?;

        let origin = query.origin.trim().to_ascii_uppercase();
        let destination = query.destination.trim().to_ascii_uppercase();
        let mut body = serde_json::json!({
            "origin": origin,
            "destination": destination,
            "departure_date": query.departure_date.trim(),
            "market": self.market,
        });
        // One-way and round-trip are different endpoints here, not a
        // second slice as Duffel has it.
        let endpoint = match &query.return_date {
            Some(back) => {
                body["return_date"] = serde_json::json!(back.trim());
                "fares/round-trip"
            }
            None => "fares/one-way",
        };
        if let Some(cabin) = &query.cabin_class {
            body["cabin_class"] = serde_json::json!(cabin.trim().to_ascii_lowercase());
        }
        if let Some(max) = query.max_connections {
            body["max_stops"] = serde_json::json!(max);
        }
        if let Some(adults) = query.adults {
            body["adults"] = serde_json::json!(adults);
        }

        let resp = self
            .http
            .post(format!("{}/{endpoint}", self.base_url))
            .header("X-Api-Key", &self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(IgnavError::Api {
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }
        let parsed: Response = serde_json::from_str(&text).map_err(|e| IgnavError::Decode {
            detail: format!("{e}; body: {}", text.chars().take(200).collect::<String>()),
        })?;
        Ok(parsed.itineraries.into_iter().filter_map(flight).collect())
    }

    /// Where a flight found earlier can be bought.
    ///
    /// The lookup returns a *refreshed* itinerary, so the price here is
    /// today's rather than the one the search quoted — the caller compares
    /// the two and says so when it has moved.
    pub async fn booking_links(&self, ignav_id: &str) -> Result<BookingLinks, IgnavError> {
        let resp = self
            .http
            .post(format!("{}/fares/booking-links", self.base_url))
            .header("X-Api-Key", &self.api_key)
            .header("Accept", "application/json")
            // Deliberately just the id. Measured live: sending `market`
            // alongside it is a 400 — "ignav_id lookups do not accept
            // passenger or market fields" — because the id already carries
            // the market and passengers of the search that produced it.
            .json(&serde_json::json!({"ignav_id": ignav_id}))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(IgnavError::Api {
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }
        let parsed: RawBookingResponse =
            serde_json::from_str(&text).map_err(|e| IgnavError::Decode {
                detail: format!("{e}; body: {}", text.chars().take(200).collect::<String>()),
            })?;

        let itinerary = parsed.itinerary;
        let self_transfer = itinerary
            .as_ref()
            .and_then(|i| i.requires_self_transfer)
            .unwrap_or(false);

        let mut options: Vec<BookingOption> = parsed
            .booking_options
            .into_iter()
            .flat_map(|o| o.links)
            .filter(|l| !l.url.trim().is_empty())
            .map(|l| BookingOption {
                provider: l.provider_name.unwrap_or_default(),
                provider_type: l.provider_type.unwrap_or_default(),
                fare_name: l.fare_name,
                price: l.price.as_ref().and_then(|p| p.amount),
                currency: l.price.as_ref().and_then(|p| p.currency.clone()),
                url: l.url,
            })
            .collect();
        // Airlines first, then by price. Booking direct means one party to
        // deal with when the flight changes, which is worth more than a
        // few euro to most travellers — but see the note below.
        options.sort_by(|a, b| {
            let rank = |o: &BookingOption| u8::from(o.provider_type != "airline");
            rank(a)
                .cmp(&rank(b))
                .then(a.price.unwrap_or(f64::MAX).total_cmp(&b.price.unwrap_or(f64::MAX)))
        });

        let mut notes = Vec::new();
        if self_transfer {
            notes.push(
                "this itinerary is booked as separate tickets — the traveller collects their \
                 bags, checks in again, and has no protection if the first flight is late"
                    .to_string(),
            );
        }
        // Airline-first is a default, not a verdict: say when it costs more.
        let cheapest_airline = options
            .iter()
            .filter(|o| o.provider_type == "airline")
            .filter_map(|o| o.price)
            .fold(f64::MAX, f64::min);
        if let Some(cheaper) = options
            .iter()
            .filter(|o| o.provider_type != "airline")
            .filter(|o| o.price.is_some_and(|p| p < cheapest_airline))
            .min_by(|a, b| a.price.unwrap().total_cmp(&b.price.unwrap()))
        {
            notes.push(format!(
                "{} sells it for {:.2} {}, below the airline's own {:.2} — say both and let the \
                 user choose; booking direct means one party to deal with if the flight changes",
                cheaper.provider,
                cheaper.price.unwrap_or_default(),
                cheaper.currency.clone().unwrap_or_default(),
                cheapest_airline
            ));
        }

        Ok(BookingLinks {
            price_now: itinerary.as_ref().and_then(|i| i.price.amount),
            currency: itinerary.as_ref().and_then(|i| i.price.currency.clone()),
            self_transfer,
            options,
            notes,
        })
    }
}

#[derive(serde::Deserialize)]
struct RawBookingResponse {
    #[serde(default)]
    itinerary: Option<RawItinerary>,
    #[serde(default)]
    booking_options: Vec<RawBookingOption>,
}

#[derive(serde::Deserialize)]
struct RawBookingOption {
    #[serde(default)]
    links: Vec<RawBookingLink>,
}

#[derive(serde::Deserialize)]
struct RawBookingLink {
    #[serde(default)]
    provider_name: Option<String>,
    #[serde(default)]
    provider_type: Option<String>,
    #[serde(default)]
    fare_name: Option<String>,
    #[serde(default)]
    price: Option<RawPrice>,
    #[serde(default)]
    url: String,
}

/// Where one flight can actually be bought, and for how much.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct BookingLinks {
    /// The price Ignav states now. The lookup refreshes the itinerary, so
    /// this can differ from what the search returned.
    pub price_now: Option<f64>,
    pub currency: Option<String>,
    pub self_transfer: bool,
    /// Sellers, airlines first.
    pub options: Vec<BookingOption>,
    pub notes: Vec<String>,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct BookingOption {
    pub provider: String,
    /// `airline` or `third_party`.
    pub provider_type: String,
    pub fare_name: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub url: String,
}

/// Turns one Ignav itinerary into the shape the ranking already speaks.
///
/// `None` when it lacks a usable price — a fare with no number, or one at
/// zero, is a missing price rather than a free flight, and either would
/// sort straight to the top of a merged list.
fn flight(raw: RawItinerary) -> Option<Flight> {
    let price = raw.price.amount.filter(|p| p.is_finite() && *p > 0.0)?;
    let currency = raw.price.currency.filter(|c| !c.trim().is_empty())?;
    let legs: Vec<Leg> = [raw.outbound.as_ref(), raw.inbound.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(leg)
        .collect();
    let total_minutes = match legs.is_empty() {
        true => None,
        false => legs.iter().map(|l| l.duration_minutes).sum(),
    };

    Some(Flight {
        offer_id: raw.ignav_id.unwrap_or_default(),
        source: Source::Ignav,
        // "verified" means Ignav checked it with the seller; it still is
        // not a price held for this traveller, which is what bookable
        // means everywhere else in this codebase.
        price_status: match raw.price.status.as_deref() {
            Some("verified") => PriceStatus::Approximate,
            _ => PriceStatus::Unconfirmed,
        },
        self_transfer: raw.requires_self_transfer.unwrap_or(false),
        airline: raw
            .outbound
            .as_ref()
            .and_then(|l| l.carrier.clone())
            .unwrap_or_default(),
        price,
        currency,
        total_minutes,
        total_duration: total_minutes.map(crate::tools::duffel::human_duration),
        checked_bags: raw.bags.as_ref().and_then(|b| b.checked),
        carry_on_bags: raw.bags.as_ref().and_then(|b| b.carry_on),
        // Ignav quotes fare data rather than holding an offer, so nothing
        // expires the way a Duffel price does.
        expires_at: None,
        legs,
    })
}

/// One direction. Ignav gives each segment its own duration and the leg its
/// total, so no arithmetic is needed on the timestamps — which is just as
/// well, since they are local to their own airports here too.
fn leg(raw: &RawLeg) -> Option<Leg> {
    let first = raw.segments.first()?;
    let last = raw.segments.last()?;
    let duration_minutes = raw
        .duration_minutes
        .or_else(|| raw.segments.iter().map(|s| s.duration_minutes).sum());

    let connections: Vec<crate::tools::duffel::Connection> = raw
        .segments
        .windows(2)
        .map(|pair| crate::tools::duffel::Connection {
            airport: pair[0].arrival_airport.clone(),
            departs_from: (pair[1].departure_airport != pair[0].arrival_airport)
                .then(|| pair[1].departure_airport.clone()),
            arriving_at_local: pair[0].arrival_time_local.clone(),
            departing_at_local: pair[1].departure_time_local.clone(),
            layover_minutes: None,
            layover: None,
            changes_airport: pair[1].departure_airport != pair[0].arrival_airport,
        })
        .map(with_layover)
        .collect();

    let flights: Vec<String> = raw
        .segments
        .iter()
        .map(|s| {
            format!(
                "{}{}",
                s.marketing_carrier_code.clone().unwrap_or_default(),
                s.flight_number.clone().unwrap_or_default()
            )
        })
        .collect();

    let origin = first.departure_airport.clone();
    let destination = last.arrival_airport.clone();
    Some(Leg {
        itinerary: strip(&origin, &destination, first, last, &connections),
        origin,
        destination,
        departing_at_local: first.departure_time_local.clone(),
        arriving_at_local: last.arrival_time_local.clone(),
        duration_minutes,
        duration: duration_minutes.map(crate::tools::duffel::human_duration),
        stops: (raw.segments.len() - 1) as u32,
        flights,
        connections,
    })
}

/// Fills in the wait, which is sound here for the same reason it is on the
/// Duffel side: both stamps belong to the one airport.
fn with_layover(mut stop: crate::tools::duffel::Connection) -> crate::tools::duffel::Connection {
    let minutes = crate::tools::duffel::minutes_between(&stop.arriving_at_local, &stop.departing_at_local);
    stop.layover_minutes = minutes;
    stop.layover = minutes.map(crate::tools::duffel::human_duration);
    stop
}

/// The same one-line strip Duffel legs draw, from Ignav's field names.
fn strip(
    origin: &str,
    destination: &str,
    first: &RawSegment,
    last: &RawSegment,
    connections: &[crate::tools::duffel::Connection],
) -> String {
    let mut parts = vec![crate::tools::duffel::stamped(origin, Some(&first.departure_time_local))];
    for stop in connections {
        let place = match &stop.departs_from {
            Some(onward) => format!("{}/{onward}", stop.airport),
            None => stop.airport.clone(),
        };
        parts.push(match &stop.layover {
            Some(wait) => format!("{place} {wait}"),
            None => place,
        });
    }
    parts.push(crate::tools::duffel::stamped(destination, Some(&last.arrival_time_local)));
    parts.join(crate::tools::duffel::HOP)
}

#[derive(serde::Deserialize)]
struct Response {
    #[serde(default)]
    itineraries: Vec<RawItinerary>,
}

#[derive(serde::Deserialize)]
struct RawItinerary {
    price: RawPrice,
    #[serde(default)]
    outbound: Option<RawLeg>,
    #[serde(default)]
    inbound: Option<RawLeg>,
    #[serde(default)]
    bags: Option<RawBags>,
    #[serde(default)]
    requires_self_transfer: Option<bool>,
    #[serde(default)]
    ignav_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawPrice {
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawLeg {
    #[serde(default)]
    carrier: Option<String>,
    #[serde(default)]
    duration_minutes: Option<u32>,
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(serde::Deserialize)]
struct RawSegment {
    #[serde(default)]
    marketing_carrier_code: Option<String>,
    #[serde(default)]
    flight_number: Option<String>,
    departure_airport: String,
    departure_time_local: String,
    arrival_airport: String,
    arrival_time_local: String,
    #[serde(default)]
    duration_minutes: Option<u32>,
}

#[derive(serde::Deserialize)]
struct RawBags {
    #[serde(default)]
    carry_on: Option<u32>,
    #[serde(default)]
    checked: Option<u32>,
}

/// Hands over where a flight already shown can actually be bought.
///
/// Registered alongside `search_flights` whenever Ignav is configured.
/// Unlike Duffel's hosted checkout, these links open the seller's own page
/// with the flight already selected.
pub struct BookingLinksTool {
    pub client: IgnavClient,
    /// The same memo the search filled, so what Scout quoted can be
    /// compared against what the seller says now without asking the model
    /// to remember a number.
    pub budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
}

#[derive(Debug, serde::Deserialize)]
pub struct BookingLinksArgs {
    /// The `offer_id` of an Ignav row already shown to the user.
    pub ignav_id: String,
}

impl rig::tool::Tool for BookingLinksTool {
    const NAME: &'static str = "flight_booking_links";
    type Error = IgnavError;
    type Args = BookingLinksArgs;
    type Output = BookingLinks;

    fn description(&self) -> String {
        "Where a flight you already showed can be bought. Pass the offer_id \
         of a row from search_flights whose source is 'ignav'. Returns the \
         airline's own booking page and any resellers, each with their \
         price, opening with the flight already selected — the user does not \
         re-enter anything. Call it only when the user says which flight \
         they want. The price is re-checked at this point, so read the notes: \
         a fare that has moved since you quoted it must be reported, not \
         glossed over. Does not work for Duffel rows — those use \
         create_booking_link."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ignav_id": {"type": "string", "description": "offer_id of an ignav row from search_flights"}
            },
            "required": ["ignav_id"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut found = self.client.booking_links(&args.ignav_id).await?;
        // What this request actually put in front of the user, kept by
        // Rust rather than recalled by the model.
        if let (Some((quoted, currency)), Some(now)) =
            (self.budget.quoted_price(&args.ignav_id), found.price_now)
        {
            if (now - quoted).abs() >= 0.01 {
                let direction = if now > quoted { "risen" } else { "fallen" };
                found.notes.push(format!(
                    "the fare has {direction} since it was quoted: {quoted:.2} {currency} then, \
                     {now:.2} {} now — tell the user the new price plainly before they open the \
                     link, do not repeat the old one",
                    found.currency.clone().unwrap_or(currency.clone())
                ));
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod booking_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> IgnavClient {
        IgnavClient::new(reqwest::Client::new(), "key".to_string(), server.uri())
            .with_market("nl")
    }

    /// Two sellers for one flight, at different prices — which is the
    /// normal case and the reason this endpoint is worth calling.
    fn options(searched: f64, airline: f64, ota: f64) -> serde_json::Value {
        json!({
            "itinerary": {
                "price": {"amount": searched, "currency": "EUR", "status": "verified"},
                "outbound": {"carrier": "Transavia", "duration_minutes": 185, "segments": [{
                    "marketing_carrier_code": "HV", "flight_number": "5955",
                    "departure_airport": "AMS", "departure_time_local": "2026-09-14T17:40:00",
                    "arrival_airport": "LIS", "arrival_time_local": "2026-09-14T19:45:00",
                    "duration_minutes": 185
                }]},
                "requires_self_transfer": false
            },
            "booking_options": [{
                "legs": [0],
                "links": [
                    {"provider_name": "Gotogate", "provider_type": "third_party",
                     "fare_name": "Basic", "price": {"amount": ota, "currency": "EUR", "status": "verified"},
                     "url": "https://www.gotogate.com/x"},
                    {"provider_name": "Transavia", "provider_type": "airline",
                     "fare_name": "Basic", "price": {"amount": airline, "currency": "EUR", "status": "verified"},
                     "url": "https://www.transavia.com/y"}
                ]
            }]
        })
    }

    #[tokio::test]
    async fn a_booking_lookup_asks_by_id_and_puts_the_airline_first() {
        // Booking direct is the safer default: one party to deal with when
        // the flight changes, and no OTA between the traveller and the
        // airline. Cheaper elsewhere is worth saying, not worth silently
        // preferring.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fares/booking-links"))
            // Just the id: adding `market` here is a 400 from the real API,
            // since the id already carries the search's market. The first
            // version of this test asserted the opposite and passed.
            .and(body_json(json!({"ignav_id": "abc123"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(options(118.0, 121.0, 118.0)))
            .expect(1)
            .mount(&server)
            .await;

        let found = client(&server).booking_links("abc123").await.unwrap();
        assert_eq!(found.options[0].provider, "Transavia");
        assert_eq!(found.options[0].provider_type, "airline");
        assert_eq!(found.options[1].provider, "Gotogate");
        assert_eq!(found.price_now, Some(118.0));
        assert_eq!(found.currency.as_deref(), Some("EUR"));
    }

    #[tokio::test]
    async fn a_cheaper_third_party_is_pointed_out_rather_than_buried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(options(118.0, 140.0, 118.0)))
            .mount(&server)
            .await;

        let found = client(&server).booking_links("abc123").await.unwrap();
        assert!(
            found.notes.iter().any(|n| n.contains("Gotogate") && n.contains("118")),
            "the cheaper seller must be named with its price, got: {:?}",
            found.notes
        );
        // Still airline-first in the list; the note carries the trade-off.
        assert_eq!(found.options[0].provider_type, "airline");
    }

    #[tokio::test]
    async fn a_self_transfer_itinerary_still_says_so_at_booking_time() {
        let server = MockServer::start().await;
        let mut body = options(90.0, 90.0, 95.0);
        body["itinerary"]["requires_self_transfer"] = json!(true);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let found = client(&server).booking_links("abc123").await.unwrap();
        assert!(found.self_transfer);
        assert!(
            found.notes.iter().any(|n| n.contains("separate tickets")),
            "got: {:?}",
            found.notes
        );
    }

    #[tokio::test]
    async fn a_fare_that_moved_since_it_was_quoted_is_reported_not_glossed_over() {
        // The lookup refreshes the itinerary, so the number the user was
        // shown a minute ago can be wrong by the time they tap the link.
        // The old price comes from Rust's own memo, never from the model.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(options(139.0, 139.0, 145.0)))
            .mount(&server)
            .await;

        let budget = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
        let mut quoted = crate::tools::duffel::Flight {
            offer_id: "abc123".to_string(),
            source: Source::Ignav,
            price_status: PriceStatus::Approximate,
            self_transfer: false,
            airline: "Transavia".to_string(),
            price: 118.0,
            currency: "EUR".to_string(),
            legs: Vec::new(),
            total_minutes: None,
            total_duration: None,
            checked_bags: None,
            carry_on_bags: None,
            expires_at: None,
        };
        quoted.price = 118.0;
        budget.remember("AMS-LIS".to_string(), vec![quoted]);

        let tool = BookingLinksTool { client: client(&server), budget };
        let found = rig::tool::Tool::call(
            &tool,
            BookingLinksArgs { ignav_id: "abc123".to_string() },
        )
        .await
        .unwrap();

        assert!(
            found.notes.iter().any(|n| n.contains("118") && n.contains("139") && n.contains("risen")),
            "both prices and the direction must be stated, got: {:?}",
            found.notes
        );
    }

    #[tokio::test]
    async fn an_unchanged_fare_needs_no_warning() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(options(118.0, 118.0, 125.0)))
            .mount(&server)
            .await;

        let budget = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
        budget.remember(
            "AMS-LIS".to_string(),
            vec![crate::tools::duffel::Flight {
                offer_id: "abc123".to_string(),
                source: Source::Ignav,
                price_status: PriceStatus::Approximate,
                self_transfer: false,
                airline: "Transavia".to_string(),
                price: 118.0,
                currency: "EUR".to_string(),
                legs: Vec::new(),
                total_minutes: None,
                total_duration: None,
                checked_bags: None,
                carry_on_bags: None,
                expires_at: None,
            }],
        );

        let tool = BookingLinksTool { client: client(&server), budget };
        let found =
            rig::tool::Tool::call(&tool, BookingLinksArgs { ignav_id: "abc123".to_string() })
                .await
                .unwrap();
        assert!(
            !found.notes.iter().any(|n| n.contains("risen") || n.contains("fallen")),
            "got: {:?}",
            found.notes
        );
    }

    #[tokio::test]
    async fn a_lookup_that_fails_says_so_rather_than_returning_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404).set_body_string("unknown id"))
            .mount(&server)
            .await;
        let err = client(&server).booking_links("gone").await.unwrap_err();
        assert!(matches!(err, IgnavError::Api { status: 404, .. }), "got: {err}");
    }
}

#[cfg(test)]
mod live {
    //! Ignored by default: needs `IGNAV_API_KEY` and network.
    use super::*;
    use crate::tools::duffel::FlightQuery;

    #[tokio::test]
    #[ignore]
    async fn searches_a_real_route() {
        let key = std::env::var("IGNAV_API_KEY").expect("IGNAV_API_KEY");
        let client = IgnavClient::new(reqwest::Client::new(), key, IGNAV_API_BASE.to_string())
            .with_market(&std::env::var("SCOUT_PROBE_MARKET").unwrap_or_else(|_| "US".into()));
        let query = FlightQuery {
            origin: std::env::var("SCOUT_PROBE_ORIGIN").unwrap_or_else(|_| "AMS".into()),
            destination: std::env::var("SCOUT_PROBE_DESTINATION").unwrap_or_else(|_| "LIS".into()),
            departure_date: std::env::var("SCOUT_PROBE_DATE")
                .unwrap_or_else(|_| "2026-09-14".into()),
            return_date: std::env::var("SCOUT_PROBE_RETURN").ok(),
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        };

        let flights = client.search(&query).await.unwrap();
        println!("LIVE ignav fares={}", flights.len());
        for f in flights.iter().take(5) {
            println!(
                "  {:>8.2} {} {:?} {:<24} {:?} bags(c={:?} h={:?}) self={}",
                f.price, f.currency, f.price_status, f.airline, f.total_duration,
                f.carry_on_bags, f.checked_bags, f.self_transfer
            );
            for leg in &f.legs {
                println!("    {}", leg.itinerary);
            }
        }

        assert!(!flights.is_empty(), "a busy route should return fares");
        let f = &flights[0];
        assert!(f.price > 0.0);
        assert_eq!(f.source, Source::Ignav);
        assert!(!f.price_status.is_bookable(), "ignav never sells a bookable price");
        assert!(!f.airline.is_empty(), "carrier did not parse");
        assert!(!f.legs.is_empty(), "outbound leg did not parse");
        assert_eq!(f.legs[0].origin, query.origin, "segment airports did not parse");
        assert!(f.legs[0].duration_minutes.is_some(), "duration did not parse");
        assert!(f.legs[0].itinerary.contains('✈'), "strip did not draw: {}", f.legs[0].itinerary);

        // Where the cheapest one can actually be bought.
        let found = client.booking_links(&f.offer_id).await.unwrap();
        println!(
            "LIVE booking links for {} — now {:?} {:?}, self_transfer={}",
            f.offer_id, found.price_now, found.currency, found.self_transfer
        );
        for o in &found.options {
            println!(
                "  {:<14} {:<12} {:?} {:?}\n      {}",
                o.provider, o.provider_type, o.fare_name, o.price, o.url
            );
        }
        for n in &found.notes {
            println!("  note: {n}");
        }
        assert!(!found.options.is_empty(), "a bookable fare should have somewhere to buy it");
        assert!(found.options.iter().all(|o| o.url.starts_with("http")), "links must be real URLs");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::duffel::FlightQuery;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> IgnavClient {
        IgnavClient::new(reqwest::Client::new(), "key".to_string(), server.uri())
    }

    fn query() -> FlightQuery {
        FlightQuery {
            origin: "SFO".to_string(),
            destination: "JFK".to_string(),
            departure_date: "2026-09-05".to_string(),
            return_date: None,
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        }
    }

    /// The response shape from Ignav's own documentation.
    fn one_itinerary(amount: f64, status: &str) -> serde_json::Value {
        json!({
            "origin": "SFO", "destination": "JFK", "departure_date": "2026-09-05",
            "itineraries": [{
                "price": {"amount": amount, "currency": "USD", "status": status},
                "outbound": {
                    "carrier": "American Airlines",
                    "duration_minutes": 330,
                    "segments": [{
                        "marketing_carrier_code": "AA",
                        "flight_number": "100",
                        "operating_carrier_name": "American Airlines",
                        "departure_airport": "SFO",
                        "departure_time_local": "2026-09-05T08:00:00",
                        "departure_timezone": "America/Los_Angeles",
                        "departure_time_utc": "2026-09-05T15:00:00Z",
                        "arrival_airport": "JFK",
                        "arrival_time_local": "2026-09-05T16:30:00",
                        "arrival_timezone": "America/New_York",
                        "arrival_time_utc": "2026-09-05T20:30:00Z",
                        "duration_minutes": 330,
                        "aircraft": "Boeing 777"
                    }]
                },
                "cabin_class": "economy",
                "bags": {"carry_on": 1, "checked": 0},
                "requires_self_transfer": false,
                "ignav_id": "5e4fcd2f1dc340649eb19f6ee2afb57a"
            }]
        })
    }

    #[tokio::test]
    async fn a_one_way_search_maps_onto_the_shape_the_ranking_speaks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fares/one-way"))
            .and(header("X-Api-Key", "key"))
            .and(body_json(json!({
                "origin": "SFO", "destination": "JFK", "departure_date": "2026-09-05",
                // Always sent; US is Ignav's own default and ours.
                "market": "US"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(one_itinerary(299.0, "verified")))
            .expect(1)
            .mount(&server)
            .await;

        let flights = client(&server).search(&query()).await.unwrap();
        assert_eq!(flights.len(), 1);
        let f = &flights[0];
        assert_eq!(f.source, Source::Ignav);
        // Verified is still not bookable: it is a price somewhere else.
        assert_eq!(f.price_status, PriceStatus::Approximate);
        assert_eq!(f.price, 299.0);
        assert_eq!(f.currency, "USD");
        assert_eq!(f.airline, "American Airlines");
        assert_eq!(f.offer_id, "5e4fcd2f1dc340649eb19f6ee2afb57a");
        assert_eq!(f.total_minutes, Some(330));
        assert_eq!(f.carry_on_bags, Some(1));
        assert_eq!(f.checked_bags, Some(0));
        assert_eq!(f.legs.len(), 1);
        assert_eq!(f.legs[0].origin, "SFO");
        assert_eq!(f.legs[0].destination, "JFK");
        assert_eq!(f.legs[0].flights, vec!["AA100"]);
        assert_eq!(f.legs[0].itinerary, "SFO 08:00 05.09 ✈ JFK 16:30 05.09");
    }

    #[tokio::test]
    async fn the_market_is_sent_so_fares_come_back_in_the_travellers_currency() {
        // Measured live: without a market, Ignav answers in USD while
        // Duffel answers in EUR — and the currency guard in rank() then
        // drops every Ignav row, so the whole provider silently does
        // nothing. The market is what makes the two comparable at all.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(json!({
                "origin": "SFO", "destination": "JFK",
                "departure_date": "2026-09-05", "market": "NL"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(one_itinerary(299.0, "verified")))
            .expect(1)
            .mount(&server)
            .await;

        client(&server).with_market("nl").search(&query()).await.unwrap();
    }

    #[tokio::test]
    async fn an_unverified_price_is_marked_weaker_than_a_verified_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(one_itinerary(199.0, "unverified")),
            )
            .mount(&server)
            .await;
        let flights = client(&server).search(&query()).await.unwrap();
        assert_eq!(flights[0].price_status, PriceStatus::Unconfirmed);
    }

    #[tokio::test]
    async fn a_round_trip_goes_to_the_round_trip_endpoint_and_keeps_both_legs() {
        let server = MockServer::start().await;
        let mut body = one_itinerary(542.0, "verified");
        body["itineraries"][0]["inbound"] = json!({
            "carrier": "American Airlines",
            "duration_minutes": 380,
            "segments": [{
                "marketing_carrier_code": "AA", "flight_number": "101",
                "departure_airport": "JFK", "departure_time_local": "2026-09-12T18:00:00",
                "arrival_airport": "SFO", "arrival_time_local": "2026-09-12T21:20:00",
                "duration_minutes": 380
            }]
        });
        Mock::given(method("POST"))
            .and(path("/fares/round-trip"))
            .and(body_json(json!({
                "origin": "SFO", "destination": "JFK", "market": "US",
                "departure_date": "2026-09-05", "return_date": "2026-09-12"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let mut q = query();
        q.return_date = Some("2026-09-12".to_string());
        let flights = client(&server).search(&q).await.unwrap();
        assert_eq!(flights[0].legs.len(), 2);
        assert_eq!(flights[0].legs[1].origin, "JFK");
        assert_eq!(flights[0].total_minutes, Some(330 + 380));
    }

    #[tokio::test]
    async fn a_self_transfer_itinerary_is_flagged_because_the_risk_is_the_travellers() {
        // Two tickets: collect the bags, check in again, and no protection
        // when the first leg runs late. Duffel never returns these, so the
        // flag only ever comes from here.
        let server = MockServer::start().await;
        let mut body = one_itinerary(180.0, "verified");
        body["itineraries"][0]["requires_self_transfer"] = json!(true);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let flights = client(&server).search(&query()).await.unwrap();
        assert!(flights[0].self_transfer);
    }

    #[tokio::test]
    async fn a_fare_with_no_usable_price_is_dropped_rather_than_priced_at_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"itineraries": [
                {"price": {"currency": "USD", "status": "verified"}, "ignav_id": "a"},
                {"price": {"amount": 0, "currency": "USD", "status": "verified"}, "ignav_id": "b"},
                {"price": {"amount": 250, "currency": "USD", "status": "verified"},
                 "outbound": {"carrier": "X", "duration_minutes": 100, "segments": []},
                 "ignav_id": "c"}
            ]})))
            .mount(&server)
            .await;

        let ids: Vec<String> =
            client(&server).search(&query()).await.unwrap().into_iter().map(|f| f.offer_id).collect();
        assert_eq!(ids, vec!["c"], "a free flight is a missing price, not a bargain");
    }

    #[tokio::test]
    async fn no_fares_is_an_empty_list_and_an_error_is_an_error() {
        // Ignav's own checklist: "An empty itineraries array is a valid
        // successful response, not an error."
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"itineraries": []})))
            .mount(&server)
            .await;
        assert!(client(&server).search(&query()).await.unwrap().is_empty());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let err = client(&server).search(&query()).await.unwrap_err();
        assert!(matches!(err, IgnavError::Api { status: 401, .. }), "got: {err}");
    }
}
