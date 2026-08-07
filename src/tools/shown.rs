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

/// The last set of flights shown in each chat.
#[derive(Default)]
pub struct ShownFlights {
    by_chat: DashMap<i64, (Instant, Vec<Flight>)>,
}

impl ShownFlights {
    /// Replaces whatever that chat was last shown. Only the most recent
    /// search is kept: an older one is a different question, and its
    /// prices are the ones most likely to have moved.
    pub fn remember(&self, chat_id: i64, flights: Vec<Flight>, now: Instant) {
        if flights.is_empty() {
            return;
        }
        self.by_chat.insert(chat_id, (now, flights));
    }

    /// A flight this chat was actually shown, by provider id.
    ///
    /// `None` covers both "never shown" and "shown too long ago", which the
    /// caller treats the same way: refuse rather than spend a request on
    /// an id that may have been invented.
    pub fn find(&self, chat_id: i64, offer_id: &str, now: Instant) -> Option<Flight> {
        let entry = self.by_chat.get(&chat_id)?;
        let (shown_at, flights) = entry.value();
        if now.duration_since(*shown_at) > REMEMBER_FOR {
            return None;
        }
        flights.iter().find(|f| f.offer_id == offer_id).cloned()
    }

    /// The ids this chat could legitimately ask to book, for an error
    /// message that tells the model what it should have used.
    pub fn offer_ids(&self, chat_id: i64, now: Instant) -> Vec<String> {
        self.by_chat
            .get(&chat_id)
            .filter(|e| now.duration_since(e.value().0) <= REMEMBER_FOR)
            .map(|e| e.value().1.iter().map(|f| f.offer_id.clone()).collect())
            .unwrap_or_default()
    }

    /// Drops chats whose flights have expired. Called when a search is
    /// recorded, so the map cannot grow without bound in a long-lived bot.
    pub fn evict_expired(&self, now: Instant) {
        self.by_chat
            .retain(|_, (shown_at, _)| now.duration_since(*shown_at) <= REMEMBER_FOR);
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
    fn a_new_search_replaces_the_last_one() {
        // The older search answered a different question and its prices are
        // the ones most likely to have moved.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(7, vec![flight("old", 100.0)], now);
        shown.remember(7, vec![flight("new", 200.0)], now);
        assert!(shown.find(7, "old", now).is_none());
        assert!(shown.find(7, "new", now).is_some());
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
