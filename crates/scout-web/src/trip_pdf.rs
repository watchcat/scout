//! A printable snapshot of a durable trip.
//!
//! The browser UI is interactive and intentionally compact. This document is
//! the hand-off copy: every route, option, warning and saved-fare caveat in a
//! self-contained HTML page that Chromium can print without network access.

use chrono::{NaiveDate, NaiveDateTime, Utc};
use scout_core::trips::{Plan, TripCandidate, TripSegment};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

const PDF_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_PDFS: usize = 2;
const MAX_PDF_BYTES: u64 = 10 * 1024 * 1024;
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 64;
const MAX_CANDIDATES_PER_SEGMENT: usize = 64;
const MAX_FIELD_BYTES: usize = 64 * 1024;

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
        .kill_on_drop(true);

    let result = tokio::time::timeout(PDF_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::Timeout)??;
    if !result.status.success() {
        return Err(Error::Chrome(
            String::from_utf8_lossy(&result.stderr)
                .trim()
                .chars()
                .take(500)
                .collect(),
        ));
    }
    if tokio::fs::metadata(&output).await?.len() > MAX_PDF_BYTES {
        return Err(Error::InvalidOutput);
    }
    let bytes = tokio::fs::read(output).await?;
    if !bytes.starts_with(b"%PDF-") || bytes.len() < 1024 {
        return Err(Error::InvalidOutput);
    }
    Ok(bytes)
}

