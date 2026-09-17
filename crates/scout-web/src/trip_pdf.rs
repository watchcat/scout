//! A printable snapshot of a durable trip.
//!
//! The browser UI is interactive and intentionally compact. This document is
//! the hand-off copy: every route, option, warning and saved-fare caveat in a
//! self-contained HTML page that Chromium can print without network access.

use chrono::{NaiveDate, NaiveDateTime, Utc};
use scout_core::trips::{Plan, Readiness, TripCandidate, TripItem};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

const PDF_TIMEOUT: Duration = Duration::from_secs(30);
/// How often to look for a finished PDF.
///
/// This is pure added latency on every export, so it wants to be short: a
/// local render has its PDF on disk about 1.3s in, and 25ms is under a
/// twentieth of that while costing ~50 `stat` calls for the whole render.
/// Polling tighter buys nothing anybody can perceive; polling at, say, 250ms
/// would put a fifth of a second on every download for no saving worth having.
const PDF_POLL: Duration = Duration::from_millis(25);
const MAX_CONCURRENT_PDFS: usize = 2;
const MAX_PDF_BYTES: u64 = 10 * 1024 * 1024;
/// Below this a `%PDF-` header is a fragment, not a page anybody can read.
const MIN_PDF_BYTES: u64 = 1024;
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_ITEMS: usize = 64;
const MAX_CANDIDATES_PER_ITEM: usize = 64;
/// Beside the candidate bound, and for the same reason: what this file
/// prints is bounded by this file. One mail could carry a hundred files.
const MAX_ATTACHMENTS_PER_ITEM: usize = 32;
const MAX_FIELD_BYTES: usize = 64 * 1024;
/// The largest calendar gap that still counts as a join: two days here, so
/// three or more is a stay. Not one day, because `days_apart` is coarse
/// and one of the two ways it is coarse goes quiet — see there. The page
/// holds the same number, and `connection_gaps.json` fails on both sides
/// if either moves.
const DAYS_APART: i64 = 2;

static PDF_SLOTS: OnceLock<Semaphore> = OnceLock::new();

#[derive(Debug)]
pub enum Error {
    Unavailable,
    Busy,
    Timeout,
    Io(std::io::Error),
    Chrome(String),
    InputTooLarge,
    InvalidOutput,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "Chromium is not available"),
            Self::Busy => write!(f, "the PDF renderer is busy"),
            Self::Timeout => write!(f, "PDF rendering timed out"),
            Self::Io(e) => write!(f, "PDF rendering I/O failed: {e}"),
            Self::Chrome(e) => write!(f, "Chromium could not render the PDF: {e}"),
            Self::InputTooLarge => write!(f, "trip is too large to render as a PDF"),
            Self::InvalidOutput => write!(f, "Chromium returned something other than a PDF"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
pub fn available() -> bool {
    chrome().is_some()
}

/// An ASCII filename safe to place in a quoted Content-Disposition value.
pub fn filename(name: &str) -> String {
    let mut stem = String::new();
    let mut separator = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            stem.push(c.to_ascii_lowercase());
            separator = false;
        } else if !separator && !stem.is_empty() {
            stem.push('-');
            separator = true;
        }
        if stem.len() >= 60 {
            break;
        }
    }
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        stem.push_str("trip");
    }
    format!("{stem}-itinerary.pdf")
}

pub async fn render(plan: &Plan) -> Result<Vec<u8>, Error> {
    validate_plan(plan)?;
    let chrome = chrome().ok_or(Error::Unavailable)?;
    let _slot = PDF_SLOTS
        .get_or_init(|| Semaphore::new(MAX_CONCURRENT_PDFS))
        .try_acquire()
        .map_err(|_| Error::Busy)?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("trip.html");
    let output = dir.path().join("trip.pdf");
    let document = html(plan);
    if document.len() > MAX_HTML_BYTES {
        return Err(Error::InputTooLarge);
    }
    tokio::fs::write(&source, document).await?;

    let log = dir.path().join("chrome.log");

    let mut command = Command::new(chrome);
    command
        .arg("--headless=new")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-gpu")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--disable-javascript")
        .arg("--no-first-run")
        .arg("--no-pdf-header-footer")
        .arg("--print-to-pdf-no-header")
        .arg(format!(
            "--user-data-dir={}",
            dir.path().join("profile").display()
        ))
        .arg(format!("--print-to-pdf={}", output.display()))
        .arg(format!("file://{}", source.display()))
        .stdout(std::process::Stdio::null())
        // Chromium is chatty even on a clean run, and a pipe nobody drains
        // fills its buffer and blocks the render before the PDF is written.
        // A file in the temp dir cannot fill, and still keeps the stderr to
        // quote when the process dies with something worth repeating.
        .stderr(std::process::Stdio::from(std::fs::File::create(&log)?))
        .kill_on_drop(true);

    let mut child = command.spawn()?;
    let result = collect(&mut child, &output, &log).await;
    // Chromium has to be killed on every path, not just the happy one: by the
    // time the PDF is readable the process is usually still running, and it is
    // the wait for it to leave that used to burn the whole timeout.
    // `kill_on_drop` covers a panic; this covers the returns.
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}

/// Waits for a finished PDF rather than for Chromium to exit.
///
/// Chromium writes the whole document and then, with `--user-data-dir` on
/// macOS, simply stays up: measured here, the PDF is complete and correct
/// 1.3s in while the process is still alive 25s later. Waiting on the process
/// therefore meant waiting out `PDF_TIMEOUT` on work that had already
/// finished. Waiting on the artefact instead makes the render as quick as the
/// render, and makes it behave the same way on a laptop as in the container.
/// Do not "simplify" this back to `Command::output` — that waits for the exit
/// *and* for the stdio pipes to close, which is the bug.
async fn collect(child: &mut Child, output: &Path, log: &Path) -> Result<Vec<u8>, Error> {
    let deadline = tokio::time::Instant::now() + PDF_TIMEOUT;
    let mut settled = None;
    loop {
        let size = tokio::fs::metadata(output).await.map(|meta| meta.len()).unwrap_or(0);
        if size > MAX_PDF_BYTES {
            return Err(Error::InvalidOutput);
        }
        // `complete` is what decides the file is whole; this only avoids
        // re-reading megabytes on every tick while Chromium is still writing.
        if size >= MIN_PDF_BYTES && settled == Some(size) {
            let bytes = tokio::fs::read(output).await?;
            if complete(&bytes) {
                return Ok(bytes);
            }
        }
        settled = Some(size);

        if let Some(status) = child.try_wait()? {
            // A Chromium that died before writing has its reason on stderr,
            // and "no usable sandbox" is a better answer than a timeout.
            if !status.success() {
                let reason = tokio::fs::read(log).await.unwrap_or_default();
                return Err(Error::Chrome(
                    String::from_utf8_lossy(&reason)
                        .trim()
                        .chars()
                        .take(500)
                        .collect(),
                ));
            }
            // It exited cleanly, so whatever is on disk is all there will be.
            let bytes = tokio::fs::read(output).await.unwrap_or_default();
            return if complete(&bytes) { Ok(bytes) } else { Err(Error::InvalidOutput) };
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        tokio::time::sleep(PDF_POLL).await;
    }
}

/// Whether `bytes` is a PDF that has been written all the way to its end.
///
/// Size and mtime both lie while Chromium is mid-write, and a truncated PDF
/// still opens with `%PDF-` — hand one to a traveller and they get an
/// unreadable download. A PDF says so itself: `%%EOF` is the last thing in
/// the file, so a document carrying the trailer at its end is a whole one.
fn complete(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"%PDF-") || (bytes.len() as u64) < MIN_PDF_BYTES {
        return false;
    }
    // Chromium writes `%%EOF\n`, and the spec lets the trailer be followed by
    // an end-of-line, so ignore trailing whitespace before looking for it.
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(0, |i| i + 1);
    bytes[..end].ends_with(b"%%EOF")
}

