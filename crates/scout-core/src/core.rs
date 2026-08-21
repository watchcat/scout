use crate::agent::AgentDeps;
use crate::config::Config;
use crate::store::Store;
use crate::tools::kagi::{KagiClient, KAGI_API_BASE};
use std::collections::HashSet;
use std::time::Duration;

/// Everything answering a question needs, and nothing about how the question
/// arrived.
///
/// In phase 2b-2 this is what the HTTP service owns. Nothing here may learn
/// what a `ChatId` is.
pub struct Core {
    pub(crate) cfg: Config,
    pub(crate) deps: AgentDeps,
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
    /// `return_url` is where Duffel Links sends a traveller back to — today
    /// the bot's own address, from `getMe`. Taking it here is a deliberate
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
    pub fn population(&self) -> (usize, usize, i64) {
        (
            self.cfg.allowed_user_ids.len(),
            self.cfg.admin_user_ids.len(),
            self.cfg.invite_daily_requests,
        )
    }

    pub fn is_founder(&self, telegram_id: i64) -> bool {
        is_founder(&self.cfg.allowed_user_ids, telegram_id)
    }

    pub fn is_admin(&self, telegram_id: i64) -> bool {
        is_admin_id(&self.cfg.admin_user_ids, telegram_id)
    }

    /// Public only until the scheduler moves inside core in Task 5; it is
    /// the one remaining way a channel can get a handle on the database.
    pub fn store(&self) -> crate::store::Store {
        self.deps.store.clone()
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
}
