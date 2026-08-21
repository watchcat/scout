//! What Scout last put in front of each chat.
//!
//! Booking always happens in a *later* message than the search — "flights
//! to Hong Kong" then, a turn later, "book the Etihad one". The per-request
//! memo in [`crate::tools::budget`] is empty by then, so anything that needs
//! to know what was quoted has to remember it across turns.
//!
//! Two things need that. A booking lookup has to be able to reject an id
//! Scout never issued: the alternative is trusting a model to carry a
//! 32-character hex string across a turn, which is the invented-ASIN
//! problem in different clothing, and it costs a paid request to discover.
//! And a price can only be reported as changed if the old one was kept.

use crate::tools::duffel::Flight;
use dashmap::DashMap;
use std::time::{Duration, Instant};

/// How long a shown flight stays answerable.
///
/// Long enough to cover a conversation — measured, Ignav still resolved an
/// id twenty minutes old — and short enough that a booking lookup is not
/// offered against something from another day. A stale price is not a
/// problem here: the lookup re-checks it and says when it moved.
const REMEMBER_FOR: Duration = Duration::from_secs(45 * 60);

/// Most flights kept per chat.
///
/// A trip conversation searches a dozen routes at seven rows each, and all
/// of them have to stay bindable — this holds that comfortably while still
/// bounding a chat that never stops asking.
const MAX_PER_CHAT: usize = 200;

/// How many ids a refusal names. Enough to correct a wrong one, few enough
/// that the sentence telling the model what to do is still readable.
const IDS_IN_AN_ERROR: usize = 12;

/// One flight and when it was last put in front of the chat.
struct Seen {
    at: Instant,
    flight: Flight,
}

/// Every flight shown in each chat recently, oldest first.
#[derive(Default)]
pub struct ShownFlights {
    by_chat: DashMap<i64, Vec<Seen>>,
}

impl ShownFlights {
    /// Adds to what the chat has been shown rather than replacing it.
    ///
    /// Replacing was right while a search was followed by booking one of
    /// its own rows. Building a trip is four searches and then four
    /// bindings, and under replacement three of those came back "was not
    /// shown in this conversation" — measured in production, nine refusals
    /// in two hours, every one of them a request that should have worked.
    ///
    /// Keeping an older sighting costs nothing in honesty, because nothing
    /// downstream quotes it from here: binding stores the itinerary and
    /// re-prices at finalisation, and a booking lookup re-checks the fare
    /// and says what moved.
    pub fn remember(&self, chat_id: i64, flights: Vec<Flight>, now: Instant) {
        if flights.is_empty() {
            return;
        }
        let mut entry = self.by_chat.entry(chat_id).or_default();
        entry.retain(|seen| now.duration_since(seen.at) <= REMEMBER_FOR);
        for flight in flights {
            // A route searched twice keeps one entry, refreshed — the newer
            // sighting is the one the traveller was actually shown.
            entry.retain(|seen| seen.flight.offer_id != flight.offer_id);
            entry.push(Seen { at: now, flight });
        }
        if entry.len() > MAX_PER_CHAT {
            let excess = entry.len() - MAX_PER_CHAT;
            entry.drain(..excess);
        }
    }

    /// A flight this chat was actually shown, by provider id.
    ///
    /// `None` covers both "never shown" and "shown too long ago", which the
    /// caller treats the same way: refuse rather than spend a request on
    /// an id that may have been invented.
    pub fn find(&self, chat_id: i64, offer_id: &str, now: Instant) -> Option<Flight> {
        self.by_chat
            .get(&chat_id)?
            .iter()
            .find(|seen| {
                seen.flight.offer_id == offer_id
                    && now.duration_since(seen.at) <= REMEMBER_FOR
            })
            .map(|seen| seen.flight.clone())
    }

    /// The ids this chat could legitimately ask to book, newest first, for
    /// an error message that tells the model what it should have used.
    ///
    /// Capped: since these accumulate across a whole trip conversation,
    /// listing every one would bury the instruction under a wall of hex.
    /// The newest are the ones a confused call was most likely reaching for.
    pub fn offer_ids(&self, chat_id: i64, now: Instant) -> Vec<String> {
        self.recent_ids(chat_id, now, IDS_IN_AN_ERROR)
    }

    /// How many ids are answerable right now, whether or not they were all
    /// listed.
    pub fn remembered(&self, chat_id: i64, now: Instant) -> usize {
        self.recent_ids(chat_id, now, usize::MAX).len()
    }

