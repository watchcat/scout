use crate::agent::AgentDeps;
use crate::config::Config;
use crate::store::Store;
use crate::tools::kagi::{KagiClient, KAGI_API_BASE};
use std::collections::HashSet;
use std::time::Duration;

// The payload of `GET /v1/deliveries` once there is one, so it lives with
// the rest of what both sides speak rather than here.
use scout_api::DueDelivery;

/// Everything answering a question needs, and nothing about how the question
/// arrived.
///
/// In phase 2b-2 this is what the HTTP service owns. Nothing here may learn
/// what a `ChatId` is.
pub struct Core {
    pub(crate) cfg: Config,
    pub(crate) deps: AgentDeps,
}

/// What a reorder reminder says, wherever it is delivered.
///
/// Named rather than written inline at the one call site, because "every
/// channel says the same sentence" is the claim the whole payload design
/// rests on, and an anonymous literal is one careless edit from breaking it
/// quietly.
pub fn reminder_text(item: &str) -> String {
    format!("⏰ Time to reorder {item} — want me to search for deals?")
}

/// Who may talk to this bot, for the start-up log.
pub struct Population {
    pub founders: usize,
    pub admins: usize,
    pub daily_cap: i64,
}

/// Whether someone arriving right now can get in.
///
/// Deliberately not a count. How many seats are left is nobody's business
/// outside this process; whether it is worth turning up is everybody's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A round is open and has room. `join_url` is `None` only when the bot
    /// could not read its own address at start-up, which disables the link
    /// rather than sending anyone somewhere that is not ours.
    Open { join_url: Option<String> },
    /// Full, closed, or no round at all. One answer on purpose: which it is
    /// tells a stranger about rounds they were not invited to.
    Full,
}

/// Founders are the people paying for the bot, named in the environment by
/// Telegram id because that is the only name they had when the list was
/// written. Exempt from the daily cap.
pub fn is_founder(founders: &HashSet<i64>, telegram_id: i64) -> bool {
    founders.contains(&telegram_id)
}

/// Admin rights are likewise granted by Telegram id in the environment.
pub fn is_admin_id(admins: &HashSet<i64>, telegram_id: i64) -> bool {
    admins.contains(&telegram_id)
}

impl Core {
    /// Opens the database and builds every client the agent can use.
    ///
    /// `return_url` is the bot's own address, from `getMe`. Two things read
    /// it: Duffel Links, as where to send a traveller after checkout, and
    /// `admission`, as the base of the join link a stranger is offered. They
    /// agree today only because the bot's own chat is the one address Scout
    /// owns — that is a coincidence the type does not enforce, and it is why
    /// this wants to become a named concept rather than a borrowed field. Taking it here is a deliberate
    /// simplification while there is one channel: it is really a property of
    /// the channel, not of the process, and a core serving two channels has
    /// two return addresses where a constructor argument holds one. The path
    /// out is already there — `build_agent` is built per incoming message
    /// and reads the field at that point — so this moves to the request when
    /// a second channel arrives.
    pub fn start(cfg: Config, return_url: Option<String>) -> anyhow::Result<Self> {
        let store = Store::open(&cfg.db_path)?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let kagi = KagiClient::new(http.clone(), cfg.kagi_api_key.clone(), KAGI_API_BASE.to_string());
        let llm = crate::agent::llm_client(&cfg.minimax_api_key)?;
        let ebay = cfg.ebay_credentials.clone().map(|(id, secret)| {
            crate::tools::ebay::EbayClient::new(
                http.clone(),
                id,
                secret,
                cfg.ebay_marketplace.clone(),
                crate::tools::ebay::EBAY_API_BASE.to_string(),
            )
        });
        tracing::info!(ebay_api = ebay.is_some(), "eBay Browse API verification");
        let marktplaats = crate::tools::marktplaats::MarktplaatsClient::new(
            http.clone(),
            crate::tools::marktplaats::MARKTPLAATS_BASE.to_string(),
        );

        let renderer = crate::tools::browser::find_chrome().map(crate::tools::browser::Renderer::new);
        tracing::info!(headless_browser = renderer.is_some(), "fallback renderer");

        let bol = cfg.bol_credentials.clone().map(|(id, secret)| {
            crate::tools::bol::BolClient::new(
                http.clone(),
                id,
                secret,
                cfg.bol_country.clone(),
                crate::tools::bol::BOL_API_BASE.to_string(),
                crate::tools::bol::BOL_LOGIN_BASE.to_string(),
            )
        });
        tracing::info!(bol_api = bol.is_some(), "bol.com Catalog API");

        let perplexity = cfg.perplexity_api_key.clone().map(|key| {
            crate::tools::perplexity::PerplexityClient::new(
                http.clone(),
                key,
                crate::tools::perplexity::PERPLEXITY_API_BASE.to_string(),
            )
        });
        tracing::info!(
            perplexity = perplexity.is_some(),
            "second search engine (Kagi always on)"
        );

        let duffel = cfg.duffel_api_key.clone().map(|key| {
            crate::tools::duffel::DuffelClient::new(
                http.clone(),
                key,
                crate::tools::duffel::DUFFEL_API_BASE.to_string(),
            )
            .with_markup(cfg.duffel_markup_rate)
        });
        tracing::info!(
            duffel_api = duffel.is_some(),
            markup_rate = cfg.duffel_markup_rate,
            links = cfg.duffel_links_enabled,
            ignav = cfg.ignav_api_key.is_some(),
            "flight search"
        );

        let http_for_ignav = http.clone();
        let deps = AgentDeps {
            llm,
            kagi,
            renderer,
            bol,
            perplexity,
            http,
            ebay,
            duffel,
            marktplaats,
            store,
            secondhand_sites: cfg.secondhand_sites.clone(),
            return_url,
            links_enabled: cfg.duffel_links_enabled,
            shown: std::sync::Arc::new(crate::tools::shown::ShownFlights::default()),
            ignav: cfg.ignav_api_key.clone().map(|key| {
                crate::tools::ignav::IgnavClient::new(
                    http_for_ignav,
                    key,
                    crate::tools::ignav::IGNAV_API_BASE.to_string(),
                )
            }),
        };

        Ok(Self { cfg, deps })
    }

