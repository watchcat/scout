//! Whether a moved item works on its new day.
//!
//! The move has already happened: the page moves an item at once and this
//! is the second opinion that follows it. It is an ordinary run of the
//! main agent in the trip's own thread — the desk cannot search the web,
//! and whether a shop is open on a Wednesday is a web question — so the
//! check is recorded in the chat like anything else Scout was asked.
//!
//! What is here is everything but the run itself: the brief, the reading
//! of the reply, and the rule for what is written down. The run is handed
//! in as a closure, which is what lets the rule be tested without a model.

use crate::core::{blocking, Core};
use crate::run::RunOutcome;
use std::time::Duration;

/// How many times a check asks before it gives up on a thread that is
/// busy with another run, and how long it waits between. Two minutes in
/// all: a run in the same thread is usually the reader's own question.
pub const BUSY_TRIES: usize = 12;
pub const BUSY_PAUSE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Fine,
    Warn(String),
    /// No line that says, or one that cannot be read. Nothing is written;
    /// the reply is in the chat for anyone who looks.
    Unknown,
}

/// One item moved by the traveller, as the check needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub account_id: i64,
    pub item_id: i64,
    pub trip: String,
    pub title: String,
    pub from: String,
    pub to: String,
}

/// `prefix` off the front of `s`, whatever its case. On the original
/// string, not a lowered copy: lowering can change a string's length, and
/// the reason is cut from the model's own words.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// The verdict a reply ends with. Only the last line counts: a verdict
/// followed by more prose is the model talking about one, not giving it.
pub fn verdict_of(reply: &str) -> Verdict {
    let Some(line) = reply.lines().rev().map(str::trim).find(|line| !line.is_empty()) else {
        return Verdict::Unknown;
    };
    // A model that sets the whole line in bold or code still said it.
    let line = line.trim_matches(|c| matches!(c, '*' | '`' | '_'));
    let Some(rest) = strip_prefix_ci(line, "verdict:") else {
        return Verdict::Unknown;
    };
    let rest = rest.trim();
    if rest.trim_end_matches('.').eq_ignore_ascii_case("fine") {
        return Verdict::Fine;
    }
    let Some(reason) = strip_prefix_ci(rest, "warn") else {
        return Verdict::Unknown;
    };
    let reason = reason.trim_start_matches(|c: char| matches!(c, '-' | '—' | '–' | ':') || c.is_whitespace()).trim();
    if reason.is_empty() {
        Verdict::Unknown
    } else {
        Verdict::Warn(reason.to_string())
    }
}

/// "Wed 23 Sep 2026", with the year: the main agent is told to write the
/// year out whenever a date goes on to a tool or another agent.
fn day_label(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%a %-d %b %Y").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// What the run is asked. The first line reads as something the traveller
/// did, because the thread shows it as their turn; the rest is a system
/// note, which the transcript cuts and the model reads.
pub fn brief(title: &str, trip: &str, from: &str, to: &str) -> String {
    format!(
        "Moved {title} from {} to {}. Check it.\n\n\
         [system note] The traveller moved this item on the trip \"{trip}\" themselves; it is already moved. \
         Ask the desk what the trip holds. Say whether the item works on its new day: are they in that city \
         that day, by the flights and the stay; does it clash with anything timed; and if it names a place, \
         search for that place's opening days and hours on that date - two searches at most. \
         Do not change the trip. End with one line, exactly \"verdict: fine\" or \
         \"verdict: warn - <reason in one sentence>\".",
        day_label(from),
        day_label(to),
    )
}

/// Marks the item as being checked, so the card says so from the response
/// to the move rather than five seconds later.
pub async fn begin(core: &Core, item_id: i64) -> anyhow::Result<()> {
    let store = core.store();
    blocking(move || store.start_item_check(item_id)).await
}

/// Ends a check that never ran — no thread to run it in — so the card
/// stops saying it is being checked.
pub async fn abandon(core: &Core, m: &Move) {
    let store = core.store();
    let (item_id, to) = (m.item_id, m.to.clone());
    if let Err(e) = blocking(move || store.finish_item_check(item_id, &to, None)).await {
        tracing::warn!(error = %e, item_id, "could not end a check that never ran");
    }
}

/// Asks, reads the verdict, writes it down, and tells the phone when it is
/// a warning. `ask` is the run: it is handed the brief and answers with
/// what `run_agent` returned.
///
/// Every way out ends the check on the item, which is the one thing this
/// must not forget: a card left saying "Checking…" is a page polling for
/// an answer that is not coming.
pub async fn check<F, Fut>(core: &Core, m: &Move, mut ask: F) -> Verdict
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<RunOutcome>>,
{
    let prompt = brief(&m.title, &m.trip, &m.from, &m.to);
    let mut verdict = Verdict::Unknown;
    for attempt in 0..BUSY_TRIES {
        match ask(prompt.clone()).await {
            Ok(RunOutcome::Answered(reply)) => {
                verdict = verdict_of(&reply);
                break;
            }
            // The thread has a run in it, or every slot is taken. Both
            // pass; neither is a reason to drop the check at once.
            Ok(RunOutcome::Busy | RunOutcome::Overloaded) => {
                if attempt + 1 < BUSY_TRIES {
                    tokio::time::sleep(BUSY_PAUSE).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, item_id = m.item_id, "a move could not be checked");
                break;
            }
        }
    }
    let warning = match &verdict {
        Verdict::Warn(reason) => Some(reason.clone()),
        _ => None,
    };
    let store = core.store();
    let (item_id, to, reason) = (m.item_id, m.to.clone(), warning.clone());
    match blocking(move || store.finish_item_check(item_id, &to, reason.as_deref())).await {
        Ok(true) => {
            if let Err(e) = nudge(core, m, warning.as_deref().unwrap_or_default()).await {
                tracing::warn!(error = %e, item_id = m.item_id, "could not send a move's warning to the phone");
            }
        }
        // Fine, unknown, or about a day the item has left.
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, item_id = m.item_id, "could not record a move's verdict"),
    }
    verdict
}