fn validate_plan(plan: &Plan) -> Result<(), Error> {
    if plan.trip.items.len() > MAX_ITEMS
        || plan.trip.name.len() > MAX_FIELD_BYTES
        || plan.notes.iter().any(|note| note.len() > MAX_FIELD_BYTES)
    {
        return Err(Error::InputTooLarge);
    }
    let too_long = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
    };
    for item in &plan.trip.items {
        if item.candidates.len() > MAX_CANDIDATES_PER_ITEM
            || item.attachments.len() > MAX_ATTACHMENTS_PER_ITEM
            || item.title.len() > MAX_FIELD_BYTES
            || item.date.len() > MAX_FIELD_BYTES
            || too_long(&item.origin)
            || too_long(&item.destination)
            || too_long(&item.place)
            || too_long(&item.notes)
            || too_long(&item.confirmation_code)
            // A filename came off a stranger's mail like the rest of this,
            // and now reaches the page, so it is bounded like the rest.
            || item.attachments.iter().any(|file| file.filename.len() > MAX_FIELD_BYTES)
        {
            return Err(Error::InputTooLarge);
        }
        for candidate in &item.candidates {
            if candidate.airline.len() > MAX_FIELD_BYTES
                || candidate.flight_numbers.len() > MAX_FIELD_BYTES
                || candidate.itinerary.len() > MAX_FIELD_BYTES
                || candidate
                    .departing_at_local
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
                || candidate
                    .arriving_at_local
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
                || candidate
                    .quoted_currency
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
                || candidate
                    .source
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
            {
                return Err(Error::InputTooLarge);
            }
        }
    }
    Ok(())
}

fn chrome() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SCOUT_CHROME") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.exists())
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn selected(item: &TripItem) -> Option<&TripCandidate> {
    item.candidates
        .iter()
        .find(|candidate| candidate.chosen)
        .or_else(|| (item.candidates.len() == 1).then(|| &item.candidates[0]))
}

/// The booked mark, or nothing when the item is not booked. The paper
/// half of `chat.js::bookedMark`, and one function for every kind of item
/// for the same reason it is one there: a card that marked a flight
/// differently from a stay would read as two different states.
/// Held, or still to book, and the code where there is one — the paper
/// half of `chat.js::statePill` and `bookedMark`.
///
/// Paper says it for an unbooked item too, which the old mark did not:
/// printing nothing at all left the two states looking identical on the
/// page you carry, which is the complaint this answers on screen. The
/// word does the work here, since a printed plan may well be black and
/// white by the time anybody reads it.
fn booked_mark(item: &TripItem) -> String {
    if !item.booked {
        return "<div class=\"state\">To book</div>".to_string();
    }
    match item.confirmation_code.as_deref() {
        Some(code) => format!("<div class=\"state held\">Held · {}</div>", escape(code)),
        None => "<div class=\"state held\">Held</div>".to_string(),
    }
}

/// The files the booking arrived with, named, or nothing when it arrived
/// with none. A traveller carrying the printed plan and a folder of PDFs
/// has to be able to say which file belongs to which booking, and the
/// filename is the only handle both halves share — the page's
/// `attachmentLinks` prints the same string.
///
/// Names only: there is nothing a piece of paper can do with a link, and
/// the bytes are behind a session anyway.
///
/// A real airline names its file `eTicket_Receipt_ABC123_SURNAME_..._LIS.pdf`
/// — one unbreakable token wider than the column. `.segment-head` is a flex
/// row, so its left div needs `min-width:0` and this div needs
/// `overflow-wrap:anywhere`, or the name refuses to shrink and pushes the
/// `<time>` out of a `.segment` that clips its overflow: the date, silently,
/// which is the one thing a traveller reads off paper. Both rules are in the
/// stylesheet above; the page keeps the same pair on its chip, `min-width:0`
/// on the `.attachment-links a` and `overflow-wrap:anywhere` on the span
/// inside it that holds the name.
fn tickets(item: &TripItem) -> String {
    if item.attachments.is_empty() {
        return String::new();
    }
    let label = if item.attachments.len() == 1 { "ticket" } else { "tickets" };
    let names = item
        .attachments
        .iter()
        .map(|file| escape(&file.filename))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("<div class=\"tickets\">{label} · {names}</div>")
}

/// The traveller's own note about this item, or nothing when they wrote
/// none. The paper half of `chat.js::noteParts`, with the one difference
/// that matters: the page makes a bare `http(s)` URL pressable and this
/// does not. Paper cannot be pressed, and a note is the one stored field
/// written to hold a link, so trying would put markup where the traveller
/// sees angle brackets.
///
/// Escaped like every other stored string. `overflow-wrap:anywhere` in the
/// stylesheet for the same reason the ticket names have it: a map link is
/// one unbreakable token wider than the column, and `.segment` clips what
/// overflows — which would silently take the date with it.
fn note(item: &TripItem) -> String {
    match item.notes.as_deref() {
        Some(note) => format!("<div class=\"note\">{}</div>", escape(note)),
        None => String::new(),
    }
}

/// What a leg with no option says, as `(headline, detail)`. The paper
/// half of `chat.js::noFlightLine`, and it has to agree with it: a ticket
/// the traveller holds must not be printed as a route still to be
/// searched, and the two surfaces describing one leg differently is worse
/// than either wording alone. Neither string is escaped because neither
/// comes from anywhere but here.
fn no_flight_line(booked: bool) -> (&'static str, &'static str) {
    match booked {
        true => ("Booked.", "The confirmation did not say which flight."),
        false => ("No flight saved yet.", "Ask Scout in chat to search this route."),
    }
}

/// An end of a flight leg for a sentence. Only a flight reaches the code
/// that asks, so the fallback is for a row that lost its route, not a stay.
fn airport(end: &Option<String>) -> &str {
    end.as_deref().unwrap_or("—")
}