    /// Everyone currently admitted, as Telegram ids, for the adapter's gate.
    ///
    /// A read at start-up and nothing more: the gate runs on every update,
    /// including from strangers, so it must not touch the database.
    pub fn members(&self) -> anyhow::Result<Vec<i64>> {
        self.deps.store.active_members()
    }

    /// For the start-up log line.
    pub fn schema_version(&self) -> anyhow::Result<i64> {
        self.deps.store.schema_version()
    }

    /// Founders, admins and the cap, for the same log line.
    pub fn population(&self) -> Population {
        Population {
            founders: self.cfg.allowed_user_ids.len(),
            admins: self.cfg.admin_user_ids.len(),
            daily_cap: self.cfg.invite_daily_requests,
        }
    }

    pub fn is_founder(&self, telegram_id: i64) -> bool {
        is_founder(&self.cfg.allowed_user_ids, telegram_id)
    }

    pub fn is_admin(&self, telegram_id: i64) -> bool {
        is_admin_id(&self.cfg.admin_user_ids, telegram_id)
    }

    /// A handle on the database, for this crate only. `store` is private, so
    /// a return type of `Store` is itself what keeps this in here.
    pub(crate) fn store(&self) -> crate::store::Store {
        self.deps.store.clone()
    }

    /// Everything due to be delivered on one channel today.
    ///
    /// Which day it is is decided here rather than passed in, because a
    /// channel that chose the date could quietly shift everyone's cadence.
    /// Rows core would refuse to act on are logged and left where they are:
    /// skipping one is not an error worth abandoning the tick for.
    pub async fn due_deliveries(&self, channel: &str) -> anyhow::Result<Vec<DueDelivery>> {
        let store = self.store();
        let channel = channel.to_string();
        let today = chrono::Local::now().date_naive();
        blocking(move || {
            let due = store.due_reminders(&today.to_string())?;
            Ok(due
                .into_iter()
                .filter(|r| r.channel == channel)
                .filter(|r| crate::schedule::next_date(r, today).is_some())
                .map(|r| DueDelivery {
                    id: r.id,
                    channel: r.channel,
                    address: r.address,
                    text: reminder_text(&r.item),
                })
                .collect())
        })
        .await
    }

