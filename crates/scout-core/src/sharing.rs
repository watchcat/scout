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

/// What choosing a trip's address came to, for the owner's form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressChoice {
    /// Free (from a check) or now the trip's (from a set), as normalised.
    Free(String),
    /// A rule broken, with the sentence for the form.
    Invalid(String),
    /// Another trip's address, or somebody's personal one.
    Taken,
    NotFound,
    NotOwner,
}

/// The owner's trip by name, for the two address calls: `Err` carries the
/// answer when it is not one the owner can change.
fn owned_trip(store: &crate::store::Store, account_id: i64, trip: &str) -> anyhow::Result<Result<i64, AddressChoice>> {
    let Some(id) = trip_id(store, account_id, trip)? else { return Ok(Err(AddressChoice::NotFound)) };
    if !store.is_owner(account_id, id)? {
        return Ok(Err(AddressChoice::NotOwner));
    }
    Ok(Ok(id))
}

/// Whether the owner could give the trip this address right now, for a
/// form that asks as they type. Nothing is written.
pub async fn check_address(core: &Core, account_id: i64, trip: &str, raw: &str) -> anyhow::Result<AddressChoice> {
    let address = match crate::inbox::check_trip_address(raw) {
        Ok(a) => a,
        Err(why) => return Ok(AddressChoice::Invalid(why)),
    };
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let id = match owned_trip(&store, account_id, &trip)? {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        Ok(if store.trip_address_free(id, &address)? { AddressChoice::Free(address) } else { AddressChoice::Taken })
    })
    .await
}

/// Gives the trip the address its owner chose. The old one then belongs
/// to nobody, and mail sent to it stops arriving — the page asks first.
pub async fn set_address(core: &Core, account_id: i64, trip: &str, raw: &str) -> anyhow::Result<AddressChoice> {
    let address = match crate::inbox::check_trip_address(raw) {
        Ok(a) => a,
        Err(why) => return Ok(AddressChoice::Invalid(why)),
    };
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let id = match owned_trip(&store, account_id, &trip)? {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        // The store asks ownership again, under the lock that writes.
        Ok(match store.set_trip_address(account_id, id, &address)? {
            crate::store::AddressClaim::Set => AddressChoice::Free(address),
            crate::store::AddressClaim::Taken => AddressChoice::Taken,
            crate::store::AddressClaim::NotOwner => AddressChoice::NotOwner,
            crate::store::AddressClaim::NotFound => AddressChoice::NotFound,
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

/// The accounts on a trip, the owner first and the editors after, as
/// `Store::trip_people` orders them. Empty for a trip that is gone. The
/// inbox worker reads this to forward and announce a trip's mail to
/// everyone on it rather than to the owner alone.
pub async fn people_of(core: &Core, trip_id: i64) -> anyhow::Result<Vec<i64>> {
    let store = core.store();
    blocking(move || Ok(store.trip_people(trip_id)?.into_iter().map(|p| p.account_id).collect())).await
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
    async fn only_the_owner_chooses_a_kept_trips_address_and_it_must_be_free() {
        let (core, _dir, owner) = core().await;
        assert_eq!(set_address(&core, owner, "October", "october2026").await.unwrap(), AddressChoice::NotFound, "a draft has none");

        // Sharing keeps the trip, so it has an address from here on.
        let masha = core.store().account_for_telegram(12).unwrap();
        share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        assert_eq!(set_address(&core, masha, "October", "october2026").await.unwrap(), AddressChoice::NotOwner);
        assert_eq!(check_address(&core, masha, "October", "october2026").await.unwrap(), AddressChoice::NotOwner);
        assert_eq!(
            check_address(&core, owner, "October", "Oct_2026").await.unwrap(),
            AddressChoice::Invalid("letters, digits, dots and dashes only".into())
        );
        assert_eq!(check_address(&core, owner, "October", " October2026 ").await.unwrap(), AddressChoice::Free("october2026".into()));
        assert_eq!(set_address(&core, owner, "October", "October2026").await.unwrap(), AddressChoice::Free("october2026".into()));
        let seen = crate::trips::find(&core, masha, "October").await.unwrap().unwrap().address;
        assert_eq!(seen.as_deref(), Some("october2026"), "everyone on the trip sees the new one");
        assert_eq!(
            crate::inbox::check_handle(&core, masha, "october2026").await.unwrap(),
            crate::inbox::Claim::Taken,
            "a trip's address is not free as a personal one"
        );

        // Somebody's personal address is taken, and so is another trip's.
        crate::inbox::set_handle(&core, masha, "masha").await.unwrap();
        assert_eq!(check_address(&core, owner, "October", "masha").await.unwrap(), AddressChoice::Taken);
        crate::trips::seed_trip_for_tests(&core, owner, "Porto").await.unwrap();
        crate::trips::keep(&core, owner, "Porto").await.unwrap();
        assert_eq!(set_address(&core, owner, "Porto", "october2026").await.unwrap(), AddressChoice::Taken);

        let stranger = core.store().account_for_telegram(13).unwrap();
        assert_eq!(set_address(&core, stranger, "October", "x2026x").await.unwrap(), AddressChoice::NotFound);
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