fn duration(minutes: Option<i64>) -> String {
    let Some(minutes) = minutes.filter(|minutes| *minutes >= 0) else {
        return "Duration unavailable".to_string();
    };
    let days = minutes / (24 * 60);
    let hours = (minutes % (24 * 60)) / 60;
    let minutes = minutes % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 || parts.is_empty() {
        parts.push(format!("{minutes:02}m"));
    }
    parts.join(" ")
}

fn date(value: &str) -> String {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|date| date.format("%a, %b %-d, %Y").to_string())
        .unwrap_or_else(|_| value.to_string())
}

fn clock(value: Option<&str>) -> &str {
    value
        .and_then(|value| value.get(11..16))
        .filter(|value| value.as_bytes().get(2) == Some(&b':'))
        .unwrap_or("—")
}

/// "Stay", "Activity", "Transport": a non-flight's kind as its kicker.
fn kind_label(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// When a non-flight item is, by the page's rule (`itemDateLabel`): a stay
/// that ends on a later day is a range, an activity with a clock shows it,
/// anything else is its day.
fn item_when(item: &TripItem) -> String {
    let end = item
        .ends_at
        .as_deref()
        .and_then(|end| end.get(..10))
        .filter(|end| *end != item.date);
    if let Some(end) = end {
        return format!("{} – {}", date(&item.date), date(end));
    }
    match clock(item.starts_at.as_deref()) {
        "—" => date(&item.date),
        clock => format!("{}, {}", date(&item.date), clock),
    }
}

/// The price line on a parked option, as `(amount, qualifier)`. The paper
/// half of `chat.js::savedFareLine`, and it has to agree with it.
///
/// An option that came off a forwarded confirmation and carries no number
/// is not a lookup that failed: the mail did not state a price, and
/// "Price unavailable" sends the reader hunting for a fault that is not
/// there. A fare a search never returned keeps that wording, because there
/// something really did fail to come back.
///
/// The qualifier on a stated fare says what kind of number it is. "paid"
/// for one off a confirmation: that is the total actually paid, and "when
/// saved" hedges a number there is nothing tentative about — this printed
/// "when saved" for it while the page said "paid", which is the two
/// surfaces disagreeing about one leg.
fn fare(candidate: &TripCandidate) -> (String, &'static str) {
    let from_mail = candidate.source.as_deref() == Some("email");
    let from_ignav = candidate.source.as_deref() == Some("ignav");
    let qualifier = match (from_mail, from_ignav) {
        (true, _) => "paid",
        (_, true) => "estimate when saved",
        _ => "when saved",
    };
    let Some(price) = candidate.quoted_price else {
        return match from_mail {
            true => ("Price not stated".to_string(), "on the confirmation"),
            false => ("Price unavailable".to_string(), qualifier),
        };
    };
    let amount = match candidate.quoted_currency.as_deref() {
        Some("EUR") => format!("€{price:.2}"),
        Some("USD") => format!("${price:.2}"),
        Some("GBP") => format!("£{price:.2}"),
        Some("JPY") => format!("¥{price:.0}"),
        Some(currency) => format!("{price:.2} {currency}"),
        None => format!("{price:.2}"),
    };
    match from_ignav {
        true => (format!("from {amount}"), qualifier),
        false => (amount, qualifier),
    }
}

fn saved_total(plan: &Plan) -> Option<String> {
    let mut currency: Option<&str> = None;
    let mut total = 0.0;
    let mut approximate = false;
    // Flights only: a stay's price is not a saved fare, and a stay with no
    // options must not make the total unavailable.
    for flight in plan.trip.flights() {
        let candidate = selected(flight)?;
        let price = candidate.quoted_price?;
        let next = candidate.quoted_currency.as_deref()?;
        if currency.is_some_and(|known| known != next) {
            return None;
        }
        currency = Some(next);
        total += price;
        approximate |= candidate.source.as_deref() == Some("ignav");
    }
    let currency = currency?;
    let amount = match currency {
        "EUR" => format!("€{total:.2}"),
        "USD" => format!("${total:.2}"),
        "GBP" => format!("£{total:.2}"),
        "JPY" => format!("¥{total:.0}"),
        other => format!("{total:.2} {other}"),
    };
    Some(if approximate {
        format!("from {amount}")
    } else {
        amount
    })
}

/// "a, b and c": a list in a sentence, which is where these are read.
/// The paper half of `chat.js::listOf`.
fn list_of(items: &[String]) -> String {
    match items.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The notice at the top of the plan, as `(headline, detail, class)`. The
/// paper half of `chat.js::readinessAlert`, and it has to agree with it in
/// all three states: a hand-off copy that tells its reader to go and shop
/// fares for seats they are already holding is the complaint this answers,
/// and two surfaces describing one trip differently is worse than either
/// wording alone.
///
/// `flights` is how many flights the plan draws, which is what tells
/// "pricing this trip" from "pricing what is left of it" — `legs` names
/// only the ones still to buy.
/// The banner's two questions, printed. `to_book` is the second of them —
/// what the traveller still has to go and get — and it is why "Booked."
/// no longer speaks for a trip with three unbooked activities on it. The
/// page's `chat.js::readinessAlert` says the same things in the same
/// cases; the two must not disagree on paper and on screen.
/// What is still to book, as a sentence, or empty when nothing is. The
/// paper half of `chat.js::bookingLine`, down to where it stops naming
/// and starts counting: a banner is a glance, and the items below say it
/// each for themselves.
fn booking_line(to_book: &[String]) -> String {
    let names: Vec<&str> = to_book.iter().map(|name| name.trim()).filter(|name| !name.is_empty()).collect();
    if names.is_empty() {
        return String::new();
    }
    let owned: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    if owned.len() <= 3 {
        return format!("{} {} not booked yet.", list_of(&owned), if owned.len() == 1 { "is" } else { "are" });
    }
    format!("{}, and {} more, are not booked yet.", list_of(&owned[..3]), owned.len() - 3)
}

fn readiness_notice(
    readiness: &Readiness,
    flights: usize,
    to_book: &[String],
) -> (&'static str, String, &'static str) {
    let booking = booking_line(to_book);
    let joined = |first: String| {
        if booking.is_empty() { first } else { format!("{first} {booking}") }
    };
    match readiness {
        Readiness::Booked if booking.is_empty() => (
            "Booked.",
            "Everything on this trip is held. Nothing is waiting on the traveller.".to_string(),
            "ok",
        ),
        Readiness::Booked => (
            "Flights booked.",
            joined("Every flight on this trip is a ticket the traveller already holds.".to_string()),
            "ok",
        ),
        // A trip with no flights is not a trip with a problem: pricing has
        // nothing to do here, and the refusal the pricing tool gives such a
        // trip was being printed as though something were wrong with it.
        Readiness::NoFlights if booking.is_empty() => (
            "Booked.",
            "Everything on this trip is held. Nothing is waiting on the traveller.".to_string(),
            "ok",
        ),
        Readiness::NoFlights => ("Nothing to price.", format!("This trip has no flights. {booking}"), "ok"),
        Readiness::Ready { legs } if legs.len() < flights => (
            "Ready to price.",
            joined(format!(
                "Only {} {} still to buy; the rest of this trip is already booked. Refresh live \
                 fares with Scout before booking.",
                list_of(legs),
                if legs.len() == 1 { "is" } else { "are" },
            )),
            "ok",
        ),
        Readiness::Ready { .. } => (
            "Ready to price.",
            joined(
                "Every segment has a flight selected. Refresh live fares with Scout before booking."
                    .to_string(),
            ),
            "ok",
        ),
        Readiness::NotReady { reason } => ("Needs a decision.", joined(reason.clone()), ""),
    }
}

/// The join between two legs, or `None` where there is no join to draw.
///
/// The paper half of `chat.js::connectionCheck`, and it has to agree with
/// it: two flights a week apart are not a connection, and neither is a
/// pair with something booked between them — the traveller planned that
/// stay, and a card counting the hours of it as a layover is the bug this
/// answers. `item_between` is the caller's knowledge, because only the
/// loop below can see what sits in the gap.
///
/// A departure scheduled before the previous arrival survives both
/// silences: that is an error in the itinerary rather than advice about a
/// join, and no amount of time or hotel nights makes it flyable. The
/// transfer warning does not survive them — a week ahead, or over a
/// booking, "ground travel is not included" describes the trip the
/// traveller deliberately planned.
///
/// Nothing is lost by the silence over a booked stay: `itinerary_notes` in
/// core still reports a tight turnaround between consecutive flights
/// whatever sits between them, and this plan prints those notes above.
fn connection(
    before: &TripItem,
    after: &TripItem,
    item_between: bool,
) -> Option<(String, &'static str)> {
    let at = airport(&before.destination);
    let same_airport = before.destination == after.origin;
    let parse = |value: Option<&str>| {
        value.and_then(|value| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").ok())
    };
    let arrival = selected(before);
    let departure = selected(after);
    // Only comparable at one airport: both clocks are local to the place
    // they are stated in, so two ends of a transfer cannot be subtracted.
    let minutes = match (same_airport, arrival, departure) {
        (true, Some(arrival), Some(departure)) => parse(arrival.arriving_at_local.as_deref())
            .zip(parse(departure.departing_at_local.as_deref()))
            .map(|(arrival, departure)| (departure - arrival).num_minutes()),
        _ => None,
    };
    if minutes.is_some_and(|minutes| minutes < 0) {
        return Some((
            format!("Impossible connection at {at}: the next flight leaves before arrival."),
            "danger",
        ));
    }
    if item_between {
        return None;
    }
    // Minutes where they are real — one airport, two decided flights, two
    // clocks that mean the same thing — and calendar days where they are
    // all there is. See `days_apart` for why that threshold is two.
    let apart = match minutes {
        Some(minutes) => minutes > 24 * 60,
        None => days_apart(before, after) > DAYS_APART,
    };
    if apart {
        return None;
    }
    if arrival.is_none() {
        return Some((
            "Choose the arriving flight to check this connection.".to_string(),
            "warn",
        ));
    }
    if departure.is_none() {
        return Some((
            "Choose the departing flight to check this connection.".to_string(),
            "warn",
        ));
    }
    if !same_airport {
        return Some((
            format!(
                "Airport transfer: arrive at {at}, continue from {}. Ground travel is not included.",
                airport(&after.origin)
            ),
            "warn",
        ));
    }
    let Some(minutes) = minutes else {
        return Some((format!("Connection at {at}: timing unavailable."), "warn"));
    };
    if minutes < 180 {
        return Some((
            format!(
                "{} at {at} — tight connection; allow at least 3 hours between separate tickets.",
                duration(Some(minutes)),
            ),
            "danger",
        ));
    }
    Some((
        format!(
            "{} at {at} between the selected flights.",
            duration(Some(minutes)),
        ),
        "ok",
    ))
}

/// Whole days from the day one leg leaves to the day the next one leaves.
/// The paper half of `chat.js::daysApart`, and the comment there is the
/// long version: both sides are the same kind of day on purpose, the
/// measure errs only towards looking further apart than the legs are, and
/// `DAYS_APART` is the slack that buys back. A date that will not parse
/// leaves the two treated as adjacent, so the check still runs — silence
/// is what costs somebody a connection.
fn days_apart(before: &TripItem, after: &TripItem) -> i64 {
    let day = |item: &TripItem| {
        NaiveDate::parse_from_str(item.date.get(..10).unwrap_or(""), "%Y-%m-%d").ok()
    };
    match day(before).zip(day(after)) {
        Some((from, to)) => (to - from).num_days(),
        None => 0,
    }
}

pub fn html(plan: &Plan) -> String {
    let trip = &plan.trip;
    // The route line, the selected count and the connection checks are
    // about flights: a stay has no airports and no options to choose.
    let flights: Vec<&TripItem> = trip.flights().collect();
    let route = if flights.is_empty() {
        "Route not set".to_string()
    } else {
        let mut airports = vec![airport(&flights[0].origin)];
        for flight in &flights {
            let origin = airport(&flight.origin);
            if airports.last().copied() != Some(origin) {
                airports.push(origin);
            }
            airports.push(airport(&flight.destination));
        }
        airports.join(" → ")
    };
    let generated = Utc::now().format("%Y-%m-%d %H:%M UTC");
    let selected_count = flights
        .iter()
        .filter(|flight| selected(flight).is_some())
        .count();

    let mut out =
        String::from("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>");
    out.push_str(&escape(&format!("{} — planned trip", trip.name)));
    out.push_str(
        r#"</title>
<style>
@page{size:A4;margin:11mm 13mm 13mm}*{box-sizing:border-box}body{margin:0;color:#17343b;background:#fff;font:9.5pt/1.35 Arial,"Liberation Sans",sans-serif}header{border-bottom:2px solid #2aa198;padding-bottom:5mm;margin-bottom:5mm}.brand{color:#2aa198;font-size:9pt;font-weight:700;letter-spacing:.16em;text-transform:uppercase}.route{margin:1.5mm 0 .5mm;color:#50666b;font-size:9pt;font-weight:700;letter-spacing:.08em}.title{margin:0;color:#002b36;font-size:24pt;line-height:1.05}.summary{display:grid;grid-template-columns:repeat(5,1fr);gap:2mm;margin:4mm 0 0}.fact{padding:2.3mm;background:#f2f7f6;border-radius:2mm}.fact b{display:block;color:#61767a;font-size:6.8pt;text-transform:uppercase;letter-spacing:.08em}.fact span{display:block;margin-top:.7mm;color:#002b36;font-size:9.5pt;font-weight:700}.notice{margin:0 0 3.5mm;padding:2.4mm 3mm;border-left:3px solid #b58900;background:#fff9e7;color:#6c5817}.notice.ok{border-color:#859900;background:#f6f8e8;color:#4f5d10}.page-note{margin:-1mm 0 4mm;color:#61767a;font-size:8pt}.segment{break-inside:avoid;margin:0 0 4mm;border:1px solid #cad9d7;border-radius:2.5mm;overflow:hidden}.segment-head{display:flex;justify-content:space-between;gap:5mm;padding:3mm;background:#eaf3f2}.segment-head>div{min-width:0}.segment-head .number{color:#61767a;font-size:7pt;font-weight:700;letter-spacing:.1em;text-transform:uppercase}.segment-head h2{margin:.7mm 0 0;color:#002b36;font-size:14pt}.segment-head time{color:#50666b;font-size:8pt}.segment-head .place{margin-top:.7mm;color:#50666b;font-size:8.5pt}.segment-head .state{display:inline-block;margin-top:1mm;padding:.5mm 1.8mm;border:.3mm dashed #789196;border-radius:99mm;color:#50666b;font-size:7pt;font-weight:700}.segment-head .state.held{border-style:solid;border-color:#859900;color:#4f5d10}.segment-head .tickets{margin-top:.7mm;color:#50666b;font-size:7.5pt;overflow-wrap:anywhere}.option{display:grid;grid-template-columns:6mm 1fr 30mm;gap:2.5mm;padding:3mm;border-top:1px solid #dbe6e4;break-inside:avoid}.option.selected{background:#effaf8;border-left:3px solid #2aa198}.mark{width:4.5mm;height:4.5mm;border:1.5px solid #789196;border-radius:50%;margin-top:.7mm}.selected .mark{border:1.5px solid #2aa198;box-shadow:inset 0 0 0 1mm #effaf8;background:#2aa198}.airline{color:#002b36;font-weight:700}.numbers,.source{color:#61767a;font-size:7.5pt}.itinerary{margin:1.3mm 0 .7mm;color:#002b36;font:8.5pt/1.35 ui-monospace,SFMono-Regular,Menlo,monospace}.meta{color:#50666b;font-size:7.8pt}.price{text-align:right;color:#002b36;font-size:11.5pt;font-weight:700}.price small{display:block;color:#61767a;font-size:6.5pt;font-weight:400;text-transform:uppercase}.note{padding:2.4mm 3mm;border-top:1px solid #dbe6e4;color:#50666b;font-size:8pt;overflow-wrap:anywhere}.connection{break-inside:avoid;margin:-1.5mm 3mm 3mm;padding:2mm 2.5mm;border-left:2px solid #859900;background:#f7f9ef;color:#4f5d10}.connection.warn{border-color:#b58900;background:#fff9e7;color:#6c5817}.connection.danger{border-color:#dc322f;background:#fff0ef;color:#8f211f}.foot{break-inside:avoid;margin-top:4mm;padding-top:3mm;border-top:1px solid #cad9d7;color:#61767a;font-size:7.5pt}.foot strong{color:#17343b}@media print{a{color:inherit;text-decoration:none}}
</style></head><body>"#,
    );
    write!(
        out,
        "<header><div class=\"brand\">Scout · Planned trip</div><div class=\"route\">{}</div><h1 class=\"title\">{}</h1><div class=\"summary\">",
        escape(&route),
        escape(&trip.name)
    )
    .unwrap();
    for (label, value) in [
        ("Travellers", trip.adults.to_string()),
        (
            "Cabin",
            trip.cabin_class
                .as_deref()
                .unwrap_or("Not set")
                .replace('_', " "),
        ),
        ("Status", trip.status.clone()),
        (
            "Flights selected",
            format!("{selected_count}/{}", flights.len()),
        ),
        (
            "Saved fare total",
            saved_total(plan).unwrap_or_else(|| "Not available".to_string()),
        ),
    ] {
        write!(
            out,
            "<div class=\"fact\"><b>{}</b><span>{}</span></div>",
            escape(label),
            escape(&value)
        )
        .unwrap();
    }
    out.push_str("</div></header>");

    let (headline, detail, tone) = readiness_notice(&plan.readiness, flights.len(), &plan.to_book);
    write!(
        out,
        "<div class=\"notice {tone}\"><strong>{headline}</strong> {}</div>",
        escape(&detail)
    )
    .unwrap();
    for note in &plan.notes {
        write!(
            out,
            // `chat.js::ITINERARY_NOTE`, which has the long version: these
            // notes are not all about connections, and the two surfaces
            // were heading the same sentence two different ways.
            "<div class=\"notice\"><strong>Itinerary note.</strong> {}</div>",
            escape(note)
        )
        .unwrap();
    }
    out.push_str("<p class=\"page-note\">Times are local to each airport. Flight options not marked Selected are saved alternatives.</p>");

    for (index, segment) in trip.items.iter().enumerate() {
        if !segment.is_flight() {
            // The page's card, on paper: what it is, its name, where, and
            // a booking's code — the one thing a traveller reads off an
            // itinerary at a hotel desk.
            let place = segment
                .place
                .as_deref()
                .map(|place| format!("<div class=\"place\">{}</div>", escape(place)))
                .unwrap_or_default();
            let booked = booked_mark(segment);
            write!(
                out,
                "<section class=\"segment\"><div class=\"segment-head\"><div><div class=\"number\">{}</div><h2>{}</h2>{}{}{}</div><time>{}</time></div>{}</section>",
                escape(&kind_label(&segment.kind)),
                escape(&segment.title),
                place,
                booked,
                tickets(segment),
                escape(&item_when(segment)),
                note(segment)
            )
            .unwrap();
            continue;
        }
        write!(
            out,
            "<section class=\"segment\"><div class=\"segment-head\"><div><div class=\"number\">Segment {}</div><h2>{} → {}</h2>{}{}</div><time>{}</time></div>{}",
            segment.position,
            escape(airport(&segment.origin)),
            escape(airport(&segment.destination)),
            // The mark a stay carries, on a leg: a leg bought by forwarding
            // a confirmation has a code, and the code is the one thing a
            // traveller reads off a printed itinerary at a desk.
            booked_mark(segment),
            tickets(segment),
            escape(&date(&segment.date)),
            note(segment)
        )
        .unwrap();
        if segment.candidates.is_empty() {
            let (headline, detail) = no_flight_line(segment.booked);
            write!(out, "<div class=\"option\"><div></div><div><strong>{headline}</strong><div class=\"meta\">{detail}</div></div></div>")
                .unwrap();
        }
        let picked = selected(segment).map(|candidate| candidate.candidate);
        for candidate in &segment.candidates {
            let is_selected = picked == Some(candidate.candidate);
            let (amount, qualifier) = fare(candidate);
            write!(
                out,
                "<div class=\"option {}\"><div class=\"mark\"></div><div><div><span class=\"airline\">{}</span> <span class=\"numbers\">{}</span>{}</div><div class=\"itinerary\">{}</div><div class=\"meta\">{} → {} · {}{} </div></div><div class=\"price\">{}<small>{}</small></div></div>",
                if is_selected { "selected" } else { "" },
                escape(&candidate.airline),
                escape(&candidate.flight_numbers.replace(',', " ·")),
                candidate
                    .source
                    .as_deref()
                    .map(|source| format!(" <span class=\"source\">{}</span>", escape(source)))
                    .unwrap_or_default(),
                escape(&candidate.itinerary),
                escape(clock(candidate.departing_at_local.as_deref())),
                escape(clock(candidate.arriving_at_local.as_deref())),
                escape(&duration(candidate.duration_minutes)),
                if is_selected { " · Selected" } else { " · Alternative" },
                escape(&amount),
                qualifier,
            )
            .unwrap();
        }
        out.push_str("</section>");
        // The connection is to the next *flight*, whatever sits between:
        // a stay between two legs does not change when the second departs.
        // What sits between does change whether there is a join to draw at
        // all, and only this loop can see it, so it is passed down.
        let rest = &trip.items[index + 1..];
        if let Some(gap) = rest.iter().position(|item| item.is_flight()) {
            if let Some((message, tone)) = connection(segment, &rest[gap], gap > 0) {
                write!(
                    out,
                    "<div class=\"connection {}\"><strong>Connection:</strong> {}</div>",
                    tone,
                    escape(&message)
                )
                .unwrap();
            }
        }
    }

    write!(
        out,
        "<footer class=\"foot\"><strong>Saved itinerary, not a ticket.</strong> Prices shown are the amounts recorded when these options were saved and may have changed. Refresh live fares with Scout before booking. Generated {generated}.</footer></body></html>"
    )
    .unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::trips::{Plan, Trip, TripCandidate, TripItem};

    /// A chosen option with nothing on it but the fact of being chosen:
    /// a base for the cases that care only about times and airports.
    fn candidate() -> TripCandidate {
        TripCandidate {
            candidate: 1,
            chosen: true,
            airline: "KLM".to_string(),
            flight_numbers: "KL1579".to_string(),
            itinerary: "somewhere".to_string(),
            departing_at_local: None,
            arriving_at_local: None,
            duration_minutes: None,
            quoted_price: None,
            quoted_currency: None,
            source: None,
        }
    }

    /// A flight leg as `load_trip` reads one: its time of day comes from
    /// the chosen option, the columns a stay uses are empty.
    fn flight(position: i64, origin: &str, destination: &str, candidate: TripCandidate) -> TripItem {
        TripItem {
            id: position,
            position,
            kind: "flight".to_string(),
            title: format!("{origin} → {destination}"),
            place: None,
            origin: Some(origin.to_string()),
            destination: Some(destination.to_string()),
            date: "2026-10-12".to_string(),
            starts_at: candidate.departing_at_local.clone(),
            ends_at: candidate.arriving_at_local.clone(),
            booked: false,
            confirmation_code: None,
            price: None,
            currency: None,
            notes: None,
            arrival_id: None,
            candidates: vec![candidate],
            attachments: Vec::new(),
        }
    }

    fn plan() -> Plan {
        Plan {
            trip: Trip {
                id: 7,
                name: "October <escape>".to_string(),
                adults: 2,
                cabin_class: Some("premium_economy".to_string()),
                status: "planning".to_string(),
                items: vec![
                    flight(
                        1,
                        "AMS",
                        "LIS",
                        TripCandidate {
                            candidate: 1,
                            chosen: true,
                            airline: "KLM & friends".to_string(),
                            flight_numbers: "KL1579".to_string(),
                            itinerary: "AMS 08:20 12.10 ✈ LIS 10:25 12.10".to_string(),
                            departing_at_local: Some("2026-10-12T08:20:00".to_string()),
                            arriving_at_local: Some("2026-10-12T10:25:00".to_string()),
                            duration_minutes: Some(185),
                            quoted_price: Some(184.0),
                            quoted_currency: Some("EUR".to_string()),
                            source: Some("duffel".to_string()),
                        },
                    ),
                    // A stay on the same timeline, between the two legs. It
                    // has no options and no route, so it must not count
                    // towards "flights selected" or the saved total — and
                    // sitting between the flights, it is what the
                    // connection check has to look past to find the second.
                    TripItem {
                        id: 2,
                        position: 2,
                        kind: "stay".to_string(),
                        title: "Hotel <Roma>".to_string(),
                        place: Some("Rome".to_string()),
                        origin: None,
                        destination: None,
                        date: "2026-10-12".to_string(),
                        starts_at: None,
                        ends_at: Some("2026-10-15T11:00:00".to_string()),
                        booked: true,
                        confirmation_code: Some("ABC123".to_string()),
                        price: Some(410.0),
                        currency: Some("EUR".to_string()),
                        notes: None,
                        arrival_id: None,
                        candidates: vec![],
                        // The booking arrived by mail with its ticket on
                        // it, which is the common case for an item that
                        // is already booked.
                        attachments: vec![scout_api::AttachmentRef {
                            id: 4,
                            filename: "voucher <1>.pdf".to_string(),
                            mime: "application/pdf".to_string(),
                        }],
                    },
                    flight(
                        3,
                        "LIS",
                        "FCO",
                        TripCandidate {
                            candidate: 1,
                            chosen: true,
                            airline: "TAP".to_string(),
                            flight_numbers: "TP834".to_string(),
                            itinerary: "LIS 12:00 12.10 ✈ FCO 16:00 12.10".to_string(),
                            departing_at_local: Some("2026-10-12T12:00:00".to_string()),
                            arriving_at_local: Some("2026-10-12T16:00:00".to_string()),
                            duration_minutes: Some(180),
                            quoted_price: Some(126.0),
                            quoted_currency: Some("EUR".to_string()),
                            source: Some("ignav".to_string()),
                        },
                    ),
                ],
                // Only a kept trip can reach here: the traveller has to see
                // a trip in their list before they can ask for its PDF.
                kept: true,
            },
            readiness: Readiness::Ready {
                legs: vec!["segment 1 (AMS→LIS)".to_string(), "segment 3 (LIS→FCO)".to_string()],
            },
            // The field one release of open tabs still reads. Nothing in
            // this file touches it: the printed plan is built from
            // `readiness` like the page's own renderer.
            not_ready: None,
            to_book: Vec::new(),
            notes: vec!["Separate tickets need extra care.".to_string()],
            chat: None,
        }
    }

    #[test]
    fn the_document_contains_every_detail_and_escapes_stored_text() {
        let mut plan = plan();
        // A leg bought by forwarding a confirmation carries its ticket too,
        // and the flight head prints it from its own call — this leg is not
        // booked, so a head that printed `booked_mark` here instead would
        // show nothing at all.
        plan.trip.items[0].attachments = vec![scout_api::AttachmentRef {
            id: 9,
            filename: "eTicket <KL1579>.pdf".to_string(),
            mime: "application/pdf".to_string(),
        }];
        let html = html(&plan);
        for expected in [
            "October &lt;escape&gt;",
            "AMS → LIS → FCO",
            "2/2",
            "from €310.00",
            "premium economy",
            "KLM &amp; friends",
            "KL1579",
            "AMS 08:20 12.10 ✈ LIS 10:25 12.10",
            "from €126.00",
            "estimate when saved",
            "Saved itinerary, not a ticket",
            // The heading over core's notes, pinned here and in
            // chat.test.mjs: the two surfaces headed the same sentence two
            // different ways, and nothing else holds them together.
            "<strong>Itinerary note.</strong> Separate tickets need extra care.",
            // The stay is on the page as its own line, escaped like the
            // rest: kind, name, place, its range, and the booking code.
            "<div class=\"number\">Stay</div>",
            "Hotel &lt;Roma&gt;",
            "Rome",
            "Held · ABC123",
            // Beside the code: which ticket belongs to this booking, for a
            // traveller holding the printed plan and a folder of PDFs.
            // Escaped like everything else that came out of a stranger's
            // mail.
            "ticket · voucher &lt;1&gt;.pdf",
            // And on a flight head, which prints it from its own call.
            "ticket · eTicket &lt;KL1579&gt;.pdf",
        ] {
            assert!(html.contains(expected), "missing `{expected}`");
        }
        assert!(
            html.contains(&format!("{} – {}", date("2026-10-12"), date("2026-10-15"))),
            "a stay that ends on a later day is not shown as a range"
        );
        assert!(!html.contains("October <escape>"));
        assert!(!html.contains("Hotel <Roma>"));
    }

    #[test]
    fn the_printed_banner_says_what_is_still_to_book_the_way_the_page_does() {
        // The two surfaces have to agree: a trip whose flights are held
        // but whose activities are not must not be headed "Booked." on
        // paper while the page says otherwise — and "nothing is waiting
        // on you" is the sentence the live report was about.
        let mut held = plan();
        held.readiness = Readiness::Booked;
        held.to_book = vec!["Silver workshop".to_string(), "Dolphin tour".to_string()];
        let page = html(&held);
        assert!(page.contains("Flights booked."), "{page}");
        assert!(page.contains("Silver workshop and Dolphin tour are not booked yet."), "{page}");
        assert!(!page.contains("Nothing is waiting"), "{page}");

        held.to_book = Vec::new();
        let page = html(&held);
        assert!(page.contains("Booked."));
        assert!(page.contains("Nothing is waiting on the traveller."), "{page}");

        // A trip with no flights is not a trip with a problem.
        let mut city = plan();
        city.readiness = Readiness::NoFlights;
        city.to_book = vec!["Hotel Alfama".to_string()];
        let page = html(&city);
        assert!(page.contains("Nothing to price."), "{page}");
        assert!(page.contains("Hotel Alfama is not booked yet."), "{page}");
        assert!(!page.contains("no flights yet"), "a flightless trip was printed as a fault: {page}");
    }

    #[test]
    fn a_note_is_printed_on_the_item_it_belongs_to_as_text_and_only_as_text() {
        // The note is the one field on an item written to hold a link, and
        // paper can do nothing with a link — so it prints as the characters
        // the traveller typed, escaped like every other stored string, and
        // never as an anchor. The page's card is where a link is pressable.
        let mut noted = plan();
        noted.trip.items[0].notes = Some("gate <B> — https://maps.example/a?b=1&c=2".to_string());
        noted.trip.items[1].notes = Some("ask for the terrace".to_string());
        let page = html(&noted);
        assert!(page.contains("ask for the terrace"), "{page}");
        assert!(
            page.contains("gate &lt;B&gt; — https://maps.example/a?b=1&amp;c=2"),
            "{page}"
        );
        assert!(!page.contains("<a href"), "a printed note is not a link: {page}");
        // An item with nothing written on it gains no empty line.
        assert!(!html(&plan()).contains("class=\"note\""));
    }

    #[test]
    fn a_booked_leg_with_no_option_is_not_printed_as_a_route_to_search() {
        // The paper half of `chat.js::noFlightLine`: a ticket the traveller
        // is carrying must not be printed as a flight nobody has looked for
        // yet — least of all on the page they hand to a desk.
        let mut booked = plan();
        let leg = &mut booked.trip.items[0];
        leg.candidates.clear();
        leg.booked = true;
        leg.confirmation_code = Some("KL<7788>".to_string());
        let page = html(&booked);
        assert!(page.contains("The confirmation did not say which flight."), "{page}");
        assert!(!page.contains("No flight saved yet."));
        // And the code, on a leg as on a stay: it is the one thing read off
        // a printed itinerary at a desk. Escaped like everything stored.
        assert!(page.contains("Held · KL&lt;7788&gt;"), "{page}");
        // A leg nobody has bought still says what it needs.
        let mut unbooked = plan();
        unbooked.trip.items[0].candidates.clear();
        assert!(html(&unbooked).contains("Ask Scout in chat to search this route."));
    }

    #[test]
    fn a_confirmation_that_stated_no_price_is_not_printed_as_a_failed_lookup() {
        // The paper half of `chat.js::savedFareLine`. Nothing failed here:
        // the airline's mail did not state a price, and "Price unavailable"
        // sends the reader hunting for a fault that is not there.
        let mut plan = plan();
        let option = &mut plan.trip.items[0].candidates[0];
        option.quoted_price = None;
        option.quoted_currency = None;
        option.source = Some("email".to_string());
        let page = html(&plan);
        assert!(page.contains("Price not stated"), "{page}");
        assert!(page.contains("on the confirmation"), "{page}");
        assert!(!page.contains("Price unavailable"), "{page}");

        // A fare a search never returned still says so: there, something
        // really did fail to come back.
        let mut searched = plan.clone();
        searched.trip.items[0].candidates[0].source = Some("duffel".to_string());
        assert!(html(&searched).contains("Price unavailable"));
    }

    #[test]
    fn a_fare_off_a_confirmation_is_printed_as_paid_here_too() {
        // The page says "paid" for a fare that came off a forwarded
        // confirmation, because it is the total actually paid rather than a
        // quote that may have moved. This said "when saved", which hedges a
        // number there is nothing tentative about.
        let mut plan = plan();
        plan.trip.items[0].candidates[0].source = Some("email".to_string());
        let page = html(&plan);
        assert!(page.contains("<small>paid</small>"), "{page}");
    }

    #[test]
    fn the_shared_connection_cases_decide_the_same_way_here_as_on_the_page() {
        // `connection_gaps.json` is read by this test and by the matching
        // one in chat.test.mjs. The page and this plan draw the same card
        // in the same places from two implementations, and that file is
        // the only thing that makes one of them go red when the other's
        // threshold moves — the calendar-day fallback in particular is
        // reached by none of the hand-written cases on either side.
        let gaps: serde_json::Value =
            serde_json::from_str(include_str!("connection_gaps.json")).unwrap();
        let cases = gaps["cases"].as_array().unwrap();
        assert!(cases.len() >= 12, "the shared cases went missing");
        let leg = |side: &serde_json::Value| {
            let stamp = |key: &str| side[key].as_str().map(str::to_string);
            let (arriving, departing) = (stamp("arriving_at_local"), stamp("departing_at_local"));
            let decided = arriving.is_some() || departing.is_some() || side["chosen"] == true;
            TripItem {
                origin: side["origin"].as_str().map(str::to_string),
                destination: side["destination"].as_str().map(str::to_string),
                date: side["date"].as_str().unwrap().to_string(),
                candidates: match decided {
                    false => Vec::new(),
                    true => vec![TripCandidate {
                        arriving_at_local: arriving,
                        departing_at_local: departing,
                        ..candidate()
                    }],
                },
                ..flight(1, "AAA", "BBB", candidate())
            }
        };
        for case in cases {
            let drawn = connection(
                &leg(&case["before"]),
                &leg(&case["after"]),
                case["item_between"] == true,
            );
            let got = match &drawn {
                None => "none",
                Some((_, "ok")) => "fine",
                Some((_, "warn")) => "warning",
                Some((_, tone)) => tone,
            };
            assert_eq!(got, case["expect"].as_str().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn the_printed_plan_draws_a_join_only_where_the_page_would() {
        // The paper half of `chat.js::connectionCheck`. This plan's two
        // legs are a same-day connection with a hotel booked between them,
        // which is where the page now says nothing at all: the traveller
        // booked the stay and knows they are staying.
        let mut joined = plan();
        assert!(!html(&joined).contains("Connection:"), "{}", html(&joined));

        // Nothing between them, and it is a join again — saying how long
        // it is and where, which is the whole use of the card. The shared
        // cases pin which card appears; only this pins what it reads.
        joined.trip.items.remove(1);
        let page = html(&joined);
        assert!(
            page.contains("1h 35m at LIS — tight connection; allow at least 3 hours between separate tickets."),
            "{page}",
        );

        // A week apart is not a connection however little sits between.
        let mut apart = joined.clone();
        apart.trip.items[1].date = "2026-10-19".to_string();
        apart.trip.items[1].candidates[0].departing_at_local = Some("2026-10-19T12:00:00".to_string());
        assert!(!html(&apart).contains("Connection:"), "{}", html(&apart));

        // An itinerary that cannot be flown is an error, not information
        // about a join, so it survives both silences.
        let mut impossible = plan();
        impossible.trip.items[2].candidates[0].departing_at_local =
            Some("2026-10-12T09:00:00".to_string());
        assert!(html(&impossible).contains("Impossible connection"), "{}", html(&impossible));
    }

    #[test]
    fn the_printed_plan_says_the_same_three_things_about_pricing_the_page_does() {
        // The page and the paper describing one trip differently is worse
        // than either wording alone, and a hand-off copy telling its reader
        // to go and shop flights they have already bought is the complaint
        // this whole change comes from.
        let mut booked = plan();
        booked.readiness = Readiness::Booked;
        let page = html(&booked);
        assert!(page.contains("Booked."), "{page}");
        assert!(!page.contains("Ready to price."), "{page}");
        assert!(!page.contains("Needs a decision."), "{page}");

        let mut part = plan();
        part.readiness = Readiness::Ready { legs: vec!["segment 3 (LIS→FCO)".to_string()] };
        let page = html(&part);
        assert!(page.contains("Ready to price."), "{page}");
        assert!(page.contains("segment 3 (LIS→FCO)"), "{page}");
        assert!(page.contains("already booked"), "the bought leg is not in that total: {page}");

        let mut undecided = plan();
        undecided.readiness = Readiness::NotReady { reason: "segment 1 has no flight".to_string() };
        let page = html(&undecided);
        assert!(page.contains("Needs a decision."), "{page}");
        assert!(page.contains("segment 1 has no flight"), "{page}");
    }

    #[test]
    fn filenames_are_ascii_bounded_and_header_safe() {
        assert_eq!(filename("October Escape"), "october-escape-itinerary.pdf");
        assert_eq!(filename("  東京 / 2027  "), "2027-itinerary.pdf");
        assert!(!filename(&"a".repeat(200)).len().gt(&80));
    }

    #[test]
    fn oversized_stored_text_is_rejected_before_rendering() {
        let mut oversized = plan();
        oversized.trip.name = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(matches!(
            validate_plan(&oversized),
            Err(Error::InputTooLarge)
        ));

        // A filename is stranger-supplied bytes off an email like the rest,
        // and this file bounds what it prints rather than trusting whoever
        // stored it.
        let mut named = plan();
        named.trip.items[1].attachments[0].filename = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(matches!(validate_plan(&named), Err(Error::InputTooLarge)));

        // Short names, but a great many of them: bounded on its own here,
        // beside the candidate count, rather than through whatever limit
        // the ingest path happens to keep.
        let mut many = plan();
        many.trip.items[1].attachments = std::iter::repeat_n(
            scout_api::AttachmentRef {
                id: 1,
                filename: "t.pdf".to_string(),
                mime: "application/pdf".to_string(),
            },
            MAX_ATTACHMENTS_PER_ITEM + 1,
        )
        .collect();
        assert!(matches!(validate_plan(&many), Err(Error::InputTooLarge)));

        // And the fixture as it stands, with its one ticket, is fine.
        assert!(validate_plan(&plan()).is_ok());
    }

    /// Shaped like what Chromium writes: a header, a body comfortably over the
    /// minimum, and the trailer that says the document is finished.
    fn written_pdf() -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        pdf.extend(std::iter::repeat_n(b'x', 2048));
        pdf.extend_from_slice(b"\nstartxref\n14309\n%%EOF\n");
        pdf
    }

    #[test]
    fn a_pdf_is_only_finished_once_it_carries_its_trailer() {
        let whole = written_pdf();
        assert!(complete(&whole));
        // Chromium's own output ends with a newline; a trailer sitting flush
        // against the end of the file is just as finished.
        assert!(complete(whole.trim_ascii_end()));

        // Every prefix of a real PDF is a file we could catch mid-write, and
        // not one of them may be handed to a reader as a finished download.
        for cut in [1, 64, 1200, whole.len() - 6, whole.len() - 2] {
            assert!(
                !complete(&whole[..cut]),
                "a PDF truncated to {cut} bytes was accepted as finished"
            );
        }

        // The trailer alone is not enough: it has to be a PDF, and it has to
        // be big enough to be a page rather than a fragment.
        assert!(!complete(b"not a pdf at all %%EOF"));
        assert!(!complete(b"%PDF-1.4\n%%EOF\n"));
    }

    #[tokio::test]
    async fn chromium_produces_a_real_pdf_when_it_is_installed() {
        if !available() {
            return;
        }
        let pdf = render(&plan()).await.unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(pdf.len() > 10_000);
    }
}