    /// The delivery arrived: move the reminder on to its next date.
    ///
    /// A channel that does not call this leaves the row untouched, which is
    /// how a failed send retries on the next tick. Advancing from the date
    /// the row already held rather than from today keeps the cadence
    /// anchored to the original date, and makes a second acknowledgement
    /// harmless: once the row is no longer due there is nothing to find.
    ///
    /// Takes the channel as well as the id because ids are sequential and
    /// mean nothing on their own: without it, one channel could acknowledge
    /// another's reminder, the row would advance, and the person who was
    /// actually waiting would simply not be reminded — with no error
    /// anywhere, because a successful advance looks the same either way.
    /// Over HTTP that is an id no one has to guess.
    ///
    /// Two costs come from an acknowledgement that carries nothing else, and
    /// both are deliberate, because a channel that has sent a message knows
    /// its own name and the id it was given and nothing more. It re-reads
    /// the due rows to find the one being acknowledged, so a tick delivering
    /// n reminders runs 2n+1 queries where one process ran n+1; that is
    /// cheap at this table's size and would not be over a network.
    /// And it works out today for itself, so a tick that spans local midnight
    /// and lands exactly on an interval boundary can push a reminder one
    /// interval further than intended — a single skipped reorder prompt, at
    /// most once a night, in exchange for an acknowledgement that needs no
    /// clock agreement between the two sides.
    pub async fn delivery_done(&self, channel: &str, id: i64) -> anyhow::Result<()> {
        let store = self.store();
        let channel = channel.to_string();
        let today = chrono::Local::now().date_naive();
        blocking(move || {
            let Some(reminder) = store
                .due_reminders(&today.to_string())?
                .into_iter()
                .find(|r| r.id == id && r.channel == channel)
            else {
                return Ok(());
            };
            let Some(next_due) = crate::schedule::next_date(&reminder, today) else {
                return Ok(());
            };
            store.set_next_due(id, &next_due.to_string())
        })
        .await
    }

    /// Records a request against the caller's account.
    ///
    /// Takes a Telegram id and resolves it, so the caller never has to know
    /// that the two are different numbers — the mistake that made /stat
    /// report an empty week.
    pub async fn log_request(&self, telegram_id: i64, kind: &'static str) -> anyhow::Result<()> {
        let store = self.store();
        blocking(move || {
            let account_id = store.account_for_telegram(telegram_id)?;
            store.log_request(account_id, kind)
        })
        .await
    }

    /// Remembers what to call someone, under their account.
    pub async fn note_display_name(&self, telegram_id: i64, name: String) -> anyhow::Result<()> {
        let store = self.store();
        blocking(move || {
            let account_id = store.account_for_telegram(telegram_id)?;
            store.remember_user(account_id, &name)
        })
        .await
    }

    /// Records where someone last spoke, so an announcement can reach them.
    pub async fn note_address(
        &self,
        telegram_id: i64,
        channel: &'static str,
        address: String,
    ) -> anyhow::Result<()> {
        let store = self.store();
        blocking(move || {
            let account_id = store.account_for_telegram(telegram_id)?;
            store.note_delivery(account_id, channel, &address)
        })
        .await
    }

    /// Accounts with no recorded name, as (account, telegram id), for a
    /// channel that can go and ask.
    pub async fn accounts_missing_names(&self, limit: usize) -> anyhow::Result<Vec<(i64, i64)>> {
        let store = self.store();
        blocking(move || store.accounts_missing_display_names(limit)).await
    }

    /// Files a name a channel looked up, under the account it belongs to.
    pub async fn record_name(&self, account_id: i64, name: String) -> anyhow::Result<()> {
        let store = self.store();
        blocking(move || store.remember_user(account_id, &name)).await
    }

    /// Takes a backup now and prunes old ones. Returns where it went.
    ///
    /// Runs off the async runtime: the copy holds the store's mutex for its
    /// duration, and that mutex is shared with the agent.
    pub async fn backup(&self, reason: crate::backup::Reason) -> anyhow::Result<std::path::PathBuf> {
        let store = self.store();
        let db = std::path::PathBuf::from(&self.cfg.db_path);
        blocking(move || {
            let dir = crate::backup::dir_for(&db);
            std::fs::create_dir_all(&dir)?;
            let to = dir.join(crate::backup::file_name_now(reason));
            store.backup_to(&to)?;
            crate::backup::prune(&dir, crate::backup::KEEP)?;
            Ok(to)
        })
        .await
    }

