//! Sharing a trip: its link, joining it, leaving it, and taking someone
//! off it. Account-facing and async, as `trips` is; the rules live in the
//! store, under its lock.

use crate::core::{blocking, Core};
pub use crate::store::{Joined, LinkState, TripLink, LINK_ADMITS, LINK_DAYS};

/// UTC, naive: the clock every timestamp in `trip_links` is written and
/// compared in.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// The answer to an action only some people on a trip may take.
#[derive(Debug, Clone, PartialEq)]
pub enum Owned<T> {
    Done(T),
    /// No trip of that name the account reaches.
    NotFound,
    /// On it, but not the one this is for: an editor asking for the
    /// owner's controls, or the owner asking to leave.
    NotOwner,
}

/// What the join page shows before anyone signs in.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub state: LinkState,
    pub trip: String,
    pub first: Option<String>,
    pub last: Option<String>,
    /// The owner first.
    pub people: Vec<String>,
}

fn trip_id(store: &crate::store::Store, account_id: i64, name: &str) -> anyhow::Result<Option<i64>> {
    Ok(store.find_trip(account_id, name)?.map(|t| t.id))
}

/// The trip's live link, made from `fresh` if there is none. Anyone on
/// the trip may ask; `None` when the account does not reach it.
pub async fn link(core: &Core, account_id: i64, trip: &str, fresh: String) -> anyhow::Result<Option<TripLink>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(None) };
        match store.live_link(id, now())? {
            Some(live) => Ok(Some(live)),
            None => store.make_link(id, &fresh, now()).map(Some),
        }
    })
    .await
}

/// A new link in place of the old, for the owner.
pub async fn new_link(core: &Core, account_id: i64, trip: &str, fresh: String) -> anyhow::Result<Owned<TripLink>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        if !store.is_owner(account_id, id)? {
            return Ok(Owned::NotOwner);
        }
        store.make_link(id, &fresh, now()).map(Owned::Done)
    })
    .await
}

/// A new address for the trip, for the owner.
///
/// Ownership is asked here, as `new_link` asks it, although the store asks
/// again: the store's `None` means both "not the owner" and "a draft", and
/// only the first is `NotOwner`. A draft has no address to replace — the
/// page shows none on it, so offers no Replace — and is `NotFound`, the
/// answer for a trip the request cannot act on. No variant of its own:
/// nothing on the page could ask.
pub async fn new_address(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<String>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        if !store.is_owner(account_id, id)? {
            return Ok(Owned::NotOwner);
        }
        Ok(match store.new_trip_address(account_id, id)? {
            Some(address) => Owned::Done(address),
            None => Owned::NotFound,
        })
    })
    .await
}

/// Turns the link off, for the owner.
pub async fn link_off(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        Ok(if store.links_off(account_id, id, now())? { Owned::Done(()) } else { Owned::NotOwner })
    })
    .await
}

/// An editor leaving. `NotOwner` is the owner asking, who cannot leave.
pub async fn leave(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        Ok(if store.leave_trip(account_id, id, now())? { Owned::Done(()) } else { Owned::NotOwner })
    })
    .await
}

/// The owner taking someone off the trip.
pub async fn remove(core: &Core, account_id: i64, trip: &str, person: i64) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        if !store.is_owner(account_id, id)? {
            return Ok(Owned::NotOwner);
        }
        store.remove_from_trip(account_id, id, person, now())?;
        Ok(Owned::Done(()))
    })
    .await
}

/// What a link opens. `None` for a token that was never made, or whose
/// trip is gone.
pub async fn preview(core: &Core, token: &str) -> anyhow::Result<Option<Preview>> {
    let store = core.store();
    let token = token.to_string();
    blocking(move || {
        let (state, trip) = store.link_trip(&token, now())?;
        let Some(trip) = trip else { return Ok(None) };
        let people = store.trip_people(trip.id)?.into_iter().map(|p| p.name).collect();
        let last = trip
            .items
            .iter()
            .map(|i| i.ends_at.as_deref().and_then(|e| e.get(..10)).unwrap_or(&i.date).to_string())
            .max();
        Ok(Some(Preview { state, first: trip.items.first().map(|i| i.date.clone()), last, trip: trip.name, people }))
    })
    .await
}

/// What to call someone on a trip until they say: the first word of their
/// Telegram name, else what comes before the @ of their oldest address,
/// else "Traveller". Cut to 40 characters.
pub async fn suggested_name(core: &Core, account_id: i64) -> anyhow::Result<String> {
    let store = core.store();
    blocking(move || {
        let telegram = store
            .display_name_of(account_id)?
            .and_then(|n| n.split_whitespace().next().map(str::to_string));
        let name = match telegram {
            Some(name) => name,
            None => store
                .emails_of(account_id)?
                .first()
                .and_then(|e| e.split('@').next().filter(|l| !l.is_empty()).map(str::to_string))
                .unwrap_or_else(|| "Traveller".to_string()),
        };
        Ok(name.chars().take(40).collect())
    })
    .await
}

