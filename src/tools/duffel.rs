//! Flight search through the Duffel API.
//!
//! Search only: this never creates an order, so no passenger details and no
//! payment ever pass through here. Duffel's own pricing makes that mode
//! explicit — searches are free against an allowance of 1500 per order, and
//! with zero orders every search is billed at $0.005.
//!
//! The model picks the route and dates; the ranking below is done in Rust,
//! for the same reason `compare_prices` exists. Duffel returns
//! `total_amount` as a *string*, and sorting those lexically puts "1000.00"
//! below "62.19".
//!
//! The tool the agent calls is first; everything below it is what that call
//! is made of — the query, the client, the response parsing, the ranking.

/// Live flight search. Registered only when `DUFFEL_API_KEY` is set, so the
/// model never sees a tool that cannot work.
pub struct FlightSearchTool {
    pub client: DuffelClient,
    /// Every search is billed, so each one is recorded against the user who
    /// caused it and shown in `/stat`.
    pub store: crate::store::Store,
    pub user_id: i64,
    /// Shared across one user request: caps how many searches it can buy,
    /// and remembers what each one returned so a repeat costs nothing.
    pub budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
}

impl rig::tool::Tool for FlightSearchTool {
    const NAME: &'static str = "search_flights";
    type Error = DuffelError;
    type Args = FlightQuery;
    type Output = FlightSearchOutput;

    fn description(&self) -> String {
        "Search live flight prices and schedules for a route and date. \
         Returns real, bookable offers from the airlines themselves — prices, \
         times, stops, flight numbers and included baggage — ranked cheapest \
         first in Rust. Use this for any 'flights to X', 'how much to fly to \
         X', or 'cheapest flight' question instead of searching the web: \
         fares change hourly and a search result page cannot be trusted for \
         one. Give airports as 3-letter IATA codes and dates as YYYY-MM-DD. \
         Scout cannot book: quote the numbers and let the user buy from the \
         airline."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "origin": {"type": "string", "description": "departure airport or city, 3-letter IATA code (AMS, LHR, NYC)"},
                "destination": {"type": "string", "description": "arrival airport or city, 3-letter IATA code"},
                "departure_date": {"type": "string", "description": "YYYY-MM-DD"},
                "return_date": {"type": "string", "description": "YYYY-MM-DD; omit for a one-way"},
                "adults": {"type": "integer", "description": "adult passengers, default 1"},
                "cabin_class": {"type": "string", "enum": CABIN_CLASSES},
                "max_connections": {"type": "integer", "description": "0 for direct flights only, up to 2"},
                "flex_days": {"type": "integer", "description": "also price this many days either side of departure_date, max 3. Each day is a separate paid search, so ask for it only when the traveller says their dates are flexible. Returns by_date: the cheapest fare per day."}
            },
            "required": ["origin", "destination", "departure_date"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Checked before anything is bought, so a bad query costs nothing.
        args.validate()?;

        // One day, or one per day of a flexible window. Nearest the
        // requested date first, so a short allowance costs the far edge
        // rather than the day that was actually asked for.
        let window = args.window()?;
        let flexible = window.len() > 1;

        let mut affordable = Vec::new();
        let mut unaffordable = Vec::new();
        for day in window {
            let key = day.cache_key();
            // The allowance limits what is bought, not what is already
            // known — a day this request has paid for is always served.
            if self.budget.recall(&key).is_some() || self.budget.claim_one() {
                affordable.push((key, day));
            } else {
                unaffordable.push(day.departure_date.clone());
            }
        }
        if affordable.is_empty() {
            return Err(DuffelError::Invalid(format!(
                "this request has already used its {} flight searches — answer now with the \
                 routes you have already looked up rather than searching another",
                crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST
            )));
        }

        // Run together: seven sequential searches would be half a minute of
        // silence, and Duffel's rate limit is far above this.
        let searched = futures::future::join_all(
            affordable.into_iter().map(|(key, day)| self.one_day(key, day)),
        )
        .await;

        let mut all = Vec::new();
        let mut by_date = Vec::new();
        let mut failures = Vec::new();
        for (day, result) in searched {
            match result {
                Ok(flights) => {
                    by_date.push(DayPrice::new(&day.departure_date, &flights));
                    all.extend(flights);
                }
                // One bad day must not sink a whole window; a single-date
                // search still surfaces its error as before.
                Err(e) if flexible => failures.push(format!("{} ({e})", day.departure_date)),
                Err(e) => return Err(e),
            }
        }
        by_date.sort_by(|a, b| a.date.cmp(&b.date));

        let mut out = FlightSearchOutput::new(&args, rank(all));
        // Travels with the prices it is baked into. Relying on the preamble
        // alone was measured failing in production: a reply quoted a fare
        // that silently included the fee and never mentioned it.
        if self.client.markup_rate() > 0.0 {
            out.notes.push(format!(
                "every price here already includes a {} booking fee, which is what the checkout \
                 charges — say so in the reply rather than quoting the number bare",
                percentage(self.client.markup_rate())
            ));
        }
        if flexible {
            out.by_date = by_date;
            if !unaffordable.is_empty() {
                unaffordable.sort();
                out.notes.push(format!(
                    "this request could not afford to check {} — only the days listed were \
                     searched, so say which window the answer covers",
                    unaffordable.join(", ")
                ));
            }
            if !failures.is_empty() {
                out.notes.push(format!("no prices came back for {}", failures.join(", ")));
            }
        }
        Ok(out)
    }
}

/// Hands the traveller a Duffel-hosted checkout. Registered alongside
/// `search_flights` whenever flights are configured.
pub struct BookingLinkTool {
    pub client: DuffelClient,
    pub user_id: i64,
    /// Where Duffel sends the traveller when they are done — the bot's own
    /// Telegram address, so they land back in the conversation.
    pub return_url: String,
}

impl rig::tool::Tool for BookingLinkTool {
    const NAME: &'static str = "create_booking_link";
    type Error = DuffelError;
    type Args = serde_json::Value;
    type Output = BookingLink;

    fn description(&self) -> String {
        "Give the user a link to book a flight. Call this ONLY when they say \
         they want to book, never as part of showing prices. The link opens \
         Duffel's own checkout, where they search, pick their flight and pay \
         — Scout never handles passenger details or card details. The link \
         is single-use and expires, so ask for a fresh one each time rather \
         than repeating an old one. It does not carry the flight already \
         found: tell the user the route, date and price to re-enter, because \
         the checkout starts at its own search box."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(BookingLink {
            url: self.client.booking_link(self.user_id, &self.return_url).await?,
            expires_in: "24 hours if unopened, 20 minutes once opened",
            note: "the checkout opens on its own search box — it cannot be \
                   pre-filled, so repeat the route, date and price for the \
                   user to enter",
        })
    }
}

#[derive(Debug, serde::Serialize)]
pub struct BookingLink {
    pub url: String,
    pub expires_in: &'static str,
    pub note: &'static str,
}

impl FlightSearchTool {
    /// One day of the window: from this request's memo if it is there,
    /// otherwise bought from Duffel and remembered.
    async fn one_day(
        &self,
        key: String,
        day: FlightQuery,
    ) -> (FlightQuery, Result<Vec<Flight>, DuffelError>) {
        if let Some(flights) = self.budget.recall(&key) {
            tracing::info!(
                route = %key,
                bought = self.budget.spent(),
                "flight search served from this request's memo, not bought again"
            );
            return (day, Ok(flights));
        }
        let found = self.client.search(&day).await;
        // Counted whenever the request actually left the machine. A search
        // Duffel then refused is assumed billable: understating the bill is
        // the worse of the two errors.
        self.note_search().await;
        if let Ok(flights) = &found {
            self.budget.remember(key, flights.clone());
        }
        (day, found)
    }

    /// Records one billable search. A failure here is logged and swallowed:
    /// the search has already been paid for, and losing the answer as well
    /// would make a bookkeeping problem into a user-facing one.
    async fn note_search(&self) {
        let (store, user_id) = (self.store.clone(), self.user_id);
        let logged = tokio::task::spawn_blocking(move || {
            store.log_request(user_id, crate::store::Store::FLIGHT_SEARCH)
        })
        .await;
        if let Err(e) = logged.map_err(anyhow::Error::from).and_then(|r| r) {
            tracing::warn!(error = %e, "could not record a flight search for /stat");
        }
    }
}

/// Duffel writes durations as ISO 8601 ("PT2H30M"), including a day part on
/// overnight itineraries ("P1DT2H5M"). Whole minutes is all a traveller
/// needs.
pub fn duration_minutes(iso: &str) -> Option<u32> {
    let rest = iso.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut minutes: u32 = 0;
    // A duration with no recognised component is not zero minutes, it is no
    // answer at all — zero would sort as the fastest flight in the set.
    let mut found = false;
    let mut digits = String::new();

    let mut take = |c: char, digits: &mut String, minutes: &mut u32| -> Option<()> {
        let n: u32 = digits.parse().ok()?;
        digits.clear();
        let per = match c {
            'W' => 7 * 24 * 60,
            'D' => 24 * 60,
            'H' => 60,
            'M' => 1,
            // Seconds are in the grammar but below the resolution anyone
            // reads a flight in, so they round away.
            'S' => 0,
            _ => return None,
        };
        *minutes = minutes.checked_add(n.checked_mul(per)?)?;
        found = true;
        Some(())
    };

    for (section, units) in [(date, "WD"), (time, "HMS")] {
        for c in section.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            if !units.contains(c) {
                return None;
            }
            take(c, &mut digits, &mut minutes)?;
        }
        // A trailing number with no unit ("PT2H30") is malformed.
        if !digits.is_empty() {
            return None;
        }
    }
    found.then_some(minutes)
}

/// Duffel sends money as a decimal string. Anything unparseable is not a
/// price and must not be guessed at.
pub fn amount(raw: &str) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

#[derive(Debug, thiserror::Error)]
pub enum DuffelError {
    #[error("duffel request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("duffel api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("duffel returned an unexpected response: {detail}")]
    Decode { detail: String },
    /// Phrased as an instruction to the model, not a diagnostic.
    #[error("{0}")]
    Invalid(String),
}

/// At most this many options in a reply; Duffel routinely returns dozens of
/// near-identical offers and a chat cannot show them.
const ROW_CAP: usize = 5;
/// A cheaper flight that costs this much extra time is worth pointing out
/// rather than presenting as simply "the cheapest".
const MUCH_SLOWER_MINUTES: u32 = 180;

/// One direction of a trip: outbound is a leg, the return is another.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Leg {
    pub origin: String,
    pub destination: String,
    /// Local time at the departure airport, with no UTC offset — measured
    /// live, LHR 10:03 to JFK 13:01 is a 7h58m flight. The two timestamps
    /// are in different time zones and subtracting them is meaningless,
    /// which is why `duration` is supplied ready-made.
    pub departing_at_local: String,
    /// Local time at the arrival airport. See `departing_at_local`.
    pub arriving_at_local: String,
    pub duration_minutes: Option<u32>,
    /// The same duration written out ("7h 58m"), so nothing downstream has
    /// a reason to do time arithmetic on the two local timestamps.
    pub duration: Option<String>,
    pub stops: u32,
    /// Marketing flight numbers in order, e.g. `["KL1693"]`.
    pub flights: Vec<String>,
    /// Where the traveller changes plane and for how long, in order. Empty
    /// on a direct flight.
    pub connections: Vec<Connection>,
    /// The whole leg drawn on one line, for the reply to print unchanged:
    /// `AMS 20:15 15.09 ✈ PVG 3h 20m ✈ HKG 20:35 16.09`.
    ///
    /// Built here rather than by the model for the same reason the ranking
    /// is: a model retyping departure times is a model that can get one
    /// wrong, and a wrong departure time reads exactly like a right one.
    pub itinerary: String,
}

