//! A per-key request throttle held in memory, and the keys it counts on.
//!
//! In memory rather than in the database for the same reason sessions are:
//! this is consulted on a path a stranger controls, and the database must
//! not be. The worst case of the choice is that a deploy forgets who was
//! being noisy, which is the right shape of failure for a counter.
//!
//! What a key *is* matters as much as the counting. A limit keyed on the
//! string a stranger typed limits nothing they cannot retype, so the two
//! key functions below live here next to the counter rather than in the
//! handler: changing one without the other is how a limit stops binding.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Map size below which a sweep is not worth the walk.
///
/// A thousand dead keys is a few tens of kilobytes; sweeping to reclaim it
/// on a site that sees a handful of sign-ins an hour would be all cost.
const SWEEP_FLOOR: usize = 1024;

/// Everything behind the one mutex.
struct Counts {
    hits: HashMap<String, Vec<Instant>>,
    /// Map size at which the next full sweep happens. Grown to twice the
    /// live set after each sweep, so sweeping is amortised O(1) per
    /// request while the map stays within a constant factor of what is
    /// actually still inside its window.
    sweep_at: usize,
    /// Only a test reads this — it is how "the map is not walked on every
    /// call" is asserted without timing anything.
    #[cfg(test)]
    sweeps: usize,
}

impl Counts {
    #[cfg(test)]
    fn note_sweep(&mut self) {
        self.sweeps += 1;
    }
    #[cfg(not(test))]
    fn note_sweep(&mut self) {}
}

pub struct Limiter {
    quota: usize,
    window: Duration,
    counts: Mutex<Counts>,
}

impl Limiter {
    pub fn new(quota: usize, window: Duration) -> Self {
        Self {
            quota,
            window,
            counts: Mutex::new(Counts {
                hits: HashMap::new(),
                sweep_at: SWEEP_FLOOR,
                #[cfg(test)]
                sweeps: 0,
            }),
        }
    }

    /// True when this key may proceed, counting the attempt.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        // A poisoned lock means another thread panicked mid-update. The
        // stake is a rate counter, so recovering and carrying on beats
        // taking the sign-in page down.
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());

        // This key's own expired hits, which is what makes the window let
        // go. Bounded by the quota, unlike the whole map.
        let hits = counts.hits.entry(key.to_string()).or_default();
        hits.retain(|t| now.duration_since(*t) < self.window);
        let allowed = hits.len() < self.quota;
        if allowed {
            hits.push(now);
        }

        // Every other key's, occasionally. This used to happen on every
        // call: an attacker who never repeats a key grows the map, and the
        // walk over it happened once per request, under a `std::sync::Mutex`
        // held on a tokio worker thread — O(n) per request with n chosen by
        // the attacker, which is a lock convoy on the sign-in path rather
        // than a defence of it. Doing it on a size threshold keeps the
        // memory bound the sweep was for (never sweeping is not an option)
        // and takes the walk off the common path.
        if counts.hits.len() >= counts.sweep_at {
            self.sweep(&mut counts, now);
        }
        allowed
    }

    fn sweep(&self, counts: &mut Counts, now: Instant) {
        counts.hits.retain(|_, hits| {
            hits.retain(|t| now.duration_since(*t) < self.window);
            !hits.is_empty()
        });
        counts.sweep_at = counts.hits.len().saturating_mul(2).max(SWEEP_FLOOR);
        counts.note_sweep();
    }

    /// How many keys are being tracked. For asserting the memory bound.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.counts.lock().unwrap().hits.len()
    }

    /// How many full sweeps have happened.
    #[cfg(test)]
    fn sweeps(&self) -> usize {
        self.counts.lock().unwrap().sweeps
    }
}

/// Domains that ignore dots in the local part.
///
/// Deliberately a short list rather than a rule. Gmail treats
/// `v.i.c.t.i.m@` and `victim@` as one mailbox; essentially nobody else
/// does, and RFC 5321 says the local part belongs to the receiving host.
/// Stripping dots everywhere would put two different people's addresses in
/// one bucket at every provider that takes them literally, and the cost of
/// that lands on the honest one of the two.
///
/// Google Workspace domains ignore dots as well and cannot be recognised
/// from here, so a mail bomb spelled with dots at a Workspace domain still
/// gets one bucket per spelling. That residual is accepted: the alternative
/// bound is guessing, and the `+tag` strip below still covers the easier
/// half of the same trick there.
const DOT_INSENSITIVE: [&str; 2] = ["gmail.com", "googlemail.com"];

/// The rate-limit key for an address: as close as we can cheaply get to
/// "the inbox this would land in".
///
/// Only ever a key. The address that gets stored as an identity and the
/// address the message is sent to are the one the visitor typed — two
/// spellings of one Gmail account are one account to Google and must stay
/// two distinct identities to us if that is how they were entered.
pub fn address_key(address: &str) -> String {
    let lowered = address.trim().to_lowercase();
    // `rsplit_once`: the domain is whatever follows the last `@`.
    let Some((local, domain)) = lowered.rsplit_once('@') else {
        // Not an address shape at all. `deliverable` refuses these before
        // they reach here; keying on the whole string is the safe answer
        // if that ever stops being true.
        return lowered;
    };

    // `+tag` is stripped at every domain, not just the ones known to
    // support it. A provider that treats `+` literally gets one honest
    // user sharing a bucket with themselves, which costs them a slower
    // second sign-in; leaving it in gives an attacker an unlimited supply
    // of keys for one inbox, which is the finding.
    let tagless = local.split_once('+').map_or(local, |(before, _)| before);
    // A local part that is *only* a tag is not a tagged anything.
    let local = if tagless.is_empty() { local } else { tagless };

    if DOT_INSENSITIVE.contains(&domain) {
        format!("{}@{domain}", local.replace('.', ""))
    } else {
        format!("{local}@{domain}")
    }
}