/// Joins, and tells the Telegram gate when that gave a seat.
pub async fn join(core: &Core, account_id: i64, token: &str, name: &str, rename: Option<&str>) -> anyhow::Result<Joined> {
    let founder = core.founder_account(account_id).await?;
    let owner = {
        let store = core.store();
        let token = token.to_string();
        blocking(move || Ok(store.link_trip(&token, now())?.1.map(|t| t.owner_id))).await?
    };
    let owner_name = match owner {
        Some(owner) => suggested_name(core, owner).await?,
        None => "Owner".to_string(),
    };
    let name: String = name.trim().chars().take(40).collect();
    let name = if name.is_empty() { suggested_name(core, account_id).await? } else { name };
    let store = core.store();
    let (token, rename) = (token.to_string(), rename.map(str::to_string));
    let joined = blocking(move || {
        store.join_trip(crate::store::Join {
            token: &token,
            account_id,
            name: &name,
            rename: rename.as_deref(),
            founder,
            owner_name: &owner_name,
            now: now(),
        })
    })
    .await?;
    if matches!(joined, Joined::Joined { seated: true, .. }) {
        core.note_membership_changed();
    }
    Ok(joined)
}

/// Puts `account` on the owner's trip as joining would. Test scaffolding
/// for the other crates.
#[doc(hidden)]
pub async fn share_for_tests(core: &Core, owner: i64, trip: &str, account: i64, name: &str) -> anyhow::Result<()> {
    let token = format!("{account:0>22}");
    let link = link(core, owner, trip, token).await?.ok_or_else(|| anyhow::anyhow!("no trip {trip:?}"))?;
    match join(core, account, &link.token, name, None).await? {
        Joined::Joined { .. } | Joined::Already { .. } => Ok(()),
        other => anyhow::bail!("could not share for a test: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn core() -> (Core, tempfile::TempDir, i64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sharing.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let owner = core.store().account_for_telegram(11).unwrap();
        crate::trips::seed_trip_for_tests(&core, owner, "October").await.unwrap();
        (core, dir, owner)
    }

    #[tokio::test]
    async fn the_owner_shares_someone_joins_and_the_owner_takes_them_off() {
        let (core, _dir, owner) = core().await;
        let made = link(&core, owner, "October", "a".repeat(22)).await.unwrap().unwrap();
        assert_eq!(made.token, "a".repeat(22));
        let again = link(&core, owner, "October", "b".repeat(22)).await.unwrap().unwrap();
        assert_eq!(again.token, "a".repeat(22), "the live link, not a new one");

        let masha = core.store().account_for_telegram(12).unwrap();
        let shown = preview(&core, &made.token).await.unwrap().unwrap();
        assert_eq!(shown.trip, "October");
        assert!(matches!(
            join(&core, masha, &made.token, "Masha", None).await.unwrap(),
            Joined::Joined { seated: true, .. }
        ));
        assert_eq!(remove(&core, masha, "October", owner).await.unwrap(), Owned::NotOwner);
        assert_eq!(leave(&core, owner, "October").await.unwrap(), Owned::NotOwner);
        assert_eq!(remove(&core, owner, "October", masha).await.unwrap(), Owned::Done(()));
        assert!(crate::trips::find(&core, masha, "October").await.unwrap().is_none());
        assert_eq!(new_link(&core, masha, "October", "c".repeat(22)).await.unwrap(), Owned::NotFound);
    }

    #[tokio::test]
    async fn only_the_owner_replaces_a_kept_trips_address() {
        let (core, _dir, owner) = core().await;
        assert_eq!(new_address(&core, owner, "October").await.unwrap(), Owned::NotFound, "a draft has none to replace");

        // Sharing keeps the trip, so it has an address from here on.
        let masha = core.store().account_for_telegram(12).unwrap();
        share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let old = crate::trips::find(&core, owner, "October").await.unwrap().unwrap().address.expect("a kept trip has one");
        assert_eq!(new_address(&core, masha, "October").await.unwrap(), Owned::NotOwner);
        let new = match new_address(&core, owner, "October").await.unwrap() {
            Owned::Done(address) => address,
            other => panic!("{other:?}"),
        };
        assert_ne!(new, old);
        let seen = crate::trips::find(&core, masha, "October").await.unwrap().unwrap().address;
        assert_eq!(seen.as_deref(), Some(new.as_str()), "everyone on the trip sees the new one");

        let stranger = core.store().account_for_telegram(13).unwrap();
        assert_eq!(new_address(&core, stranger, "October").await.unwrap(), Owned::NotFound);
    }

    #[tokio::test]
    async fn the_suggested_name_is_the_first_word_of_a_telegram_name_then_traveller() {
        let (core, _dir, owner) = core().await;
        core.store()
            .conn_for_tests()
            .execute("INSERT INTO users (account_id, display_name) VALUES (?, 'Sasha Petrova')", duckdb::params![owner])
            .unwrap();
        assert_eq!(suggested_name(&core, owner).await.unwrap(), "Sasha");
        let web = core.store().account_for_telegram(99).unwrap();
        assert_eq!(suggested_name(&core, web).await.unwrap(), "Traveller");
    }
}