/// A change of plane part-way through a leg.
///
/// Without this a two-flight itinerary reads as two flight numbers and a
/// total, which cannot answer "where do I change and how long have I got?"
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Connection {
    /// Where the inbound flight lands.
    pub airport: String,
    /// Where the onward flight leaves from, when that is a different
    /// airport; `None` when it is the same one.
    pub departs_from: Option<String>,
    /// Landing and next departure, both local to this airport — so unlike
    /// the times on `Leg`, these two *are* comparable, which is what makes
    /// `layover_minutes` trustworthy.
    pub arriving_at_local: String,
    pub departing_at_local: String,
    pub layover_minutes: Option<u32>,
    /// The wait written out ("3h 20m"). `None` means the offer did not
    /// state both times — never zero, which would read as no wait at all.
    pub layover: Option<String>,
    /// True when the onward flight leaves from a different airport. That is
    /// a coach transfer with your own bags, and no airline protects it.
    pub changes_airport: bool,
}

/// One priced offer. `price` is the whole trip for all passengers, as Duffel
/// states it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Flight {
    pub offer_id: String,
    pub airline: String,
    pub price: f64,
    pub currency: String,
    pub legs: Vec<Leg>,
    /// Sum of the legs; `None` when any leg failed to state one.
    pub total_minutes: Option<u32>,
    /// The same total written out ("7h 58m").
    pub total_duration: Option<String>,
    /// Bags included for the whole trip, per passenger. `None` means the
    /// offer did not say — which is not the same as none included.
    pub checked_bags: Option<u32>,
    pub carry_on_bags: Option<u32>,
    /// When this price stops being bookable.
    pub expires_at: Option<String>,
}

/// The ranked set handed to the model. Named picks first, in the same shape
/// `compare_prices` uses, so the reply can quote them verbatim.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct FlightResults {
    pub currency: String,
    /// Comparable offers before [`ROW_CAP`] is applied, so a reply can say
    /// what it is not showing.
    pub found: usize,
    pub cheapest: Flight,
    /// Quickest total travel time; equals `cheapest` when that is also
    /// fastest, and is absent when no offer stated a duration.
    pub fastest: Option<Flight>,
    /// Every comparable offer, cheapest first, capped at [`ROW_CAP`].
    pub rows: Vec<Flight>,
    pub notes: Vec<String>,
}

/// Ranks offers by price, in Rust, so the model never does this arithmetic.
///
/// `None` when there is nothing rankable.
pub fn rank(flights: Vec<Flight>) -> Option<FlightResults> {
    let currency = dominant_currency(&flights)?;

    let mut dropped: Vec<String> = flights
        .iter()
        .filter(|f| f.currency != currency)
        .map(|f| f.currency.clone())
        .collect();
    dropped.sort();
    dropped.dedup();

    let mut rows: Vec<Flight> = flights.into_iter().filter(|f| f.currency == currency).collect();
    // Prices are finite by construction (see `amount`), so an unorderable
    // pair cannot arise; treating one as equal keeps the sort total anyway.
    rows.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
    let found = rows.len();

    let cheapest = rows.first()?.clone();
    let fastest = rows
        .iter()
        .filter_map(|f| f.total_minutes.map(|m| (m, f)))
        .min_by_key(|(m, _)| *m)
        .map(|(_, f)| f.clone());

    let mut notes = Vec::new();
    if !dropped.is_empty() {
        notes.push(format!(
            "prices came back in more than one currency; {} offers priced in {} were left out \
             rather than compared against {currency}",
            dropped.len(),
            dropped.join(", ")
        ));
    }
    // A cheapest option that costs most of a day is a different product from
    // one that costs three hours, and the price alone does not say so.
    if let (Some(quick), Some(slow)) = (fastest.as_ref(), cheapest.total_minutes) {
        if let Some(quick_minutes) = quick.total_minutes {
            let extra = slow.saturating_sub(quick_minutes);
            if extra >= MUCH_SLOWER_MINUTES {
                notes.push(format!(
                    "the cheapest option takes {} longer than the quickest, which costs {:.2} \
                     {currency} more",
                    human_duration(extra),
                    quick.price - cheapest.price
                ));
            }
        }
    }
    // A fare with no bags against one that includes them is not the price
    // difference it looks like — a hold bag at the airport routinely costs
    // more than the gap. Same trap as comparing a 3-pack with a single.
    if cheapest.checked_bags == Some(0) {
        if let Some(with_bags) = rows.iter().find(|f| f.checked_bags.unwrap_or(0) > 0) {
            notes.push(format!(
                "the cheapest option includes no checked bag; {} at {:.2} {currency} includes \
                 {} — add the airline's bag fee before calling the cheaper one cheaper",
                with_bags.airline,
                with_bags.price,
                with_bags.checked_bags.unwrap_or(0)
            ));
        }
    }
    if rows.len() > ROW_CAP {
        notes.push(format!(
            "{found} offers were returned; these are the {ROW_CAP} cheapest"
        ));
        rows.truncate(ROW_CAP);
    }

    Some(FlightResults { currency, found, cheapest, fastest, rows, notes })
}

pub const DUFFEL_API_BASE: &str = "https://api.duffel.com";
/// Duffel pins behaviour to this header; without it the API refuses the call.
const DUFFEL_VERSION: &str = "v2";
/// Passengers one household books at once. Beyond this it is a group booking
/// and belongs on the airline's own site.
const MAX_ADULTS: u32 = 9;
/// The only values Duffel accepts; anything else is a 422.
const CABIN_CLASSES: [&str; 4] = ["economy", "premium_economy", "business", "first"];
/// Widest flexible window. Duffel prices one day per search, so ±3 is
/// already seven of them; a month would be thirty, which is what the big
/// sites answer from a cache of indicative fares rather than live offers.
pub const MAX_FLEX_DAYS: u8 = 3;

/// What the model asks for. IATA codes because that is what Duffel takes —
/// resolving "Lisbon" to LIS is the model's job, and getting it wrong is
/// visible in the reply rather than silent.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FlightQuery {
    pub origin: String,
    pub destination: String,
    pub departure_date: String,
    /// Absent for a one-way.
    #[serde(default)]
    pub return_date: Option<String>,
    #[serde(default)]
    pub adults: Option<u32>,
    #[serde(default)]
    pub cabin_class: Option<String>,
    #[serde(default)]
    pub max_connections: Option<u8>,
    /// Also search this many days either side of `departure_date`. Duffel
    /// has no calendar endpoint, so a window costs one search per day —
    /// `Some(3)` is seven searches. Capped at [`MAX_FLEX_DAYS`].
    #[serde(default)]
    pub flex_days: Option<u8>,
}

impl FlightQuery {
    /// Rejects what Duffel would reject, but with a message the model can
    /// act on. A search costs money whether or not the codes were real.
    pub fn validate(&self) -> Result<(), DuffelError> {
        let origin = iata_code("origin", &self.origin)?;
        let destination = iata_code("destination", &self.destination)?;
        if origin == destination {
            return Err(DuffelError::Invalid(format!(
                "origin and destination are both {origin}; a flight needs two different places"
            )));
        }

        calendar_date("departure_date", &self.departure_date)?;
        if let Some(back) = &self.return_date {
            calendar_date("return_date", back)?;
            // ISO dates compare correctly as text, which is the one place
            // that is true of them.
            if back.trim() < self.departure_date.trim() {
                return Err(DuffelError::Invalid(format!(
                    "return_date {} is before departure_date {}",
                    back.trim(),
                    self.departure_date.trim()
                )));
            }
        }

        if let Some(adults) = self.adults {
            if !(1..=MAX_ADULTS).contains(&adults) {
                return Err(DuffelError::Invalid(format!(
                    "adults must be between 1 and {MAX_ADULTS}, got {adults}"
                )));
            }
        }
        if let Some(cabin) = &self.cabin_class {
            let cabin = cabin.trim().to_ascii_lowercase();
            if !CABIN_CLASSES.contains(&cabin.as_str()) {
                return Err(DuffelError::Invalid(format!(
                    "cabin_class must be one of {}, not {cabin:?}",
                    CABIN_CLASSES.join(", ")
                )));
            }
        }
        if let Some(max) = self.max_connections {
            if max > 2 {
                return Err(DuffelError::Invalid(format!(
                    "max_connections must be 0, 1 or 2, got {max}"
                )));
            }
        }
        if let Some(flex) = self.flex_days {
            if flex > MAX_FLEX_DAYS {
                return Err(DuffelError::Invalid(format!(
                    "flex_days must be at most {MAX_FLEX_DAYS}, got {flex} — each extra day is \
                     a separate paid search, so a wider window is not available; search the \
                     dates that matter most"
                )));
            }
        }
        Ok(())
    }

    /// One query per day of the window, nearest the requested date first.
    ///
    /// The order is the priority order: if the allowance runs out part way,
    /// what is lost is the far edge of the window rather than the day the
    /// traveller actually named.
    fn window(&self) -> Result<Vec<FlightQuery>, DuffelError> {
        let flex = i64::from(self.flex_days.unwrap_or(0));
        let parse = |label: &str, s: &str| {
            chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .map_err(|_| DuffelError::Invalid(format!("{label} is not a real date: {s:?}")))
        };
        let out_date = parse("departure_date", &self.departure_date)?;
        let back_date = self.return_date.as_deref().map(|d| parse("return_date", d)).transpose()?;

        // 0, -1, +1, -2, +2 … so the requested day is always bought first.
        let offsets = std::iter::once(0)
            .chain((1..=flex).flat_map(|n| [-n, n]))
            .collect::<Vec<i64>>();

        Ok(offsets
            .into_iter()
            .map(|offset| {
                let shift = chrono::Duration::days(offset);
                FlightQuery {
                    departure_date: (out_date + shift).format("%Y-%m-%d").to_string(),
                    // The return moves with the outbound, so a week away
                    // stays a week away — and the window stays seven
                    // searches instead of forty-nine.
                    return_date: back_date.map(|d| (d + shift).format("%Y-%m-%d").to_string()),
                    flex_days: None,
                    ..self.clone()
                }
            })
            .collect())
    }

    /// Identity of this search for the per-request memo. Normalised so the
    /// same journey asked twice — "ams" then "AMS", `adults: null` then
    /// `adults: 1` — is recognised as one search rather than bought twice.
    pub fn cache_key(&self) -> String {
        format!(
            "{}-{}|{}|{}|{}|{}|{}",
            self.origin.trim().to_ascii_uppercase(),
            self.destination.trim().to_ascii_uppercase(),
            self.departure_date.trim(),
            self.return_date.as_deref().unwrap_or("").trim(),
            self.adults.unwrap_or(1),
            self.cabin_class.as_deref().unwrap_or("").trim().to_ascii_lowercase(),
            self.max_connections.map(|m| m.to_string()).unwrap_or_default(),
        )
    }

    /// The `POST /air/offer_requests` body.
    pub fn body(&self) -> serde_json::Value {
        let origin = self.origin.trim().to_ascii_uppercase();
        let destination = self.destination.trim().to_ascii_uppercase();
        let mut slices = vec![serde_json::json!({
            "origin": origin,
            "destination": destination,
            "departure_date": self.departure_date.trim(),
        })];
        if let Some(back) = &self.return_date {
            slices.push(serde_json::json!({
                "origin": destination,
                "destination": origin,
                "departure_date": back.trim(),
            }));
        }
        let passengers: Vec<serde_json::Value> = (0..self.adults.unwrap_or(1))
            .map(|_| serde_json::json!({"type": "adult"}))
            .collect();

        let mut data = serde_json::json!({"slices": slices, "passengers": passengers});
        // Sent only when asked for: an unset filter is not the same as a
        // filter set to its default, and Duffel prices them differently.
        if let Some(cabin) = &self.cabin_class {
            data["cabin_class"] = serde_json::json!(cabin.trim().to_ascii_lowercase());
        }
        if let Some(max) = self.max_connections {
            data["max_connections"] = serde_json::json!(max);
        }
        serde_json::json!({"data": data})
    }
}