/// The rate-limit key for a client address, or `None` when the string is
/// not an address.
///
/// IPv6 is keyed by its /64. The smallest thing an IPv6 client is
/// ordinarily given is a /64, and many are handed a /56 or /48, so keying
/// on the full /128 hands one ordinary client 18 quintillion buckets and
/// the per-IP limit stops meaning anything at all. IPv4 is keyed whole: a
/// /24 there is a real neighbourhood of unrelated people.
pub fn ip_key(ip: &str) -> Option<String> {
    // A proxy that writes a bracketed literal, as in `[::1]`.
    let trimmed = ip.trim().trim_start_matches('[').trim_end_matches(']');
    match trimmed.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => Some(v4.to_string()),
        // `::ffff:1.2.3.4` is an IPv4 client wearing an IPv6 spelling.
        // Taking its /64 would file every such client under one prefix of
        // zeroes — a single bucket for a whole address family, which is
        // the opposite mistake and worse.
        IpAddr::V6(v6) => Some(match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_key_is_allowed_its_quota_and_then_refused() {
        let l = Limiter::new(3, Duration::from_secs(900));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(!l.allow("a@example.com"), "a fourth request in the window was allowed");
        // One noisy address must not lock out everyone else.
        assert!(l.allow("b@example.com"));
    }

    #[test]
    fn the_window_lets_go() {
        let l = Limiter::new(1, Duration::from_millis(50));
        assert!(l.allow("k"));
        assert!(!l.allow("k"));
        std::thread::sleep(Duration::from_millis(60));
        assert!(l.allow("k"), "the window never expired");
    }

    #[test]
    fn one_inbox_is_one_key_however_it_is_spelled() {
        // The three spellings the reviewer used. All three reach one
        // mailbox, and all three were separate buckets.
        let victim = address_key("victim@gmail.com");
        assert_eq!(address_key("victim+1@gmail.com"), victim);
        assert_eq!(address_key("VICTIM+anything@Gmail.com"), victim);
        assert_eq!(address_key("v.i.c.t.i.m@gmail.com"), victim);
        assert_eq!(address_key("v.i.c.t.i.m+2@googlemail.com"), "victim@googlemail.com");

        // And the conflations that would be wrong. Dots are only ignored
        // where the provider ignores them; two people at one company are
        // not one bucket because their names rhyme.
        assert_ne!(address_key("a.b@example.com"), address_key("ab@example.com"));
        assert_ne!(address_key("ada@example.com"), address_key("ada@example.org"));
        // `+tag` is stripped everywhere, though.
        assert_eq!(address_key("ada+news@example.com"), "ada@example.com");
        // A local part that is nothing but a tag keeps it, rather than
        // collapsing every such address at a domain into one bucket.
        assert_eq!(address_key("+ada@example.com"), "+ada@example.com");
    }

    #[test]
    fn an_ipv6_client_gets_one_bucket_and_not_a_sixty_four_of_them() {
        let prefix = ip_key("2001:db8:abcd:1::1").unwrap();
        assert_eq!(ip_key("2001:db8:abcd:1::2").unwrap(), prefix);
        assert_eq!(ip_key("2001:db8:abcd:1:ffff:ffff:ffff:ffff").unwrap(), prefix);
        assert_eq!(ip_key("[2001:db8:abcd:1::3]").unwrap(), prefix);
        // A different /64 is a different client.
        assert_ne!(ip_key("2001:db8:abcd:2::1").unwrap(), prefix);

        // IPv4 is keyed whole: a /24 is unrelated people.
        assert_eq!(ip_key("203.0.113.7").unwrap(), "203.0.113.7");
        assert_ne!(ip_key("203.0.113.8").unwrap(), ip_key("203.0.113.7").unwrap());
        // An IPv4 client in IPv6 clothing is an IPv4 client, not the
        // whole of `::/64`.
        assert_eq!(ip_key("::ffff:203.0.113.7").unwrap(), "203.0.113.7");

        // Not an address. The caller decides what to do with that; what it
        // must not do is become a bucket of its own, or a header full of
        // junk buys one bucket per junk string.
        assert!(ip_key("not-an-ip").is_none());
        assert!(ip_key("").is_none());
    }

    #[test]
    fn the_map_is_not_walked_on_every_call() {
        // The whole point of the amortisation. Under the old code this was
        // one full walk of the map per call, with the map's size chosen by
        // whoever was flooding it.
        let l = Limiter::new(1, Duration::from_secs(3600));
        for i in 0..5000 {
            l.allow(&format!("k{i}"));
        }
        assert!(l.sweeps() <= 8, "5000 calls swept the map {} times", l.sweeps());
    }

    #[test]
    fn the_map_does_not_grow_without_bound() {
        // The other half: amortised must still mean bounded. An attacker
        // who never repeats a key must not be able to keep every key they
        // have ever sent.
        let l = Limiter::new(1, Duration::from_millis(50));
        for i in 0..3000 {
            l.allow(&format!("old{i}"));
        }
        std::thread::sleep(Duration::from_millis(60));
        for i in 0..3000 {
            l.allow(&format!("new{i}"));
        }
        assert!(
            l.tracked() < 6000,
            "every key ever seen is still held: {} of 6000",
            l.tracked()
        );
    }
}