    fn recent_ids(&self, chat_id: i64, now: Instant, limit: usize) -> Vec<String> {
        self.by_chat
            .get(&chat_id)
            .map(|entry| {
                entry
                    .iter()
                    .rev()
                    .filter(|seen| now.duration_since(seen.at) <= REMEMBER_FOR)
                    .take(limit)
                    .map(|seen| seen.flight.offer_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drops flights that have expired, and chats left with none. Called
    /// when a search is recorded, so the map cannot grow without bound in a
    /// long-lived bot.
    pub fn evict_expired(&self, now: Instant) {
        self.by_chat.retain(|_, seen| {
            seen.retain(|s| now.duration_since(s.at) <= REMEMBER_FOR);
            !seen.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::duffel::{PriceStatus, Source};

    fn flight(id: &str, price: f64) -> Flight {
        Flight {
            offer_id: id.to_string(),
            source: Source::Ignav,
            price_status: PriceStatus::Approximate,
            self_transfer: false,
            airline: "Etihad".to_string(),
            price,
            currency: "EUR".to_string(),
            legs: Vec::new(),
            total_minutes: None,
            total_duration: None,
            checked_bags: None,
            carry_on_bags: None,
            expires_at: None,
        }
    }

    #[test]
    fn a_flight_shown_in_one_chat_is_answerable_from_a_later_message() {
        // The whole point: the search and the booking are different turns.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("abc", 696.0)], now);

        let found = shown.find(7, "abc", now + Duration::from_secs(120)).unwrap();
        assert_eq!(found.price, 696.0);
    }

    #[test]
    fn an_id_from_another_chat_is_not_answerable() {
        // Otherwise one household member could book against another's
        // search, and /stat's per-user scoping would be the only thing
        // keeping the two apart.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("abc", 696.0)], now);
        assert!(shown.find(8, "abc", now).is_none());
    }

    #[test]
    fn an_id_nobody_was_shown_is_refused_rather_than_looked_up() {
        // A model retyping a 32-character hex string across a turn is the
        // invented-ASIN problem again, and finding out costs a paid call.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("abc", 696.0)], now);
        assert!(shown.find(7, "made-up", now).is_none());
        assert_eq!(shown.offer_ids(7, now), vec!["abc".to_string()]);
    }

    #[test]
    fn flights_stop_being_answerable_once_they_are_old() {
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("abc", 696.0)], now);
        let later = now + REMEMBER_FOR + Duration::from_secs(1);
        assert!(shown.find(7, "abc", later).is_none());
        assert!(shown.offer_ids(7, later).is_empty());
    }

    #[test]
    fn a_new_search_adds_to_what_the_chat_has_been_shown() {
        // This replaced whatever came before it, which was right when a
        // search was followed by booking one of its own rows. A trip is
        // four searches and then four bindings, and replacing meant three
        // of them came back "was not shown in this conversation" — measured
        // in production, nine times in two hours.
        //
        // Staleness is not the reason to forget: nothing here is quoted
        // from memory. A binding stores the itinerary and re-prices at
        // finalisation, and a booking lookup re-checks the fare and reports
        // what moved.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("leg-1", 100.0)], now);
        shown.remember(7, vec![flight("leg-2", 200.0)], now);
        shown.remember(7, vec![flight("leg-3", 300.0)], now);
        for id in ["leg-1", "leg-2", "leg-3"] {
            assert!(shown.find(7, id, now).is_some(), "{id} should still be bindable");
        }
    }

    #[test]
    fn seeing_the_same_flight_again_does_not_duplicate_it() {
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("same", 100.0)], now);
        shown.remember(7, vec![flight("same", 120.0)], now);
        assert_eq!(shown.offer_ids(7, now), vec!["same".to_string()]);
        assert_eq!(
            shown.find(7, "same", now).unwrap().price,
            120.0,
            "the newer sighting is the one that counts"
        );
    }

    #[test]
    fn a_chat_that_never_stops_searching_cannot_grow_without_bound() {
        let shown = ShownFlights::default();
        let now = Instant::now();
        for i in 0..(MAX_PER_CHAT + 25) {
            shown.remember(7, vec![flight(&format!("f{i}"), 1.0)], now);
        }
        assert_eq!(shown.remembered(7, now), MAX_PER_CHAT, "the store is bounded");
        assert!(shown.find(7, "f0", now).is_none(), "the oldest go first");
        assert!(shown.find(7, &format!("f{}", MAX_PER_CHAT + 24), now).is_some());

        // A refusal names a handful, not two hundred: the sentence telling
        // the model what to do has to survive the list.
        let listed = shown.offer_ids(7, now);
        assert_eq!(listed.len(), IDS_IN_AN_ERROR);
        assert_eq!(
            listed[0],
            format!("f{}", MAX_PER_CHAT + 24),
            "newest first — that is what a wrong call was reaching for"
        );
    }

    #[test]
    fn expired_chats_are_evicted_so_the_map_cannot_grow_forever() {
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(1, vec![flight("a", 1.0)], now);
        shown.remember(2, vec![flight("b", 1.0)], now + REMEMBER_FOR);

        shown.evict_expired(now + REMEMBER_FOR + Duration::from_secs(1));
        assert!(shown.find(1, "a", now).is_none(), "the old chat should be gone");
        assert!(shown.find(2, "b", now + REMEMBER_FOR).is_some(), "the recent one stays");
    }

    #[test]
    fn an_empty_result_is_not_worth_remembering() {
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("abc", 1.0)], now);
        shown.remember(7, Vec::new(), now);
        assert!(shown.find(7, "abc", now).is_some(), "a search that found nothing erases nothing");
    }
}