/// Duffel offer-request client. Search only — it has no method that books.
#[derive(Clone)]
pub struct DuffelClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Booking fee as a rate (0.03 = 3%). Applied to quoted prices *and*
    /// sent to Duffel Links, so what Scout says and what the checkout
    /// charges are the same number by construction. Zero means none.
    markup_rate: f64,
}

impl DuffelClient {
    pub fn new(http: reqwest::Client, api_key: String, base_url: String) -> Self {
        Self { http, api_key, base_url, markup_rate: 0.0 }
    }

    pub fn with_markup(mut self, rate: f64) -> Self {
        self.markup_rate = rate.max(0.0);
        self
    }

    pub fn markup_rate(&self) -> f64 {
        self.markup_rate
    }

    /// A Duffel-hosted checkout for one traveller.
    ///
    /// Single-use and short-lived: unused sessions die after 24 hours, and
    /// a used one after 20 minutes. So this is called when someone asks to
    /// book, never in advance.
    pub async fn booking_link(&self, user_id: i64, return_url: &str) -> Result<String, DuffelError> {
        let mut data = serde_json::json!({
            // Comes back on the order, so a booking can be traced to
            // whoever asked for the link.
            "reference": user_id.to_string(),
            "success_url": return_url,
            "failure_url": return_url,
            "abandonment_url": return_url,
            "flights": {"enabled": true},
            // Scout knows nothing about hotels and should not sell them.
            "stays": {"enabled": false},
        });
        if self.markup_rate > 0.0 {
            // A rate the operator never configured must not be sent as an
            // explicit zero.
            data["markup_rate"] = serde_json::json!(format!("{:.2}", self.markup_rate));
        }

        let resp = self
            .http
            .post(format!("{}/links/sessions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Duffel-Version", DUFFEL_VERSION)
            .header("Accept", "application/json")
            .json(&serde_json::json!({"data": data}))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(DuffelError::Api {
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["data"]["url"].as_str().map(String::from))
            .ok_or_else(|| DuffelError::Decode {
                detail: format!("no session url in: {}", text.chars().take(200).collect::<String>()),
            })
    }

    pub async fn search(&self, query: &FlightQuery) -> Result<Vec<Flight>, DuffelError> {
        // Checked before the request, not after: Duffel bills per search, so
        // a call the model got wrong should cost nothing.
        query.validate()?;
        let resp = self
            .http
            .post(format!("{}/air/offer_requests", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Duffel-Version", DUFFEL_VERSION)
            .header("Accept", "application/json")
            // Without this the offers arrive empty and each search needs a
            // second round trip to be worth anything.
            .query(&[("return_offers", "true")])
            .json(&query.body())
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(DuffelError::Api {
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }
        let mut flights = parse_offers(&text)?;
        // Quoted with the fee already on, because Duffel Links will charge
        // it: saying 600 and billing 618 is exactly the gap this project
        // exists to close. A uniform rate cannot reorder the set, so the
        // ranking means the same thing either way.
        if self.markup_rate > 0.0 {
            for flight in &mut flights {
                flight.price = round_money(flight.price * (1.0 + self.markup_rate));
            }
        }
        Ok(flights)
    }
}

/// A rate as a percentage a person would say aloud: 0.03 -> "3%".
fn percentage(rate: f64) -> String {
    let pct = format!("{:.2}", rate * 100.0);
    let pct = pct.trim_end_matches('0').trim_end_matches('.');
    format!("{pct}%")
}

/// Money, to the cent. A markup multiplication otherwise produces prices
/// like 205.99999999999997.
fn round_money(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

/// Duffel takes IATA codes only, so "Amsterdam" is a 422 and a wasted search
/// fee. The message tells the model how to fix it.
fn iata_code(label: &str, value: &str) -> Result<String, DuffelError> {
    let code = value.trim();
    match code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        true => Ok(code.to_ascii_uppercase()),
        false => Err(DuffelError::Invalid(format!(
            "{label} must be a 3-letter IATA airport or city code (AMS, LHR, NYC), not {value:?} \
             — use the code for the place the traveller named"
        ))),
    }
}

fn calendar_date(label: &str, value: &str) -> Result<(), DuffelError> {
    let date = value.trim();
    let shaped = date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .char_indices()
            .all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    match shaped {
        true => Ok(()),
        false => Err(DuffelError::Invalid(format!(
            "{label} must be a date written YYYY-MM-DD, not {value:?}"
        ))),
    }
}

/// What the model receives. Shaped so an empty result is an ordinary answer
/// with `found: 0` rather than a failure — "nothing flies that route on that
/// day" is a real finding, and dressing it as an error makes the reply
/// apologise instead of saying so.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct FlightSearchOutput {
    /// Echoed back so the reply cannot quote a route nobody searched.
    pub route: String,
    /// Comparable offers before capping.
    pub found: usize,
    pub currency: Option<String>,
    pub cheapest: Option<Flight>,
    pub fastest: Option<Flight>,
    pub rows: Vec<Flight>,
    /// Cheapest per day across a flexible window, in date order. Empty for
    /// an ordinary single-date search.
    pub by_date: Vec<DayPrice>,
    pub notes: Vec<String>,
}

/// What one day of a flexible window cost. `cheapest` is `None` when
/// nothing flew that day — which is an answer, not a gap.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct DayPrice {
    pub date: String,
    pub cheapest: Option<f64>,
    pub currency: Option<String>,
    pub airline: Option<String>,
    pub found: usize,
}

impl DayPrice {
    fn new(date: &str, flights: &[Flight]) -> Self {
        let cheapest = flights
            .iter()
            .min_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            date: date.to_string(),
            cheapest: cheapest.map(|f| f.price),
            currency: cheapest.map(|f| f.currency.clone()),
            airline: cheapest.map(|f| f.airline.clone()),
            found: flights.len(),
        }
    }
}

impl FlightSearchOutput {
    pub fn new(query: &FlightQuery, ranked: Option<FlightResults>) -> Self {
        let mut route = format!(
            "{}-{} {}",
            query.origin.trim().to_ascii_uppercase(),
            query.destination.trim().to_ascii_uppercase(),
            query.departure_date.trim()
        );
        if let Some(back) = &query.return_date {
            route.push_str(&format!(", back {}", back.trim()));
        }
        // The window is part of what was asked, so it belongs in the label
        // the reply quotes — otherwise a range reads as a single date.
        if let Some(flex) = query.flex_days.filter(|f| *f > 0) {
            route.push_str(&format!(" ±{flex}"));
        }

        match ranked {
            Some(r) => Self {
                route,
                found: r.found,
                currency: Some(r.currency),
                cheapest: Some(r.cheapest),
                fastest: r.fastest,
                rows: r.rows,
                by_date: Vec::new(),
                notes: r.notes,
            },
            None => Self {
                notes: vec![format!(
                    "no flights came back for {route} — tell the user plainly that nothing is \
                     on offer for that route and date; a nearby date or a different airport \
                     may have some, but do not claim one without searching it"
                )],
                route,
                found: 0,
                currency: None,
                cheapest: None,
                fastest: None,
                rows: Vec::new(),
                by_date: Vec::new(),
            },
        }
    }
}

/// Reads the offers out of an offer-request response.
///
/// Offers Duffel states no usable price for are dropped, never defaulted:
/// a zero would sort straight to the top and be reported as a free flight.
pub fn parse_offers(body: &str) -> Result<Vec<Flight>, DuffelError> {
    let parsed: Response = serde_json::from_str(body).map_err(|e| DuffelError::Decode {
        detail: format!("{e}; body: {}", body.chars().take(200).collect::<String>()),
    })?;
    Ok(parsed.data.offers.into_iter().filter_map(flight).collect())
}

/// One offer, or `None` when it lacks the things that make it an offer.
fn flight(raw: RawOffer) -> Option<Flight> {
    let price = amount(raw.total_amount.as_deref()?)?;
    let currency = raw.total_currency.filter(|c| !c.trim().is_empty())?;
    let legs: Vec<Leg> = raw.slices.iter().filter_map(leg).collect();
    let total_minutes = match legs.is_empty() {
        true => None,
        false => legs.iter().map(|l| l.duration_minutes).sum(),
    };
    Some(Flight {
        offer_id: raw.id.unwrap_or_default(),
        airline: raw
            .owner
            .and_then(|a| a.name.or(a.iata_code))
            .unwrap_or_default(),
        price,
        currency,
        // An itinerary with no legs has no duration — summing an empty set
        // to zero would make it the fastest flight on offer.
        total_minutes,
        total_duration: total_minutes.map(human_duration),
        checked_bags: bags(&raw.slices, "checked"),
        carry_on_bags: bags(&raw.slices, "carry_on"),
        expires_at: raw.expires_at,
        legs,
    })
}

/// One direction. A change of plane is a stop on this leg, not a leg of its
/// own: the traveller asked to reach the destination, and where they change
/// is a property of that journey.
fn leg(slice: &RawSlice) -> Option<Leg> {
    let first = slice.segments.first()?;
    let last = slice.segments.last()?;
    let iata = |p: &Option<RawPlace>| p.as_ref().and_then(|p| p.iata_code.clone());
    let duration_minutes = slice
        .duration
        .as_deref()
        .and_then(duration_minutes)
        // Without a slice duration, the segments still add up — but only if
        // every one of them states its own.
        .or_else(|| {
            slice
                .segments
                .iter()
                .map(|s| s.duration.as_deref().and_then(duration_minutes))
                .sum()
        });
    let origin = iata(&first.origin).or_else(|| iata(&slice.origin)).unwrap_or_default();
    let destination = iata(&last.destination)
        .or_else(|| iata(&slice.destination))
        .unwrap_or_default();
    let connections: Vec<Connection> = slice
        .segments
        .windows(2)
        .map(|pair| connection(&pair[0], &pair[1]))
        .collect();

    Some(Leg {
        itinerary: itinerary(&origin, &destination, first, last, &connections),
        origin,
        destination,
        departing_at_local: first.departing_at.clone().unwrap_or_default(),
        arriving_at_local: last.arriving_at.clone().unwrap_or_default(),
        duration_minutes,
        duration: duration_minutes.map(human_duration),
        // Changes of plane, plus any en-route stop within a single segment
        // (a technical stop still lands the aircraft).
        stops: (slice.segments.len() - 1) as u32
            + slice.segments.iter().map(|s| s.stops.len() as u32).sum::<u32>(),
        flights: slice
            .segments
            .iter()
            .map(|s| {
                let carrier = s
                    .marketing_carrier
                    .as_ref()
                    .and_then(|c| c.iata_code.clone())
                    .unwrap_or_default();
                format!("{carrier}{}", s.marketing_carrier_flight_number.clone().unwrap_or_default())
            })
            .collect(),
        connections,
    })
}

/// One leg on a single line, ends outward: where you leave from and when,
/// each change and how long you wait there, where you land and when.
///
/// Anything the offer did not state is left out rather than guessed —
/// half a timestamp invites the reader to assume the other half.
fn itinerary(
    origin: &str,
    destination: &str,
    first: &RawSegment,
    last: &RawSegment,
    connections: &[Connection],
) -> String {
    const HOP: &str = " ✈ ";
    let mut parts = vec![stamped(origin, first.departing_at.as_deref())];
    for stop in connections {
        // A change of airport shows both, because it is the difference
        // between a walk to the next gate and a coach across a city.
        let place = match &stop.departs_from {
            Some(onward) => format!("{}/{onward}", stop.airport),
            None => stop.airport.clone(),
        };
        parts.push(match &stop.layover {
            Some(wait) => format!("{place} {wait}"),
            None => place,
        });
    }
    parts.push(stamped(destination, last.arriving_at.as_deref()));
    parts.join(HOP)
}

/// `AMS 20:15 15.09`, or bare `AMS` when there is no usable timestamp.
fn stamped(airport: &str, at: Option<&str>) -> String {
    let clock = at
        .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
        .map(|t| t.format("%H:%M %d.%m").to_string());
    match clock {
        Some(when) => format!("{airport} {when}"),
        None => airport.to_string(),
    }
}

/// The gap between landing on one flight and leaving on the next.
fn connection(inbound: &RawSegment, onward: &RawSegment) -> Connection {
    let iata = |p: &Option<RawPlace>| p.as_ref().and_then(|p| p.iata_code.clone());
    let landed_at = iata(&inbound.destination).unwrap_or_default();
    let leaves_from = iata(&onward.origin).unwrap_or_default();
    let changes_airport = !leaves_from.is_empty() && leaves_from != landed_at;

    let arriving = inbound.arriving_at.clone().unwrap_or_default();
    let departing = onward.departing_at.clone().unwrap_or_default();
    // Both stamps belong to this one airport, so the subtraction is sound
    // here even though it is meaningless between the ends of a leg. A DST
    // shift during the wait would move it by an hour; nothing in the
    // response says which zone this is, so that is left alone.
    let layover_minutes = minutes_between(&arriving, &departing);

    Connection {
        airport: landed_at,
        departs_from: changes_airport.then_some(leaves_from),
        arriving_at_local: arriving,
        departing_at_local: departing,
        layover_minutes,
        layover: layover_minutes.map(human_duration),
        changes_airport,
    }
}

/// Whole minutes from `from` to `to`, both as Duffel writes them
/// (`2026-09-16T14:30:00`). `None` unless both parse and time moves
/// forwards — a backwards gap means the two are not the same clock, and a
/// negative wait is worse than no answer.
fn minutes_between(from: &str, to: &str) -> Option<u32> {
    let parse = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok();
    let minutes = (parse(to)? - parse(from)?).num_minutes();
    u32::try_from(minutes).ok()
}

/// Bags of `kind` included for the whole trip, per passenger.
///
/// The trip allowance is the *smallest* any segment grants: a checked bag on
/// the way out and none on the way home is not a checked bag included, and
/// reporting it as one is how someone pays at the gate.
fn bags(slices: &[RawSlice], kind: &str) -> Option<u32> {
    let mut smallest: Option<u32> = None;
    for segment in slices.iter().flat_map(|s| &s.segments) {
        // No passenger breakdown means the offer did not say, which is not
        // the same as nothing included.
        let passenger = segment.passengers.first()?;
        let quantity: u32 = passenger
            .baggages
            .iter()
            .filter(|b| b.kind.as_deref() == Some(kind))
            .map(|b| b.quantity.unwrap_or(0))
            .sum();
        smallest = Some(smallest.map_or(quantity, |s: u32| s.min(quantity)));
    }
    smallest
}

#[derive(serde::Deserialize)]
struct Response {
    data: ResponseData,
}

#[derive(serde::Deserialize)]
struct ResponseData {
    #[serde(default)]
    offers: Vec<RawOffer>,
}

#[derive(serde::Deserialize)]
struct RawOffer {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    total_amount: Option<String>,
    #[serde(default)]
    total_currency: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    owner: Option<RawAirline>,
    #[serde(default)]
    slices: Vec<RawSlice>,
}

#[derive(serde::Deserialize)]
struct RawAirline {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    iata_code: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawSlice {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    origin: Option<RawPlace>,
    #[serde(default)]
    destination: Option<RawPlace>,
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(serde::Deserialize)]
struct RawPlace {
    #[serde(default)]
    iata_code: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawSegment {
    #[serde(default)]
    origin: Option<RawPlace>,
    #[serde(default)]
    destination: Option<RawPlace>,
    #[serde(default)]
    departing_at: Option<String>,
    #[serde(default)]
    arriving_at: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    marketing_carrier: Option<RawAirline>,
    #[serde(default)]
    marketing_carrier_flight_number: Option<String>,
    #[serde(default)]
    stops: Vec<serde_json::Value>,
    #[serde(default)]
    passengers: Vec<RawSegmentPassenger>,
}

#[derive(serde::Deserialize)]
struct RawSegmentPassenger {
    #[serde(default)]
    baggages: Vec<RawBaggage>,
}

#[derive(serde::Deserialize)]
struct RawBaggage {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    quantity: Option<u32>,
}

/// The currency most offers are priced in. Ties keep the one seen first, so
/// the same response always ranks the same way.
fn dominant_currency(flights: &[Flight]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for f in flights {
        match counts.iter_mut().find(|(c, _)| *c == f.currency) {
            Some((_, n)) => *n += 1,
            None => counts.push((f.currency.clone(), 1)),
        }
    }
    counts
        .into_iter()
        .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
        .map(|(c, _)| c)
}

/// Minutes as a traveller reads them: "9h", "1h 25m", "45m".
fn human_duration(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flight(id: &str, price: f64, currency: &str, minutes: Option<u32>) -> Flight {
        Flight {
            offer_id: id.to_string(),
            airline: "KLM".to_string(),
            price,
            currency: currency.to_string(),
            legs: Vec::new(),
            total_minutes: minutes,
            total_duration: minutes.map(human_duration),
            checked_bags: None,
            carry_on_bags: None,
            expires_at: None,
        }
    }

    #[test]
    fn the_cheapest_offer_wins_and_rows_run_cheapest_first() {
        let ranked = rank(vec![
            flight("c", 1000.00, "EUR", Some(200)),
            flight("a", 62.19, "EUR", Some(400)),
            flight("b", 184.00, "EUR", Some(300)),
        ])
        .unwrap();
        assert_eq!(ranked.cheapest.offer_id, "a");
        assert_eq!(ranked.currency, "EUR");
        assert_eq!(
            ranked.rows.iter().map(|f| f.offer_id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn the_fastest_is_by_time_and_ignores_offers_that_state_none() {
        let ranked = rank(vec![
            flight("slow", 100.0, "EUR", Some(600)),
            flight("quick", 300.0, "EUR", Some(140)),
            // No duration must not read as zero minutes and win.
            flight("unknown", 90.0, "EUR", None),
        ])
        .unwrap();
        assert_eq!(ranked.cheapest.offer_id, "unknown");
        assert_eq!(ranked.fastest.unwrap().offer_id, "quick");
    }

    #[test]
    fn with_no_durations_at_all_there_is_no_fastest() {
        let ranked = rank(vec![flight("a", 100.0, "EUR", None)]).unwrap();
        assert_eq!(ranked.fastest, None);
    }

    #[test]
    fn a_cheapest_that_costs_hours_more_is_called_out() {
        let ranked = rank(vec![
            flight("cheap", 100.0, "EUR", Some(700)),
            flight("quick", 130.0, "EUR", Some(160)),
        ])
        .unwrap();
        // 9h longer for EUR 30: the reader should be told, not left to
        // discover it in the times.
        assert!(
            ranked.notes.iter().any(|n| n.contains("9h") && n.contains("30")),
            "expected a slower-cheapest note, got: {:?}",
            ranked.notes
        );

        // A modest difference is not worth a note.
        let ranked = rank(vec![
            flight("cheap", 100.0, "EUR", Some(200)),
            flight("quick", 130.0, "EUR", Some(160)),
        ])
        .unwrap();
        assert!(ranked.notes.is_empty(), "got: {:?}", ranked.notes);
    }

    #[test]
    fn offers_in_another_currency_are_dropped_rather_than_compared() {
        // Comparing 60 GBP against 100 EUR by their numbers is how a dearer
        // flight would win.
        let ranked = rank(vec![
            flight("eur1", 100.0, "EUR", Some(200)),
            flight("eur2", 120.0, "EUR", Some(180)),
            flight("gbp", 60.0, "GBP", Some(190)),
        ])
        .unwrap();
        assert_eq!(ranked.currency, "EUR");
        assert_eq!(ranked.cheapest.offer_id, "eur1");
        assert!(ranked.rows.iter().all(|f| f.currency == "EUR"));
        assert!(
            ranked.notes.iter().any(|n| n.contains("GBP")),
            "the dropped currency must be mentioned, got: {:?}",
            ranked.notes
        );
    }

    #[test]
    fn a_cheapest_with_no_baggage_against_a_fare_that_has_it_is_called_out() {
        // Measured live on AMS-LIS: Transavia at 143.25 with no bags at all,
        // British Airways at 156.20 with one checked and one cabin bag. The
        // 13 EUR gap is not the real gap, and a hold bag costs more than it
        // at the airport. This is the 3-pack-versus-single-bottle mistake
        // wearing a different hat.
        let mut cheap = flight("budget", 143.25, "EUR", Some(365));
        cheap.checked_bags = Some(0);
        cheap.carry_on_bags = Some(0);
        let mut full = flight("flag", 156.20, "EUR", Some(364));
        full.checked_bags = Some(1);
        full.carry_on_bags = Some(1);

        let notes = rank(vec![cheap, full]).unwrap().notes;
        assert!(
            notes.iter().any(|n| n.contains("bag")),
            "expected a baggage note, got: {notes:?}"
        );

        // Both without bags is not a difference worth a note.
        let mut a = flight("a", 100.0, "EUR", Some(200));
        a.checked_bags = Some(0);
        let mut b = flight("b", 120.0, "EUR", Some(200));
        b.checked_bags = Some(0);
        assert!(rank(vec![a, b]).unwrap().notes.is_empty());

        // Nor is an offer that simply did not say.
        let mut a = flight("a", 100.0, "EUR", Some(200));
        a.checked_bags = None;
        let b = flight("b", 120.0, "EUR", Some(200));
        assert!(rank(vec![a, b]).unwrap().notes.is_empty());
    }

    #[test]
    fn rows_are_capped_but_the_count_is_stated() {
        let many: Vec<Flight> = (0..9)
            .map(|i| flight(&format!("o{i}"), 100.0 + i as f64, "EUR", Some(200)))
            .collect();
        let ranked = rank(many).unwrap();
        assert_eq!(ranked.rows.len(), ROW_CAP);
        assert!(
            ranked.notes.iter().any(|n| n.contains('9')),
            "a truncated set must say how many there were, got: {:?}",
            ranked.notes
        );
    }

    #[test]
    fn nothing_to_rank_is_none_rather_than_an_empty_shell() {
        assert!(rank(Vec::new()).is_none());
    }

    /// One segment, with the fields Duffel's own client types declare.
    fn segment(from: &str, to: &str, dep: &str, arr: &str, flight_no: &str, bags: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "origin": {"iata_code": from},
            "destination": {"iata_code": to},
            "departing_at": dep,
            "arriving_at": arr,
            "marketing_carrier": {"name": "KLM", "iata_code": "KL"},
            "marketing_carrier_flight_number": flight_no,
            "stops": [],
            "passengers": [{"cabin_class": "economy", "baggages": bags}]
        })
    }

    fn one_bag_each() -> serde_json::Value {
        serde_json::json!([
            {"type": "carry_on", "quantity": 1},
            {"type": "checked", "quantity": 1}
        ])
    }

    #[test]
    fn an_offer_becomes_a_flight_with_its_leg_intact() {
        let body = serde_json::json!({"data": {"id": "orq_1", "offers": [{
            "id": "off_1",
            "total_amount": "184.30",
            "total_currency": "EUR",
            "expires_at": "2026-09-01T10:32:00Z",
            "owner": {"name": "KLM", "iata_code": "KL"},
            "slices": [{
                "duration": "PT3H5M",
                "origin": {"iata_code": "AMS"},
                "destination": {"iata_code": "LIS"},
                "segments": [segment(
                    "AMS", "LIS",
                    "2026-09-14T09:15:00", "2026-09-14T11:20:00",
                    "1693", one_bag_each()
                )]
            }]
        }]}})
        .to_string();

        let flights = parse_offers(&body).unwrap();
        assert_eq!(flights.len(), 1);
        let f = &flights[0];
        assert_eq!(f.offer_id, "off_1");
        assert_eq!(f.airline, "KLM");
        assert_eq!(f.price, 184.30);
        assert_eq!(f.currency, "EUR");
        assert_eq!(f.total_minutes, Some(185));
        assert_eq!(f.checked_bags, Some(1));
        assert_eq!(f.carry_on_bags, Some(1));
        assert_eq!(f.expires_at.as_deref(), Some("2026-09-01T10:32:00Z"));
        assert_eq!(
            f.legs,
            vec![Leg {
                origin: "AMS".into(),
                destination: "LIS".into(),
                departing_at_local: "2026-09-14T09:15:00".into(),
                arriving_at_local: "2026-09-14T11:20:00".into(),
                duration_minutes: Some(185),
                duration: Some("3h 5m".into()),
                stops: 0,
                flights: vec!["KL1693".into()],
                connections: Vec::new(),
                itinerary: "AMS 09:15 14.09 ✈ LIS 11:20 14.09".into(),
            }]
        );
    }

    #[test]
    fn a_ready_made_duration_is_supplied_because_the_times_are_local() {
        // Measured live: LHR 10:03 -> JFK 13:01 is 2h58m on the clock and
        // 7h58m in the air. Duffel states each time in the local time of its
        // own airport with no offset, so subtracting them is five hours
        // wrong. Handing over the formatted duration means nothing
        // downstream has a reason to try.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_live",
            "total_amount": "218.99",
            "total_currency": "EUR",
            "owner": {"name": "American Airlines", "iata_code": "AA"},
            "slices": [{
                "duration": "PT7H58M",
                "origin": {"iata_code": "LHR"}, "destination": {"iata_code": "JFK"},
                "segments": [segment(
                    "LHR", "JFK",
                    "2026-09-14T10:03:00", "2026-09-14T13:01:00",
                    "10", one_bag_each()
                )]
            }]
        }]}})
        .to_string();

        let f = parse_offers(&body).unwrap().remove(0);
        assert_eq!(f.legs[0].duration.as_deref(), Some("7h 58m"));
        assert_eq!(f.total_duration.as_deref(), Some("7h 58m"));
        // The raw timestamps stay, but named for what they actually are.
        assert_eq!(f.legs[0].departing_at_local, "2026-09-14T10:03:00");
        assert_eq!(f.legs[0].arriving_at_local, "2026-09-14T13:01:00");
    }