    /// Housekeeping that has to happen whether or not a channel is running.
    ///
    /// Spawned by whoever owns the process — `main` today, the core binary
    /// once core has one of its own. Deliberately not part of the Telegram
    /// scheduler: a backup should not depend on a chat client existing.
    pub async fn run_maintenance(self: std::sync::Arc<Self>) {
        // Hourly checks against a daily threshold, so a restart cannot skip a
        // day and the check itself costs a stat call.
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let dir = crate::backup::dir_for(std::path::Path::new(&self.cfg.db_path));
        loop {
            ticker.tick().await;
            match crate::backup::is_due(&dir) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "could not tell whether a backup is due");
                    continue;
                }
            }
            // A failed backup must never stop the bot answering. The weakness
            // of that choice is that nobody reads logs, which is how backups
            // usually fail — hence the shouting.
            match self.backup(crate::backup::Reason::Nightly).await {
                Ok(to) => tracing::info!(path = %to.display(), "nightly backup"),
                Err(e) => tracing::error!(error = %e, "NIGHTLY BACKUP FAILED"),
            }
        }
    }

    /// Turns a photo into a search description.
    ///
    /// Wrapped so the adapter never reaches for the model itself: which
    /// model, and how it is asked, is core's business. The adapter's job
    /// stops at getting the bytes.
    pub async fn describe_photo(
        &self,
        bytes: &[u8],
        caption: Option<&str>,
    ) -> anyhow::Result<String> {
        crate::vision::describe_photo(&self.deps.llm, bytes, caption).await
    }

    /// Who may walk in, as the public page reports it.
    ///
    /// Reads the database. Callers on a request path must cache it — see
    /// `scout-web`, where a public endpoint that took the store's mutex
    /// would be a way for a stranger to slow the agent down.
    ///
    /// "Room" here is the same count `claim_seat` uses, revoked members
    /// included. That is what stops the page from lying: it can never say
    /// open where a real attempt to join would be turned away.
    pub async fn admission(&self) -> anyhow::Result<Admission> {
        let store = self.store();
        let return_url = self.deps.return_url.clone();
        blocking(move || {
            // `rounds()` comes back oldest first, so searching from the back
            // finds the newest round that will still take someone.
            let newest_with_room = store
                .rounds()?
                .into_iter()
                .rfind(|r| r.open && r.used < r.capacity);
            Ok(match newest_with_room {
                Some(r) => Admission::Open {
                    join_url: return_url.map(|u| format!("{u}?start={}", r.code)),
                },
                None => Admission::Full,
            })
        })
        .await
    }
}