fn validate_plan(plan: &Plan) -> Result<(), Error> {
    if plan.trip.segments.len() > MAX_SEGMENTS
        || plan.trip.name.len() > MAX_FIELD_BYTES
        || plan.notes.iter().any(|note| note.len() > MAX_FIELD_BYTES)
    {
        return Err(Error::InputTooLarge);
    }
    for segment in &plan.trip.segments {
        if segment.candidates.len() > MAX_CANDIDATES_PER_SEGMENT
            || segment.origin.len() > MAX_FIELD_BYTES
            || segment.destination.len() > MAX_FIELD_BYTES
            || segment.departure_date.len() > MAX_FIELD_BYTES
        {
            return Err(Error::InputTooLarge);
        }
        for candidate in &segment.candidates {
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

fn selected(segment: &TripSegment) -> Option<&TripCandidate> {
    segment
        .candidates
        .iter()
        .find(|candidate| candidate.chosen)
        .or_else(|| (segment.candidates.len() == 1).then(|| &segment.candidates[0]))
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

fn fare(candidate: &TripCandidate) -> String {
    let Some(price) = candidate.quoted_price else {
        return "Price unavailable".to_string();
    };
    let amount = match candidate.quoted_currency.as_deref() {
        Some("EUR") => format!("€{price:.2}"),
        Some("USD") => format!("${price:.2}"),
        Some("GBP") => format!("£{price:.2}"),
        Some("JPY") => format!("¥{price:.0}"),
        Some(currency) => format!("{price:.2} {currency}"),
        None => format!("{price:.2}"),
    };
    if candidate.source.as_deref() == Some("ignav") {
        format!("from {amount}")
    } else {
        amount
    }
}

fn saved_total(plan: &Plan) -> Option<String> {
    let mut currency: Option<&str> = None;
    let mut total = 0.0;
    let mut approximate = false;
    for segment in &plan.trip.segments {
        let candidate = selected(segment)?;
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

fn connection(before: &TripSegment, after: &TripSegment) -> (String, &'static str) {
    let Some(arrival) = selected(before) else {
        return (
            "Choose the arriving flight to check this connection.".to_string(),
            "warn",
        );
    };
    let Some(departure) = selected(after) else {
        return (
            "Choose the departing flight to check this connection.".to_string(),
            "warn",
        );
    };
    if before.destination != after.origin {
        return (
            format!(
                "Airport transfer: arrive at {}, continue from {}. Ground travel is not included.",
                before.destination, after.origin
            ),
            "warn",
        );
    }
    let parse = |value: Option<&str>| {
        value.and_then(|value| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").ok())
    };
    let (Some(arrival), Some(departure)) = (
        parse(arrival.arriving_at_local.as_deref()),
        parse(departure.departing_at_local.as_deref()),
    ) else {
        return (
            format!("Connection at {}: timing unavailable.", before.destination),
            "warn",
        );
    };
    let minutes = (departure - arrival).num_minutes();
    if minutes < 0 {
        return (
            format!(
                "Impossible connection at {}: the next flight leaves before arrival.",
                before.destination
            ),
            "danger",
        );
    }
    if minutes < 180 {
        return (
            format!(
                "{} at {} — tight connection; allow at least 3 hours between separate tickets.",
                duration(Some(minutes)),
                before.destination
            ),
            "danger",
        );
    }
    (
        format!(
            "{} at {} between the selected flights.",
            duration(Some(minutes)),
            before.destination
        ),
        "ok",
    )
}

pub fn html(plan: &Plan) -> String {
    let trip = &plan.trip;
    let route = if trip.segments.is_empty() {
        "Route not set".to_string()
    } else {
        let mut airports = vec![trip.segments[0].origin.as_str()];
        for segment in &trip.segments {
            if airports.last().copied() != Some(segment.origin.as_str()) {
                airports.push(&segment.origin);
            }
            airports.push(&segment.destination);
        }
        airports.join(" → ")
    };
    let generated = Utc::now().format("%Y-%m-%d %H:%M UTC");
    let selected_count = trip
        .segments
        .iter()
        .filter(|segment| selected(segment).is_some())
        .count();

    let mut out =
        String::from("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>");
    out.push_str(&escape(&format!("{} — planned trip", trip.name)));
    out.push_str(
        r#"</title>
<style>
@page{size:A4;margin:11mm 13mm 13mm}*{box-sizing:border-box}body{margin:0;color:#17343b;background:#fff;font:9.5pt/1.35 Arial,"Liberation Sans",sans-serif}header{border-bottom:2px solid #2aa198;padding-bottom:5mm;margin-bottom:5mm}.brand{color:#2aa198;font-size:9pt;font-weight:700;letter-spacing:.16em;text-transform:uppercase}.route{margin:1.5mm 0 .5mm;color:#50666b;font-size:9pt;font-weight:700;letter-spacing:.08em}.title{margin:0;color:#002b36;font-size:24pt;line-height:1.05}.summary{display:grid;grid-template-columns:repeat(5,1fr);gap:2mm;margin:4mm 0 0}.fact{padding:2.3mm;background:#f2f7f6;border-radius:2mm}.fact b{display:block;color:#61767a;font-size:6.8pt;text-transform:uppercase;letter-spacing:.08em}.fact span{display:block;margin-top:.7mm;color:#002b36;font-size:9.5pt;font-weight:700}.notice{margin:0 0 3.5mm;padding:2.4mm 3mm;border-left:3px solid #b58900;background:#fff9e7;color:#6c5817}.notice.ok{border-color:#859900;background:#f6f8e8;color:#4f5d10}.page-note{margin:-1mm 0 4mm;color:#61767a;font-size:8pt}.segment{break-inside:avoid;margin:0 0 4mm;border:1px solid #cad9d7;border-radius:2.5mm;overflow:hidden}.segment-head{display:flex;justify-content:space-between;gap:5mm;padding:3mm;background:#eaf3f2}.segment-head .number{color:#61767a;font-size:7pt;font-weight:700;letter-spacing:.1em;text-transform:uppercase}.segment-head h2{margin:.7mm 0 0;color:#002b36;font-size:14pt}.segment-head time{color:#50666b;font-size:8pt}.option{display:grid;grid-template-columns:6mm 1fr 30mm;gap:2.5mm;padding:3mm;border-top:1px solid #dbe6e4;break-inside:avoid}.option.selected{background:#effaf8;border-left:3px solid #2aa198}.mark{width:4.5mm;height:4.5mm;border:1.5px solid #789196;border-radius:50%;margin-top:.7mm}.selected .mark{border:1.5px solid #2aa198;box-shadow:inset 0 0 0 1mm #effaf8;background:#2aa198}.airline{color:#002b36;font-weight:700}.numbers,.source{color:#61767a;font-size:7.5pt}.itinerary{margin:1.3mm 0 .7mm;color:#002b36;font:8.5pt/1.35 ui-monospace,SFMono-Regular,Menlo,monospace}.meta{color:#50666b;font-size:7.8pt}.price{text-align:right;color:#002b36;font-size:11.5pt;font-weight:700}.price small{display:block;color:#61767a;font-size:6.5pt;font-weight:400;text-transform:uppercase}.connection{break-inside:avoid;margin:-1.5mm 3mm 3mm;padding:2mm 2.5mm;border-left:2px solid #859900;background:#f7f9ef;color:#4f5d10}.connection.warn{border-color:#b58900;background:#fff9e7;color:#6c5817}.connection.danger{border-color:#dc322f;background:#fff0ef;color:#8f211f}.foot{break-inside:avoid;margin-top:4mm;padding-top:3mm;border-top:1px solid #cad9d7;color:#61767a;font-size:7.5pt}.foot strong{color:#17343b}@media print{a{color:inherit;text-decoration:none}}
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
            format!("{selected_count}/{}", trip.segments.len()),
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

    let readiness = plan.not_ready.as_deref().unwrap_or(
        "Every segment has a flight selected. Refresh live fares with Scout before booking.",
    );
    write!(
        out,
        "<div class=\"notice {}\"><strong>{}</strong> {}</div>",
        if plan.not_ready.is_some() { "" } else { "ok" },
        if plan.not_ready.is_some() {
            "Needs a decision."
        } else {
            "Ready to price."
        },
        escape(readiness)
    )
    .unwrap();
    for note in &plan.notes {
        write!(
            out,
            "<div class=\"notice\"><strong>Connection note.</strong> {}</div>",
            escape(note)
        )
        .unwrap();
    }
    out.push_str("<p class=\"page-note\">Times are local to each airport. Flight options not marked Selected are saved alternatives.</p>");

    for (index, segment) in trip.segments.iter().enumerate() {
        write!(
            out,
            "<section class=\"segment\"><div class=\"segment-head\"><div><div class=\"number\">Segment {}</div><h2>{} → {}</h2></div><time>{}</time></div>",
            segment.position,
            escape(&segment.origin),
            escape(&segment.destination),
            escape(&date(&segment.departure_date))
        )
        .unwrap();
        if segment.candidates.is_empty() {
            out.push_str("<div class=\"option\"><div></div><div><strong>No flight saved yet.</strong><div class=\"meta\">Ask Scout in chat to search this route.</div></div></div>");
        }
        let picked = selected(segment).map(|candidate| candidate.candidate);
        for candidate in &segment.candidates {
            let is_selected = picked == Some(candidate.candidate);
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
                escape(&fare(candidate)),
                if candidate.source.as_deref() == Some("ignav") {
                    "estimate when saved"
                } else {
                    "when saved"
                },
            )
            .unwrap();
        }
        out.push_str("</section>");
        if let Some(after) = trip.segments.get(index + 1) {
            let (message, tone) = connection(segment, after);
            write!(
                out,
                "<div class=\"connection {}\"><strong>Connection:</strong> {}</div>",
                tone,
                escape(&message)
            )
            .unwrap();
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
    use scout_core::trips::{Plan, Trip, TripCandidate, TripSegment};

    fn plan() -> Plan {
        Plan {
            trip: Trip {
                id: 7,
                name: "October <escape>".to_string(),
                adults: 2,
                cabin_class: Some("premium_economy".to_string()),
                status: "planning".to_string(),
                segments: vec![
                    TripSegment {
                        position: 1,
                        origin: "AMS".to_string(),
                        destination: "LIS".to_string(),
                        departure_date: "2026-10-12".to_string(),
                        candidates: vec![TripCandidate {
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
                        }],
                    },
                    TripSegment {
                        position: 2,
                        origin: "LIS".to_string(),
                        destination: "FCO".to_string(),
                        departure_date: "2026-10-12".to_string(),
                        candidates: vec![TripCandidate {
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
                        }],
                    },
                ],
                // Only a kept trip can reach here: the traveller has to see
                // a trip in their list before they can ask for its PDF.
                kept: true,
            },
            not_ready: None,
            notes: vec!["Separate tickets need extra care.".to_string()],
            chat: None,
        }
    }

    #[test]
    fn the_document_contains_every_detail_and_escapes_stored_text() {
        let html = html(&plan());
        for expected in [
            "October &lt;escape&gt;",
            "AMS → LIS → FCO",
            "2/2",
            "from €310.00",
            "premium economy",
            "KLM &amp; friends",
            "KL1579",
            "AMS 08:20 12.10 ✈ LIS 10:25 12.10",
            "1h 35m at LIS",
            "tight connection",
            "from €126.00",
            "estimate when saved",
            "Saved itinerary, not a ticket",
        ] {
            assert!(html.contains(expected), "missing `{expected}`");
        }
        assert!(!html.contains("October <escape>"));
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