    #[test]
    fn a_connection_is_one_leg_with_a_stop_not_two_legs() {
        // The traveller asked to get to Lisbon; changing at Paris is a
        // property of that journey, not a second journey.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_2",
            "total_amount": "142.00",
            "total_currency": "EUR",
            "owner": {"name": "Air France", "iata_code": "AF"},
            "slices": [{
                "duration": "PT6H40M",
                "origin": {"iata_code": "AMS"},
                "destination": {"iata_code": "LIS"},
                "segments": [
                    segment("AMS", "CDG", "2026-09-14T06:30:00", "2026-09-14T07:50:00", "1241", one_bag_each()),
                    segment("CDG", "LIS", "2026-09-14T10:15:00", "2026-09-14T12:10:00", "1024", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap()[0].legs[0].clone();
        assert_eq!(leg.origin, "AMS");
        assert_eq!(leg.destination, "LIS");
        assert_eq!(leg.departing_at_local, "2026-09-14T06:30:00");
        assert_eq!(leg.arriving_at_local, "2026-09-14T12:10:00");
        assert_eq!(leg.stops, 1);
        assert_eq!(leg.flights, vec!["KL1241", "KL1024"]);
    }

    #[test]
    fn a_connection_states_its_airport_and_how_long_the_wait_is() {
        // Reported from a live chat: AMS-HKG on China Eastern came back as
        // two flight numbers and a total, so the reply could not say where
        // the change was or how long it lasted, and offered to go and look
        // it up elsewhere. Duffel sends both segments; the old leg() threw
        // their airports and times away.
        //
        // The layover arithmetic is safe where the journey arithmetic is
        // not: arrival and next departure are both local to the *same*
        // airport, so subtracting them is meaningful.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_mu",
            "total_amount": "612.00",
            "total_currency": "EUR",
            "owner": {"name": "China Eastern", "iata_code": "MU"},
            "slices": [{
                "duration": "PT20H35M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "HKG"},
                "segments": [
                    segment("AMS", "PVG", "2026-09-15T20:15:00", "2026-09-16T14:30:00", "0772", one_bag_each()),
                    segment("PVG", "HKG", "2026-09-16T17:50:00", "2026-09-16T20:35:00", "0505", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(leg.connections.len(), 1);
        let stop = &leg.connections[0];
        assert_eq!(stop.airport, "PVG");
        assert_eq!(stop.arriving_at_local, "2026-09-16T14:30:00");
        assert_eq!(stop.departing_at_local, "2026-09-16T17:50:00");
        assert_eq!(stop.layover.as_deref(), Some("3h 20m"));
        assert!(!stop.changes_airport);
    }

    #[test]
    fn a_leg_draws_itself_as_a_strip_the_reply_can_print_verbatim() {
        // Built in Rust and copied out unchanged, for the same reason the
        // ranking is: a model retyping departure times is a model that can
        // get one wrong.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_mu", "total_amount": "612.00", "total_currency": "EUR",
            "owner": {"name": "China Eastern", "iata_code": "MU"},
            "slices": [{
                "duration": "PT20H35M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "HKG"},
                "segments": [
                    segment("AMS", "PVG", "2026-09-15T20:15:00", "2026-09-16T14:30:00", "0772", one_bag_each()),
                    segment("PVG", "HKG", "2026-09-16T17:50:00", "2026-09-16T20:35:00", "0505", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(
            leg.itinerary,
            "AMS 20:15 15.09 ✈ PVG 3h 20m ✈ HKG 20:35 16.09"
        );
    }

    #[test]
    fn a_direct_flight_draws_as_a_single_hop() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_direct", "total_amount": "184.30", "total_currency": "EUR",
            "owner": {"name": "KLM", "iata_code": "KL"},
            "slices": [{
                "duration": "PT3H5M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                "segments": [segment("AMS", "LIS", "2026-09-14T09:15:00", "2026-09-14T11:20:00", "1693", one_bag_each())]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(leg.itinerary, "AMS 09:15 14.09 ✈ LIS 11:20 14.09");
    }

    #[test]
    fn two_connections_draw_as_two_waits() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_et", "total_amount": "604.05", "total_currency": "EUR",
            "owner": {"name": "Ethiopian Airlines", "iata_code": "ET"},
            "slices": [{
                "duration": "PT19H5M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "CPT"},
                "segments": [
                    segment("AMS", "ADD", "2026-10-12T17:55:00", "2026-10-13T01:40:00", "4365", one_bag_each()),
                    segment("ADD", "JNB", "2026-10-13T08:10:00", "2026-10-13T11:00:00", "0713", one_bag_each()),
                    segment("JNB", "CPT", "2026-10-13T11:55:00", "2026-10-13T13:00:00", "0845", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(
            leg.itinerary,
            "AMS 17:55 12.10 ✈ ADD 6h 30m ✈ JNB 55m ✈ CPT 13:00 13.10"
        );
    }

    #[test]
    fn a_strip_shows_both_airports_when_the_connection_moves_you() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_split", "total_amount": "200.00", "total_currency": "EUR",
            "owner": {"name": "Whoever", "iata_code": "ZZ"},
            "slices": [{
                "duration": "PT12H",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "DUB"},
                "segments": [
                    segment("AMS", "LHR", "2026-09-15T08:00:00", "2026-09-15T09:00:00", "1", one_bag_each()),
                    segment("LGW", "DUB", "2026-09-15T15:00:00", "2026-09-15T16:30:00", "2", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert!(
            leg.itinerary.contains("LHR/LGW"),
            "a change of airport must be visible in the strip, got: {}",
            leg.itinerary
        );
    }

    #[test]
    fn a_strip_leaves_out_what_the_offer_did_not_state() {
        // Half a timestamp is worse than none: it invites the reader to
        // assume the missing half.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_vague", "total_amount": "200.00", "total_currency": "EUR",
            "owner": {"name": "Whoever", "iata_code": "ZZ"},
            "slices": [{
                "duration": "PT12H",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "HKG"},
                "segments": [
                    {"origin": {"iata_code": "AMS"}, "destination": {"iata_code": "PVG"},
                     "marketing_carrier": {"iata_code": "MU"}, "marketing_carrier_flight_number": "772",
                     "stops": [], "passengers": [{"baggages": []}]},
                    segment("PVG", "HKG", "2026-09-16T17:50:00", "2026-09-16T20:35:00", "0505", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(leg.itinerary, "AMS ✈ PVG ✈ HKG 20:35 16.09");
    }

    #[test]
    fn a_layover_that_runs_past_midnight_is_still_counted_correctly() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_night",
            "total_amount": "300.00",
            "total_currency": "EUR",
            "owner": {"name": "Ethiopian Airlines", "iata_code": "ET"},
            "slices": [{
                "duration": "PT19H5M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "CPT"},
                "segments": [
                    segment("AMS", "ADD", "2026-10-12T17:55:00", "2026-10-13T01:40:00", "4365", one_bag_each()),
                    segment("ADD", "CPT", "2026-10-13T08:10:00", "2026-10-13T13:00:00", "0845", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(leg.connections[0].layover.as_deref(), Some("6h 30m"));
    }

    #[test]
    fn a_connection_that_changes_airport_is_flagged_because_you_carry_your_bags() {
        // Landing at LHR and leaving from LGW is a coach ride, not a walk
        // between gates, and no airline protects it.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_split",
            "total_amount": "200.00",
            "total_currency": "EUR",
            "owner": {"name": "Whoever", "iata_code": "ZZ"},
            "slices": [{
                "duration": "PT12H",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "DUB"},
                "segments": [
                    segment("AMS", "LHR", "2026-09-15T08:00:00", "2026-09-15T09:00:00", "1", one_bag_each()),
                    segment("LGW", "DUB", "2026-09-15T15:00:00", "2026-09-15T16:30:00", "2", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        let stop = &leg.connections[0];
        assert!(stop.changes_airport);
        assert_eq!(stop.airport, "LHR");
        assert_eq!(stop.departs_from.as_deref(), Some("LGW"));
    }

    #[test]
    fn a_direct_flight_has_no_connections_at_all() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_direct", "total_amount": "184.30", "total_currency": "EUR",
            "owner": {"name": "KLM", "iata_code": "KL"},
            "slices": [{
                "duration": "PT3H5M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                "segments": [segment("AMS", "LIS", "2026-09-14T09:15:00", "2026-09-14T11:20:00", "1693", one_bag_each())]
            }]
        }]}})
        .to_string();
        assert!(parse_offers(&body).unwrap()[0].legs[0].connections.is_empty());
    }

    #[test]
    fn a_connection_with_no_stated_times_has_no_layover_rather_than_a_zero() {
        // Zero would read as "no wait at all", which is the opposite of
        // "nobody said".
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_vague", "total_amount": "200.00", "total_currency": "EUR",
            "owner": {"name": "Whoever", "iata_code": "ZZ"},
            "slices": [{
                "duration": "PT12H",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "HKG"},
                "segments": [
                    {"origin": {"iata_code": "AMS"}, "destination": {"iata_code": "PVG"},
                     "marketing_carrier": {"iata_code": "MU"}, "marketing_carrier_flight_number": "772",
                     "stops": [], "passengers": [{"baggages": []}]},
                    segment("PVG", "HKG", "2026-09-16T17:50:00", "2026-09-16T20:35:00", "0505", one_bag_each())
                ]
            }]
        }]}})
        .to_string();

        let leg = parse_offers(&body).unwrap().remove(0).legs.remove(0);
        assert_eq!(leg.connections[0].airport, "PVG");
        assert_eq!(leg.connections[0].layover, None);
        assert_eq!(leg.connections[0].layover_minutes, None);
    }

    #[test]
    fn a_return_trip_keeps_both_directions_and_sums_their_time() {
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_3",
            "total_amount": "312.00",
            "total_currency": "EUR",
            "owner": {"name": "KLM", "iata_code": "KL"},
            "slices": [
                {"duration": "PT3H5M", "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                 "segments": [segment("AMS", "LIS", "2026-09-14T09:15:00", "2026-09-14T11:20:00", "1693", one_bag_each())]},
                {"duration": "PT3H15M", "origin": {"iata_code": "LIS"}, "destination": {"iata_code": "AMS"},
                 "segments": [segment("LIS", "AMS", "2026-09-21T12:00:00", "2026-09-21T16:15:00", "1694", one_bag_each())]}
            ]
        }]}})
        .to_string();

        let f = parse_offers(&body).unwrap().remove(0);
        assert_eq!(f.legs.len(), 2);
        assert_eq!(f.legs[1].origin, "LIS");
        assert_eq!(f.total_minutes, Some(185 + 195));
    }

    #[test]
    fn a_bag_allowed_on_only_one_leg_is_not_an_allowance_for_the_trip() {
        // Presenting this as "1 checked bag included" is how someone pays at
        // the gate on the way home.
        let body = serde_json::json!({"data": {"offers": [{
            "id": "off_4",
            "total_amount": "150.00",
            "total_currency": "EUR",
            "owner": {"name": "KLM", "iata_code": "KL"},
            "slices": [{
                "duration": "PT6H40M",
                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                "segments": [
                    segment("AMS", "CDG", "2026-09-14T06:30:00", "2026-09-14T07:50:00", "1241", one_bag_each()),
                    segment("CDG", "LIS", "2026-09-14T10:15:00", "2026-09-14T12:10:00", "1024",
                            serde_json::json!([{"type": "carry_on", "quantity": 1}]))
                ]
            }]
        }]}})
        .to_string();

        let f = parse_offers(&body).unwrap().remove(0);
        assert_eq!(f.carry_on_bags, Some(1));
        assert_eq!(f.checked_bags, Some(0));
    }

    #[test]
    fn an_offer_with_no_usable_price_is_dropped_not_priced_at_zero() {
        // A zero would sort first and be reported as the cheapest flight.
        let body = serde_json::json!({"data": {"offers": [
            {"id": "bad", "total_amount": "", "total_currency": "EUR",
             "owner": {"name": "KLM"}, "slices": []},
            {"id": "worse", "total_currency": "EUR", "owner": {"name": "KLM"}, "slices": []},
            {"id": "good", "total_amount": "99.00", "total_currency": "EUR",
             "owner": {"name": "KLM"}, "slices": []}
        ]}})
        .to_string();

        let ids: Vec<String> = parse_offers(&body).unwrap().into_iter().map(|f| f.offer_id).collect();
        assert_eq!(ids, vec!["good"]);
    }

    #[test]
    fn a_body_that_is_not_an_offer_response_is_an_error_not_an_empty_list() {
        // Silence would read as "no flights on that route", which is a
        // different and much more misleading answer.
        assert!(matches!(
            parse_offers("<html>gateway timeout</html>"),
            Err(DuffelError::Decode { .. })
        ));
        // A well-formed response that genuinely found nothing is not an error.
        let empty = serde_json::json!({"data": {"offers": []}}).to_string();
        assert!(parse_offers(&empty).unwrap().is_empty());
    }

    fn query(origin: &str, destination: &str) -> FlightQuery {
        FlightQuery {
            origin: origin.to_string(),
            destination: destination.to_string(),
            departure_date: "2026-09-14".to_string(),
            return_date: None,
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        }
    }

    #[test]
    fn a_valid_one_way_query_passes_and_defaults_to_one_adult() {
        let q = query("AMS", "LIS");
        q.validate().unwrap();
        let body = q.body();
        assert_eq!(body["data"]["passengers"], serde_json::json!([{"type": "adult"}]));
        assert_eq!(
            body["data"]["slices"],
            serde_json::json!([
                {"origin": "AMS", "destination": "LIS", "departure_date": "2026-09-14"}
            ])
        );
        // Nothing the caller did not ask for.
        assert!(body["data"].get("cabin_class").is_none());
        assert!(body["data"].get("max_connections").is_none());
    }

    #[test]
    fn a_return_date_becomes_the_second_slice_in_the_other_direction() {
        let mut q = query("AMS", "LIS");
        q.return_date = Some("2026-09-21".to_string());
        q.adults = Some(2);
        q.cabin_class = Some("business".to_string());
        q.max_connections = Some(0);
        q.validate().unwrap();

        let body = q.body();
        assert_eq!(
            body["data"]["slices"],
            serde_json::json!([
                {"origin": "AMS", "destination": "LIS", "departure_date": "2026-09-14"},
                {"origin": "LIS", "destination": "AMS", "departure_date": "2026-09-21"}
            ])
        );
        assert_eq!(body["data"]["passengers"].as_array().unwrap().len(), 2);
        assert_eq!(body["data"]["cabin_class"], "business");
        assert_eq!(body["data"]["max_connections"], 0);
    }

    #[test]
    fn place_names_are_rejected_because_duffel_wants_iata_codes() {
        // "Amsterdam" would come back as a Duffel 422 and a wasted search
        // fee; the model can fix this if it is told what is wrong.
        let err = query("Amsterdam", "LIS").validate().unwrap_err().to_string();
        assert!(err.contains("IATA"), "got: {err}");
        assert!(err.contains("Amsterdam"), "the bad value must be named, got: {err}");

        assert!(query("AM", "LIS").validate().is_err());
        assert!(query("AMS", "L1S").validate().is_err());
        // Case is the model's business, not the traveller's.
        query("ams", "lis").validate().unwrap();
        assert_eq!(query("ams", "lis").body()["data"]["slices"][0]["origin"], "AMS");
    }

    #[test]
    fn a_flight_from_a_place_to_itself_is_refused() {
        assert!(query("AMS", "ams").validate().is_err());
    }

    #[test]
    fn dates_must_be_calendar_dates_and_the_return_cannot_precede_departure() {
        let mut q = query("AMS", "LIS");
        q.departure_date = "14 September".to_string();
        let err = q.validate().unwrap_err().to_string();
        assert!(err.contains("YYYY-MM-DD"), "got: {err}");

        let mut q = query("AMS", "LIS");
        q.return_date = Some("2026-09-01".to_string());
        assert!(q.validate().is_err());
        // Same day out and back is a real trip.
        q.return_date = Some("2026-09-14".to_string());
        q.validate().unwrap();
    }

    #[test]
    fn passenger_counts_outside_what_a_household_books_are_refused() {
        let mut q = query("AMS", "LIS");
        q.adults = Some(0);
        assert!(q.validate().is_err());
        q.adults = Some(MAX_ADULTS + 1);
        assert!(q.validate().is_err());
        q.adults = Some(MAX_ADULTS);
        q.validate().unwrap();
    }

    #[test]
    fn an_invented_cabin_class_is_refused_with_the_real_ones_listed() {
        let mut q = query("AMS", "LIS");
        q.cabin_class = Some("premium".to_string());
        let err = q.validate().unwrap_err().to_string();
        assert!(err.contains("premium_economy"), "got: {err}");
        for real in ["economy", "premium_economy", "business", "first"] {
            let mut q = query("AMS", "LIS");
            q.cabin_class = Some(real.to_string());
            q.validate().unwrap();
        }
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> DuffelClient {
        DuffelClient::new(reqwest::Client::new(), "duffel_test_key".to_string(), server.uri())
    }

    fn query() -> FlightQuery {
        FlightQuery {
            origin: "AMS".to_string(),
            destination: "LIS".to_string(),
            departure_date: "2026-09-14".to_string(),
            return_date: None,
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        }
    }

    #[tokio::test]
    async fn searches_offer_requests_and_returns_the_offers_inline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            // Without this the offers come back empty and every search needs
            // a second round trip.
            .and(query_param("return_offers", "true"))
            .and(header("Authorization", "Bearer duffel_test_key"))
            .and(header("Duffel-Version", DUFFEL_VERSION))
            .and(body_json(json!({"data": {
                "slices": [{"origin": "AMS", "destination": "LIS", "departure_date": "2026-09-14"}],
                "passengers": [{"type": "adult"}]
            }})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {
                "id": "orq_1",
                "offers": [{
                    "id": "off_1", "total_amount": "184.30", "total_currency": "EUR",
                    "owner": {"name": "KLM", "iata_code": "KL"},
                    "slices": [{
                        "duration": "PT3H5M",
                        "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                        "segments": [{
                            "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "LIS"},
                            "departing_at": "2026-09-14T09:15:00",
                            "arriving_at": "2026-09-14T11:20:00",
                            "marketing_carrier": {"name": "KLM", "iata_code": "KL"},
                            "marketing_carrier_flight_number": "1693",
                            "stops": [],
                            "passengers": [{"baggages": [{"type": "checked", "quantity": 1}]}]
                        }]
                    }]
                }]
            }})))
            .expect(1)
            .mount(&server)
            .await;

        let flights = client(&server).search(&query()).await.unwrap();
        assert_eq!(flights.len(), 1);
        assert_eq!(flights[0].price, 184.30);
        assert_eq!(flights[0].legs[0].flights, vec!["KL1693"]);
    }

    #[tokio::test]
    async fn a_rejected_search_surfaces_its_status_rather_than_looking_empty() {
        // 422 is what a bad IATA code or a date in the past comes back as.
        // Reporting it as "no flights found" would be a lie.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_string(
                r#"{"errors":[{"title":"Invalid origin"}]}"#,
            ))
            .mount(&server)
            .await;

        let err = client(&server).search(&query()).await.unwrap_err();
        assert!(matches!(err, DuffelError::Api { status: 422, .. }), "got: {err}");
        assert!(err.to_string().contains("Invalid origin"), "got: {err}");
    }

    #[tokio::test]
    async fn an_invalid_query_never_reaches_the_network() {
        // Duffel bills per search; a call the model got wrong should cost
        // nothing.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let mut bad = query();
        bad.origin = "Amsterdam".to_string();
        assert!(client(&server).search(&bad).await.is_err());
    }

    /// A tool wired to a temp-file store, so the logging can be asserted.
    fn tool(server: &MockServer) -> (crate::tools::duffel::FlightSearchTool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("t.duckdb")).unwrap();
        (
            FlightSearchTool {
                client: client(server),
                store,
                user_id: 7,
                budget: std::sync::Arc::new(crate::tools::budget::FlightBudget::default()),
            },
            dir,
        )
    }

    fn searches_logged(tool: &crate::tools::duffel::FlightSearchTool) -> i64 {
        tool.store
            .flight_searches_all("2000-01-01 00:00:00")
            .unwrap()
            .get(&7)
            .copied()
            .unwrap_or(0)
    }

    /// Matches a search whose outbound date is `date`.
    fn body_json_departure(date: &'static str) -> impl wiremock::Match {
        move |req: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok()
                .and_then(|b| b["data"]["slices"][0]["departure_date"].as_str().map(String::from))
                .is_some_and(|d| d == date)
        }
    }

    /// The dates every request in `server`'s log asked for, in the order the
    /// bodies were received.
    async fn dates_asked_for(server: &MockServer) -> Vec<(String, Option<String>)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                let slices = body["data"]["slices"].as_array().unwrap().clone();
                (
                    slices[0]["departure_date"].as_str().unwrap().to_string(),
                    slices.get(1).map(|s| s["departure_date"].as_str().unwrap().to_string()),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_flexible_search_covers_the_whole_window_one_day_at_a_time() {
        // Duffel has no calendar endpoint — a range is N searches. Seven is
        // what +/-3 days costs, which is why the per-request cap is eight.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(7)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let mut flexible = query();
        flexible.flex_days = Some(3);
        let out = rig::tool::Tool::call(&tool, flexible).await.unwrap();

        let mut asked: Vec<String> = dates_asked_for(&server).await.into_iter().map(|(d, _)| d).collect();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                "2026-09-11", "2026-09-12", "2026-09-13", "2026-09-14",
                "2026-09-15", "2026-09-16", "2026-09-17"
            ]
        );
        // Reported back in date order, whatever order they came home in.
        assert_eq!(
            out.by_date.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            vec![
                "2026-09-11", "2026-09-12", "2026-09-13", "2026-09-14",
                "2026-09-15", "2026-09-16", "2026-09-17"
            ]
        );
        assert!(out.route.contains("±3"), "the window belongs in the route: {}", out.route);
    }

    #[tokio::test]
    async fn a_flexible_return_trip_shifts_both_dates_and_keeps_its_length() {
        // Someone asking for a week away wants the same week moved, not a
        // ten-day trip — and flexing both ends independently would be 49
        // searches instead of 7.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let mut flexible = query();
        flexible.return_date = Some("2026-09-21".to_string());
        flexible.flex_days = Some(1);
        rig::tool::Tool::call(&tool, flexible).await.unwrap();

        let mut pairs = dates_asked_for(&server).await;
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("2026-09-13".to_string(), Some("2026-09-20".to_string())),
                ("2026-09-14".to_string(), Some("2026-09-21".to_string())),
                ("2026-09-15".to_string(), Some("2026-09-22".to_string())),
            ]
        );
    }