/// One line to the phone, the way a booking's arrival is said. Keyed on
/// the item and the day, so the same warning about the same day — the
/// item moved away and back — is said once.
async fn nudge(core: &Core, m: &Move, reason: &str) -> anyhow::Result<bool> {
    let store = core.store();
    let body = format!("{} on {}: {reason}", m.title, day_label(&m.to));
    let key = format!("move:{}:{}", m.item_id, m.to);
    let account_id = m.account_id;
    let queued = blocking(move || {
        let Some(address) = store.delivery_address(account_id, crate::mirror::TELEGRAM)? else {
            return Ok(false);
        };
        store.enqueue_mirror(account_id, crate::mirror::TELEGRAM, &address, &body, &key, false)
    })
    .await?;
    if queued {
        core.wake_mirror();
    }
    Ok(queued)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewItem;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_verdict_is_the_last_line_and_nothing_else_is_one() {
        assert_eq!(verdict_of("Looks good.\n\nverdict: fine"), Verdict::Fine);
        assert_eq!(verdict_of("Verdict: Fine.\n\n"), Verdict::Fine);
        assert_eq!(verdict_of("**verdict: fine**"), Verdict::Fine);
        assert_eq!(
            verdict_of("The shop is shut.\nverdict: warn - APM is closed on Wednesdays"),
            Verdict::Warn("APM is closed on Wednesdays".into())
        );
        assert_eq!(verdict_of("VERDICT: WARN — You land at 15:25 that day"), Verdict::Warn("You land at 15:25 that day".into()));
        // A verdict that is not the last line is prose about a verdict.
        assert_eq!(verdict_of("verdict: fine\nActually, one more thing."), Verdict::Unknown);
        assert_eq!(verdict_of("verdict: warn"), Verdict::Unknown, "a warning with no reason is not one");
        assert_eq!(verdict_of("verdict: maybe"), Verdict::Unknown);
        assert_eq!(verdict_of("It should be fine."), Verdict::Unknown);
        assert_eq!(verdict_of(""), Verdict::Unknown);
    }

    #[test]
    fn the_brief_reads_as_what_the_traveller_did_and_tells_the_model_the_rest() {
        let text = brief("Lunch with Stanley", "Hong Kong, September", "2026-09-24", "2026-09-23");
        // The first line is what the thread shows; the note is cut from it.
        assert!(text.starts_with("Moved Lunch with Stanley from Thu 24 Sep 2026 to Wed 23 Sep 2026. Check it.\n\n[system note] "), "{text}");
        for must in [
            "on the trip \"Hong Kong, September\"",
            "it is already moved",
            "Ask the desk what the trip holds",
            "two searches at most",
            "Do not change the trip",
            "exactly \"verdict: fine\" or \"verdict: warn - <reason in one sentence>\"",
        ] {
            assert!(text.contains(must), "the brief lost {must:?}: {text}");
        }
    }

    fn core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("check.duckdb").to_str().unwrap().to_string();
        (Core::start(crate::config::Config::for_test(&p), None).unwrap(), dir)
    }

    /// A lunch on the 23rd, moved there from the 24th, with a check begun.
    async fn moved_lunch(core: &Core) -> Move {
        let store = core.store();
        let account_id = store.account_for_telegram(1).unwrap();
        store.note_delivery(account_id, crate::mirror::TELEGRAM, "4242").unwrap();
        let trip = store.upsert_trip(account_id, "Hong Kong", None, None, None).unwrap();
        let trip = store
            .add_item(trip.id, NewItem {
                kind: "activity".into(),
                title: "Lunch".into(),
                place: None,
                date: "2026-09-23".into(),
                starts_at: None,
                ends_at: None,
                notes: None,
                booked: false,
                confirmation_code: None,
                price: None,
                currency: None,
                arrival_id: None,
            })
            .unwrap();
        let m = Move {
            account_id,
            item_id: trip.items[0].id,
            trip: "Hong Kong".into(),
            title: "Lunch".into(),
            from: "2026-09-24".into(),
            to: "2026-09-23".into(),
        };
        begin(core, m.item_id).await.unwrap();
        m
    }

    fn lunch(core: &Core, m: &Move) -> crate::store::TripItem {
        let trip = core.store().find_trip(m.account_id, "Hong Kong").unwrap().unwrap();
        trip.items.into_iter().find(|i| i.id == m.item_id).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_warning_is_written_on_the_item_and_sent_to_the_phone_once() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        let answer = || async { Ok::<_, anyhow::Error>(RunOutcome::Answered("Shut.\nverdict: warn - closed on Wednesdays".into())) };
        assert_eq!(check(&core, &m, |_| answer()).await, Verdict::Warn("closed on Wednesdays".into()));
        assert_eq!(lunch(&core, &m).warning.as_deref(), Some("closed on Wednesdays"));
        assert!(!lunch(&core, &m).checking);
        // The same verdict again — the item moved away and back — says it once.
        begin(&core, m.item_id).await.unwrap();
        check(&core, &m, |_| answer()).await;
        let queued = crate::mirror::pending(&core, 10).await.unwrap();
        let lines: Vec<&str> = queued.iter().map(|row| row.body.as_str()).collect();
        assert_eq!(lines, vec!["Lunch on Wed 23 Sep 2026: closed on Wednesdays"]);
    }

    #[tokio::test(start_paused = true)]
    async fn fine_and_unreadable_write_nothing_and_still_end_the_check() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        assert_eq!(check(&core, &m, |_| async { Ok(RunOutcome::Answered("verdict: fine".into())) }).await, Verdict::Fine);
        assert_eq!((lunch(&core, &m).warning, lunch(&core, &m).checking), (None, false));
        begin(&core, m.item_id).await.unwrap();
        assert_eq!(check(&core, &m, |_| async { Ok(RunOutcome::Answered("Probably fine?".into())) }).await, Verdict::Unknown);
        assert_eq!((lunch(&core, &m).warning, lunch(&core, &m).checking), (None, false));
        begin(&core, m.item_id).await.unwrap();
        assert_eq!(check(&core, &m, |_| async { Err(anyhow::anyhow!("the model is down")) }).await, Verdict::Unknown);
        assert!(!lunch(&core, &m).checking);
        assert!(crate::mirror::pending(&core, 10).await.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_thread_is_asked_again_and_then_given_up_on() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        // Busy twice, then an answer.
        let asked = AtomicUsize::new(0);
        let verdict = check(&core, &m, |_| {
            let n = asked.fetch_add(1, Ordering::Relaxed);
            async move {
                Ok(if n < 2 { RunOutcome::Busy } else { RunOutcome::Answered("verdict: fine".into()) })
            }
        })
        .await;
        assert_eq!((verdict, asked.load(Ordering::Relaxed)), (Verdict::Fine, 3));

        // Busy for good: asked `BUSY_TRIES` times, then nothing written
        // and the card stops saying "Checking…".
        begin(&core, m.item_id).await.unwrap();
        let asked = AtomicUsize::new(0);
        let verdict = check(&core, &m, |_| {
            asked.fetch_add(1, Ordering::Relaxed);
            async { Ok(RunOutcome::Overloaded) }
        })
        .await;
        assert_eq!((verdict, asked.load(Ordering::Relaxed)), (Verdict::Unknown, BUSY_TRIES));
        assert!(!lunch(&core, &m).checking);
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_about_a_day_the_item_has_left_is_not_written() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        let store = core.store();
        let trip = store.find_trip(m.account_id, "Hong Kong").unwrap().unwrap();
        let seen = crate::store::ExpectedItem { origin: None, destination: None, title: Some("Lunch"), date: Some("2026-09-23") };
        store.move_item_checked(trip.id, 1, seen, "2026-09-26", false).unwrap();
        let verdict = check(&core, &m, |_| async { Ok(RunOutcome::Answered("verdict: warn - closed on Wednesdays".into())) }).await;
        assert_eq!(verdict, Verdict::Warn("closed on Wednesdays".into()));
        assert_eq!(lunch(&core, &m).warning, None);
        assert!(crate::mirror::pending(&core, 10).await.unwrap().is_empty(), "nothing is sent about a day it is not on");
    }
}