/// Runs a blocking store call off the async runtime.
///
/// Shared by every module that touches DuckDB: the connection sits behind a
/// mutex, so holding it across an await would block the runtime rather than
/// just the caller.
pub(crate) async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Claim;

    #[test]
    fn a_founder_is_exempt_from_the_daily_cap_and_a_member_is_not() {
        // Authorization is core's business: the adapter's gate is a cache in
        // front of this, never the decision itself.
        let founders: HashSet<i64> = [11_i64].into_iter().collect();
        assert!(is_founder(&founders, 11));
        assert!(!is_founder(&founders, 22));
    }

    #[test]
    fn an_admin_is_named_by_telegram_id_because_that_is_what_env_holds() {
        let admins: HashSet<i64> = [99_i64].into_iter().collect();
        assert!(is_admin_id(&admins, 99));
        assert!(!is_admin_id(&admins, 11));
    }

    #[test]
    fn a_core_opens_its_own_database_from_config_alone() {
        // This pins the constructor's contract: it opens the file itself,
        // from config, with no handle passed in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("start.duckdb");
        let cfg = Config::for_test(path.to_str().unwrap());

        let core = Core::start(cfg, None).unwrap();

        assert_eq!(core.members().unwrap(), Vec::<i64>::new());
        assert!(core.schema_version().unwrap() >= 5);

        // The rest of what `start` does is wire clients from config, and the
        // way that breaks is a field read from the wrong key. With none of
        // the optional keys set, every optional integration must come out as
        // nothing rather than as a client holding an empty credential — a
        // state production runs in too. Reaching into `deps` is a privilege
        // only a test inside this crate has.
        assert!(core.deps.duffel.is_none());
        assert!(core.deps.perplexity.is_none());
        assert!(!core.deps.links_enabled);
        // Not from config at all: whatever the caller passed, unchanged.
        assert!(core.deps.return_url.is_none());
    }

    #[tokio::test]
    async fn a_due_reminder_arrives_addressed_and_worded_ready_to_send() {
        // The channel is told where to send and what to say. It is not told
        // the interval, the next date, or anything else it would have to do
        // arithmetic on — that stays here, so a second channel cannot get
        // the cadence subtly wrong.
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(
            Config::for_test(dir.path().join("due.duckdb").to_str().unwrap()),
            None,
        )
        .unwrap();

        // The test is inside scout-core, so it may use the store directly —
        // that is precisely the privilege the adapter no longer has.
        let store = core.store();
        let account = store.account_for_telegram(4242).unwrap();
        store
            .create_reminder(account, "telegram", "4242", "detergent", 30, "2020-01-01")
            .unwrap();

        let due = core.due_deliveries("telegram").await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].address, "4242");
        assert!(due[0].text.contains("detergent"), "got: {}", due[0].text);

        core.delivery_done("telegram", due[0].id).await.unwrap();
        assert!(core.due_deliveries("telegram").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_channel_cannot_acknowledge_another_channels_reminder() {
        // Ids are sequential, so an acknowledgement carrying only an id is a
        // number anyone can guess. Acking the wrong one would advance a row
        // nobody delivered, and the person waiting would just never hear —
        // silently, since an advance looks identical whoever asked for it.
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(
            Config::for_test(dir.path().join("cross.duckdb").to_str().unwrap()),
            None,
        )
        .unwrap();

        let store = core.store();
        let account = store.account_for_telegram(4242).unwrap();
        store
            .create_reminder(account, "telegram", "4242", "detergent", 30, "2020-01-01")
            .unwrap();

        let due = core.due_deliveries("telegram").await.unwrap();
        assert_eq!(due.len(), 1);

        core.delivery_done("web", due[0].id).await.unwrap();

        assert_eq!(
            core.due_deliveries("telegram").await.unwrap(),
            due,
            "a web acknowledgement moved a telegram reminder"
        );
    }

    #[tokio::test]
    async fn the_newest_round_with_room_is_the_one_a_stranger_can_join() {
        // Several rounds can be open at once — nothing closes the old one
        // when a new one opens. The page has room for one answer, so core
        // picks: the newest round that is open and not yet full.
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(
            Config::for_test(dir.path().join("doors.duckdb").to_str().unwrap()),
            Some("https://t.me/scoutbot".to_string()),
        )
        .unwrap();
        let store = core.store();

        assert_eq!(core.admission().await.unwrap(), Admission::Full,
            "no rounds at all is not an invitation");

        // Created back to back. `rounds()` orders by created_at then code,
        // so if these ever landed in the same microsecond the tiebreak would
        // put `autumn` first and "newest" would resolve to `spring` — which
        // is the wrong answer and fails this test rather than passing it
        // quietly. The safe direction, but worth knowing it rests on that.
        store.create_round("spring", 1).unwrap();
        store.create_round("autumn", 1).unwrap();
        assert_eq!(
            core.admission().await.unwrap(),
            Admission::Open { join_url: Some("https://t.me/scoutbot?start=autumn".to_string()) },
            "the newer round supersedes the older one without anyone closing it"
        );

        // Fill autumn. Spring is still open with room, so the door is not shut.
        let joiner = store.account_for_telegram(7).unwrap();
        assert_eq!(store.claim_seat(joiner, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(
            core.admission().await.unwrap(),
            Admission::Open { join_url: Some("https://t.me/scoutbot?start=spring".to_string()) }
        );

        store.set_round_open("spring", false).unwrap();
        assert_eq!(core.admission().await.unwrap(), Admission::Full,
            "a closed round and a full round look the same from outside");
    }

    #[test]
    fn every_channel_is_handed_the_same_sentence() {
        assert_eq!(
            reminder_text("detergent"),
            "⏰ Time to reorder detergent — want me to search for deals?"
        );
    }
    #[tokio::test]
    async fn an_on_demand_backup_lands_beside_the_database_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("maint.duckdb");
        let core = Core::start(Config::for_test(db.to_str().unwrap()), None).unwrap();

        let first = core.backup(crate::backup::Reason::Manual).await.unwrap();
        assert!(first.exists(), "the backup file should be where it says");
        assert!(first.starts_with(crate::backup::dir_for(&db)));
        assert!(first.to_str().unwrap().contains("manual"));

        // It is a database, not just bytes.
        let restored = Core::start(Config::for_test(first.to_str().unwrap()), None).unwrap();
        assert!(restored.schema_version().unwrap() >= 5);

        // No `.partial` survives a successful run.
        let leftovers = std::fs::read_dir(crate::backup::dir_for(&db))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "partial"))
            .count();
        assert_eq!(leftovers, 0);
    }

}