    #[tokio::test]
    async fn the_cheapest_is_the_cheapest_of_any_day_in_the_window() {
        let server = MockServer::start().await;
        let offer = |id: &str, amount: &str| {
            json!({"data": {"offers": [{
                "id": id, "total_amount": amount, "total_currency": "EUR",
                "owner": {"name": "KLM"}, "slices": []
            }]}})
        };
        // The requested day is dear; the day before is not.
        Mock::given(method("POST"))
            .and(body_json_departure("2026-09-14"))
            .respond_with(ResponseTemplate::new(201).set_body_json(offer("dear", "300.00")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_json_departure("2026-09-13"))
            .respond_with(ResponseTemplate::new(201).set_body_json(offer("cheap", "180.00")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let mut flexible = query();
        flexible.flex_days = Some(1);
        let out = rig::tool::Tool::call(&tool, flexible).await.unwrap();

        assert_eq!(out.cheapest.unwrap().offer_id, "cheap");
        let day = |d: &str| out.by_date.iter().find(|x| x.date == d).unwrap().cheapest;
        assert_eq!(day("2026-09-13"), Some(180.00));
        assert_eq!(day("2026-09-14"), Some(300.00));
        assert_eq!(day("2026-09-15"), None, "a day with nothing says so");
    }

    #[tokio::test]
    async fn a_window_wider_than_the_allowance_keeps_the_days_nearest_the_one_asked_for() {
        // Losing the far edges of the window beats losing the date the
        // traveller actually named.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        // Spend all but three of the allowance on unrelated routes. Real
        // codes: a made-up one with a digit is rejected before it costs
        // anything, which is the whole point of validating first.
        let elsewhere = ["OPO", "MAD", "BCN", "FCO", "ATH", "VIE", "PRG"];
        for code in &elsewhere[..crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST - 3] {
            let mut q = query();
            q.destination = code.to_string();
            rig::tool::Tool::call(&tool, q).await.unwrap();
        }
        assert_eq!(tool.budget.spent(), crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST - 3);

        let mut flexible = query();
        flexible.flex_days = Some(3);
        let out = rig::tool::Tool::call(&tool, flexible).await.unwrap();

        let covered: Vec<&str> = out.by_date.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(covered, vec!["2026-09-13", "2026-09-14", "2026-09-15"]);
        assert!(
            out.notes.iter().any(|n| n.contains("2026-09-11") || n.contains("could not")),
            "the days it could not check must be named, got: {:?}",
            out.notes
        );
    }

    #[tokio::test]
    async fn a_window_wider_than_three_days_is_refused_before_anything_is_bought() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let mut wide = query();
        wide.flex_days = Some(15);
        let err = rig::tool::Tool::call(&tool, wide).await.unwrap_err().to_string();
        assert!(err.contains('3'), "the limit should be stated, got: {err}");
    }

    #[tokio::test]
    async fn without_flex_days_nothing_changes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(1)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let out = rig::tool::Tool::call(&tool, query()).await.unwrap();
        assert!(out.by_date.is_empty(), "a single-date search has no calendar");
        assert_eq!(out.route, "AMS-LIS 2026-09-14");
    }

    #[tokio::test]
    async fn the_same_search_twice_in_one_request_only_reaches_duffel_once() {
        // The model re-asks a route while it works through a comparison.
        // Seconds apart the offers are the same offers, and the second ask
        // is a second charge for an answer already in hand.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": [
                {"id": "off_1", "total_amount": "184.30", "total_currency": "EUR",
                 "owner": {"name": "KLM"}, "slices": []}
            ]}})))
            .expect(1)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let first = rig::tool::Tool::call(&tool, query()).await.unwrap();
        let second = rig::tool::Tool::call(&tool, query()).await.unwrap();
        assert_eq!(first, second, "the repeat must return the same answer");
        // Billing is per request sent, so the repeat is not counted either.
        assert_eq!(searches_logged(&tool), 1);
        assert_eq!(tool.budget.spent(), 1);
    }

    #[tokio::test]
    async fn the_repeat_is_recognised_however_the_model_spells_it() {
        // "ams" today and "AMS" a turn later are one route, and adults: 1
        // is what adults: null already meant.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(1)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        rig::tool::Tool::call(&tool, query()).await.unwrap();

        let mut same = query();
        same.origin = "ams".to_string();
        same.destination = " LIS ".to_string();
        same.adults = Some(1);
        rig::tool::Tool::call(&tool, same).await.unwrap();
        assert_eq!(tool.budget.spent(), 1);
    }

    #[tokio::test]
    async fn a_different_date_is_a_different_search() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(2)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        rig::tool::Tool::call(&tool, query()).await.unwrap();
        let mut later = query();
        later.departure_date = "2026-09-15".to_string();
        rig::tool::Tool::call(&tool, later).await.unwrap();
        assert_eq!(tool.budget.spent(), 2);
    }

    #[tokio::test]
    async fn a_request_that_hits_the_cap_stops_spending_and_says_why() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST as u64)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        for day in 1..=crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST {
            let mut q = query();
            q.departure_date = format!("2026-09-{day:02}");
            rig::tool::Tool::call(&tool, q).await.unwrap();
        }

        let mut over = query();
        over.departure_date = "2026-10-01".to_string();
        let err = rig::tool::Tool::call(&tool, over).await.unwrap_err().to_string();
        assert!(
            err.contains("answer") || err.contains("already"),
            "the message should tell the model to answer with what it has, got: {err}"
        );
        // Nothing further was sent, so nothing further was billed.
        assert_eq!(searches_logged(&tool), crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST as i64);
    }

    #[tokio::test]
    async fn a_repeat_still_answers_after_the_cap_is_reached() {
        // The allowance limits what is bought, not what is already known —
        // refusing a route we hold results for would throw away money
        // already spent.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .expect(crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST as u64)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        for day in 1..=crate::tools::budget::FLIGHT_SEARCHES_PER_REQUEST {
            let mut q = query();
            q.departure_date = format!("2026-09-{day:02}");
            rig::tool::Tool::call(&tool, q).await.unwrap();
        }

        let mut repeat = query();
        repeat.departure_date = "2026-09-01".to_string();
        assert!(rig::tool::Tool::call(&tool, repeat).await.is_ok());
    }

    #[tokio::test]
    async fn every_search_that_reaches_duffel_is_counted_against_the_user() {
        // Duffel bills per search and Scout makes no bookings, so the
        // allowance is zero and each of these is real money. /stat cannot
        // report what was never recorded.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        assert_eq!(searches_logged(&tool), 0);
        // Two genuinely different journeys: a repeat is served from the memo
        // and deliberately not counted (see the dedupe tests above).
        rig::tool::Tool::call(&tool, query()).await.unwrap();
        let mut other = query();
        other.destination = "OPO".to_string();
        rig::tool::Tool::call(&tool, other).await.unwrap();
        assert_eq!(searches_logged(&tool), 2);
    }

    #[tokio::test]
    async fn a_query_rejected_before_the_request_is_not_counted() {
        // Nothing left the machine, so nothing was charged. Counting it
        // would make /stat overstate the bill.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        let mut bad = query();
        bad.origin = "Amsterdam".to_string();
        assert!(rig::tool::Tool::call(&tool, bad).await.is_err());
        assert_eq!(searches_logged(&tool), 0);
    }

    #[tokio::test]
    async fn a_search_duffel_rejected_is_still_counted() {
        // The request was made, so assume it was billable. Undercounting
        // spend is the worse error of the two.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_string("{}"))
            .mount(&server)
            .await;

        let (tool, _dir) = tool(&server);
        assert!(rig::tool::Tool::call(&tool, query()).await.is_err());
        assert_eq!(searches_logged(&tool), 1);
    }

    #[tokio::test]
    async fn a_route_with_no_flights_is_an_empty_list_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": []}})))
            .mount(&server)
            .await;
        assert!(client(&server).search(&query()).await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    fn query() -> FlightQuery {
        FlightQuery {
            origin: "ams".to_string(),
            destination: "LIS".to_string(),
            departure_date: "2026-09-14".to_string(),
            return_date: None,
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        }
    }

    fn flight(price: f64) -> Flight {
        Flight {
            offer_id: "off_1".to_string(),
            airline: "KLM".to_string(),
            price,
            currency: "EUR".to_string(),
            legs: Vec::new(),
            total_minutes: Some(185),
            total_duration: Some("3h 5m".to_string()),
            checked_bags: Some(1),
            carry_on_bags: Some(1),
            expires_at: None,
        }
    }

    #[test]
    fn finding_nothing_is_an_answer_rather_than_a_failure() {
        // "No flights on that day" is a real finding. Raised as an error it
        // becomes "the flight search failed", which sends the user looking
        // for a problem that is not there.
        let out = FlightSearchOutput::new(&query(), None);
        assert_eq!(out.found, 0);
        assert!(out.cheapest.is_none());
        assert!(out.rows.is_empty());
        assert!(
            out.notes.iter().any(|n| n.contains("no flights")),
            "got: {:?}",
            out.notes
        );
    }

    #[test]
    fn the_route_is_echoed_back_normalised_so_a_reply_cannot_invent_one() {
        let out = FlightSearchOutput::new(&query(), rank(vec![flight(184.30)]));
        assert_eq!(out.route, "AMS-LIS 2026-09-14");
        assert_eq!(out.found, 1);
        assert_eq!(out.currency.as_deref(), Some("EUR"));
        assert_eq!(out.cheapest.unwrap().price, 184.30);
    }

    #[test]
    fn a_return_trip_says_so_in_the_route() {
        let mut q = query();
        q.return_date = Some("2026-09-21".to_string());
        let out = FlightSearchOutput::new(&q, None);
        assert_eq!(out.route, "AMS-LIS 2026-09-14, back 2026-09-21");
    }

    #[test]
    fn the_count_is_before_capping_so_the_reply_can_say_what_it_left_out() {
        let many: Vec<Flight> = (0..8).map(|i| flight(100.0 + i as f64)).collect();
        let out = FlightSearchOutput::new(&query(), rank(many));
        assert_eq!(out.found, 8);
        assert_eq!(out.rows.len(), ROW_CAP);
    }
}

#[cfg(test)]
mod links_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer, markup: f64) -> DuffelClient {
        DuffelClient::new(reqwest::Client::new(), "k".to_string(), server.uri()).with_markup(markup)
    }

    #[tokio::test]
    async fn a_booking_session_carries_the_markup_and_the_way_back_to_the_chat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/links/sessions"))
            .and(header("Duffel-Version", DUFFEL_VERSION))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": {"url": "https://links.duffel.com/s/abc123"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = client(&server, 0.03)
            .booking_link(42, "https://t.me/scoutbot")
            .await
            .unwrap();
        assert_eq!(url, "https://links.duffel.com/s/abc123");

        let body: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        let data = &body["data"];
        // The markup is the whole point: it is what a booking earns.
        assert_eq!(data["markup_rate"], "0.03");
        // Reference ties an order back to whoever asked for it.
        assert_eq!(data["reference"], "42");
        for key in ["success_url", "failure_url", "abandonment_url"] {
            assert_eq!(data[key], "https://t.me/scoutbot", "{key} should return to the chat");
        }
        // Flights only — Scout knows nothing about hotels.
        assert_eq!(data["flights"]["enabled"], true);
        assert_eq!(data["stays"]["enabled"], false);
    }

    #[tokio::test]
    async fn without_a_configured_markup_none_is_sent() {
        // Absent is not the same as zero: a markup field the operator never
        // set should not appear in the request at all.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/links/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": {"url": "https://links.duffel.com/s/x"}
            })))
            .mount(&server)
            .await;

        client(&server, 0.0).booking_link(1, "https://t.me/b").await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert!(body["data"].get("markup_rate").is_none());
    }

    #[tokio::test]
    async fn a_fee_is_disclosed_in_the_results_not_only_in_the_preamble() {
        // Measured in production: with a 3% fee configured, the reply quoted
        // "EUR 669.40" and never mentioned the fee. The preamble said to
        // disclose it, but that instruction sits in the longest bullet in
        // the prompt. Notes attached to the results are the channel the
        // model actually echoes — the baggage warning proves it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": [
                {"id": "a", "total_amount": "100.00", "total_currency": "EUR",
                 "owner": {"name": "KLM"}, "slices": []}
            ]}})))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let tool = FlightSearchTool {
            client: client(&server, 0.03),
            store: crate::store::Store::open(dir.path().join("t.duckdb")).unwrap(),
            user_id: 1,
            budget: std::sync::Arc::new(crate::tools::budget::FlightBudget::default()),
        };
        let out = rig::tool::Tool::call(&tool, query()).await.unwrap();
        assert!(
            out.notes.iter().any(|n| n.contains("3%") && n.contains("booking fee")),
            "the fee must travel with the prices it is baked into, got: {:?}",
            out.notes
        );

        // No fee, no note: nothing to disclose and nothing to explain away.
        let plain = FlightSearchTool {
            client: client(&server, 0.0),
            store: crate::store::Store::open(dir.path().join("u.duckdb")).unwrap(),
            user_id: 1,
            budget: std::sync::Arc::new(crate::tools::budget::FlightBudget::default()),
        };
        let out = rig::tool::Tool::call(&plain, query()).await.unwrap();
        assert!(!out.notes.iter().any(|n| n.contains("booking fee")), "got: {:?}", out.notes);
    }

    #[tokio::test]
    async fn a_refused_session_surfaces_its_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("not enabled"))
            .mount(&server)
            .await;
        let err = client(&server, 0.0).booking_link(1, "https://t.me/b").await.unwrap_err();
        assert!(matches!(err, DuffelError::Api { status: 403, .. }), "got: {err}");
    }

    #[tokio::test]
    async fn quoted_prices_already_include_the_markup_the_traveller_will_pay() {
        // Otherwise Scout says 600 and the checkout says 618, which is
        // exactly the kind of gap this whole project exists to close.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"offers": [
                {"id": "a", "total_amount": "100.00", "total_currency": "EUR",
                 "owner": {"name": "KLM"}, "slices": []},
                {"id": "b", "total_amount": "200.00", "total_currency": "EUR",
                 "owner": {"name": "TAP"}, "slices": []}
            ]}})))
            .mount(&server)
            .await;

        let flights = client(&server, 0.03).search(&query()).await.unwrap();
        assert_eq!(flights[0].price, 103.00);
        assert_eq!(flights[1].price, 206.00);
        // A uniform rate cannot reorder anything, so the ranking is
        // untouched by whatever the operator charges.
        let ranked = rank(flights).unwrap();
        assert_eq!(ranked.cheapest.offer_id, "a");
    }

    fn query() -> FlightQuery {
        FlightQuery {
            origin: "AMS".to_string(),
            destination: "LIS".to_string(),
            departure_date: "2026-09-14".to_string(),
            return_date: None,
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: None,
        }
    }
}

#[cfg(test)]
mod live {
    //! Ignored by default: needs `DUFFEL_API_KEY` and network.
    //!
    //! The mocked tests prove the parsing against a payload built from
    //! Duffel's own client types. This proves the types were right.
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn searches_a_real_route() {
        let key = std::env::var("DUFFEL_API_KEY").expect("DUFFEL_API_KEY");
        let client = DuffelClient::new(reqwest::Client::new(), key, DUFFEL_API_BASE.to_string());
        let query = FlightQuery {
            origin: std::env::var("SCOUT_PROBE_ORIGIN").unwrap_or_else(|_| "LHR".into()),
            destination: std::env::var("SCOUT_PROBE_DESTINATION").unwrap_or_else(|_| "JFK".into()),
            departure_date: std::env::var("SCOUT_PROBE_DATE").unwrap_or_else(|_| "2026-09-14".into()),
            return_date: std::env::var("SCOUT_PROBE_RETURN").ok(),
            adults: None,
            cabin_class: None,
            max_connections: None,
            flex_days: std::env::var("SCOUT_PROBE_FLEX").ok().and_then(|f| f.parse().ok()),
        };

        // A flexible window is a fan-out over days, so it is exercised
        // through the tool rather than the single-date client call.
        if query.flex_days.is_some() {
            let dir = tempfile::tempdir().unwrap();
            let tool = FlightSearchTool {
                client: client.clone(),
                store: crate::store::Store::open(dir.path().join("probe.duckdb")).unwrap(),
                user_id: 1,
                budget: std::sync::Arc::new(crate::tools::budget::FlightBudget::default()),
            };
            let out = rig::tool::Tool::call(&tool, query).await.unwrap();
            println!("LIVE route={} bought={}", out.route, tool.budget.spent());
            for day in &out.by_date {
                println!(
                    "  {} {:>8} {} ({} offers) {}",
                    day.date,
                    day.cheapest.map(|p| format!("{p:.2}")).unwrap_or_else(|| "-".into()),
                    day.currency.clone().unwrap_or_default(),
                    day.found,
                    day.airline.clone().unwrap_or_default(),
                );
            }
            assert_eq!(out.by_date.len(), 7, "a ±3 window is seven days");
            assert!(out.by_date.iter().any(|d| d.cheapest.is_some()));
            return;
        }

        let flights = client.search(&query).await.unwrap();
        println!("LIVE offers={}", flights.len());
        for f in flights.iter().take(3) {
            println!(
                "  {} {} {:.2} {} total={:?}min bags(checked={:?} carry_on={:?}) expires={:?}",
                f.offer_id, f.airline, f.price, f.currency, f.total_minutes,
                f.checked_bags, f.carry_on_bags, f.expires_at
            );
            for leg in &f.legs {
                println!(
                    "    {} -> {} dep {} arr {} {:?}min stops={} {:?}",
                    leg.origin, leg.destination, leg.departing_at_local, leg.arriving_at_local,
                    leg.duration_minutes, leg.stops, leg.flights
                );
            }
        }
        let ranked = FlightSearchOutput::new(&query, rank(flights));
        println!("LIVE ranked: {}", serde_json::to_string_pretty(&ranked).unwrap());

        // Every field the reply quotes must have survived the round trip.
        assert!(ranked.found > 0, "test mode should always have inventory");
        let cheapest = ranked.cheapest.expect("a cheapest offer");
        assert!(cheapest.price > 0.0);
        assert!(!cheapest.currency.is_empty());
        assert!(!cheapest.airline.is_empty(), "owner.name did not parse");
        assert!(!cheapest.legs.is_empty(), "slices did not parse");
        let leg = &cheapest.legs[0];
        assert_eq!(leg.origin, query.origin, "segment origin did not parse");
        assert!(!leg.departing_at_local.is_empty(), "departing_at did not parse");
        assert!(leg.duration_minutes.is_some(), "slice duration did not parse");
        assert!(
            leg.flights.iter().all(|f| f.len() > 2),
            "carrier + flight number did not join: {:?}",
            leg.flights
        );
    }
}

/// The two conversions everything above is built on: Duffel states both
/// durations and money as strings, and getting either wrong is silent.
#[cfg(test)]
mod scalar_tests {
    use super::*;

    #[test]
    fn iso_durations_become_whole_minutes() {
        assert_eq!(duration_minutes("PT2H30M"), Some(150));
        assert_eq!(duration_minutes("PT55M"), Some(55));
        assert_eq!(duration_minutes("PT3H"), Some(180));
        // Overnight legs carry a day component.
        assert_eq!(duration_minutes("P1DT2H5M"), Some(1565));
        // Seconds exist in the grammar and round away.
        assert_eq!(duration_minutes("PT1H0M30S"), Some(60));
    }

    #[test]
    fn a_duration_that_is_not_one_is_none_rather_than_zero() {
        // Zero would rank as the fastest flight in the set.
        assert_eq!(duration_minutes(""), None);
        assert_eq!(duration_minutes("2h30m"), None);
        assert_eq!(duration_minutes("PT"), None);
        assert_eq!(duration_minutes("tomorrow"), None);
    }

    #[test]
    fn amounts_arrive_as_strings_and_parse_numerically() {
        assert_eq!(amount("62.19"), Some(62.19));
        assert_eq!(amount("1000.00"), Some(1000.0));
        assert_eq!(amount("184"), Some(184.0));
        assert_eq!(amount(""), None);
        assert_eq!(amount("free"), None);
    }

    #[test]
    fn string_amounts_must_not_be_compared_as_text() {
        // The whole reason amounts are parsed: "1000.00" < "62.19" as text.
        assert!("1000.00" < "62.19");
        assert!(amount("1000.00") > amount("62.19"));
    }
}
