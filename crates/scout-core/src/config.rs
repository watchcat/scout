use anyhow::{bail, Context, Result};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub allowed_user_ids: HashSet<i64>,
    /// Users who may see everyone's numbers in `/stat`. Everyone else sees
    /// only their own. Defaults to the first id in the allowlist, which is
    /// whoever set the bot up; `SCOUT_ADMIN_USER_IDS` overrides that.
    pub admin_user_ids: HashSet<i64>,
    /// The model provider, as three settings, so a second provider is a
    /// change of `.env` and not of code: any OpenAI-shaped chat API. The
    /// key, `LLM_API_KEY`; the older `MINIMAX_API_KEY` is read when it is
    /// unset, so a deployment that never changed its `.env` keeps working.
    pub llm_api_key: String,
    /// Where that API lives, `LLM_BASE_URL` (or `MINIMAX_BASE_URL`).
    /// Configurable because a self-hoster may front the provider with a
    /// proxy of their own — a gateway that meters spend, or an in-house
    /// deployment — and because the tests need an address that refuses
    /// instead of one that answers: `for_test` points it at a closed port
    /// so no packet leaves the machine. Defaults to MiniMax itself.
    pub llm_base_url: String,
    /// The model every agent runs on, `LLM_MODEL`. Defaults to MiniMax M3.
    pub llm_model: String,
    /// The model the flight specialist runs on. Defaults to `llm_model`; a
    /// separate name is how a cheaper or sharper model gets tried on
    /// flights alone.
    pub flight_model: String,
    pub kagi_api_key: String,
    /// Perplexity Search API key; without it search runs on Kagi alone.
    pub perplexity_api_key: Option<String>,
    pub db_path: String,
    /// Where places are turned into coordinates. Nominatim by default;
    /// `for_test` points it at a closed port so no lookup leaves the
    /// machine, and a test that wants an answer stands a server in.
    pub nominatim_base_url: String,
    /// Who to name in the geocoder's User-Agent, as Nominatim's usage
    /// policy asks. Optional: without it the agent names Scout alone.
    pub nominatim_email: Option<String>,
    pub secondhand_sites: Vec<String>,
    /// bol.com Marketing Catalog API credentials; both set or disabled.
    pub bol_credentials: Option<(String, String)>,
    pub bol_country: String,
    /// eBay Browse API credentials; both set or feature disabled.
    pub ebay_credentials: Option<(String, String)>,
    pub ebay_marketplace: String,
    /// Duffel API key; without it the flight search tool is not offered.
    /// A `duffel_test_`-prefixed key searches Duffel's fake airline and
    /// costs nothing, which is the right key for trying this out.
    pub duffel_api_key: Option<String>,
    /// Booking fee added to flight prices and passed to Duffel Links, as a
    /// rate (0.03 = 3%). Applied to what Scout quotes as well, so the price
    /// shown is the price charged. Absent means no fee.
    pub duffel_markup_rate: f64,
    /// Whether Duffel has enabled Links on this account. A fresh account
    /// answers 403 `unavailable_feature` until they turn it on, so the
    /// booking tool is only offered when this says it will work.
    pub duffel_links_enabled: bool,
    /// How many requests one invited member may make per day, counted over
    /// text and photo messages since midnight UTC. Ids in
    /// `allowed_user_ids` are exempt — founders are the people paying for
    /// the bot. Bounds volume per person; rounds bound how many people
    /// there are.
    pub invite_daily_requests: i64,
    /// The program that draws a PDF's pages for reading from pictures:
    /// `SCOUT_PDFTOPPM`, or poppler's `pdftoppm` on the path. A setting so
    /// a test can stand a script in for it.
    pub pdftoppm: String,
    /// Ignav fare-search key. A second flights provider whose prices are
    /// approximate rather than bookable (their docs say to show them as
    /// "from $299"), so its rows arrive labelled. 1,000 free requests,
    /// then $0.002 each.
    pub ignav_api_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// A Config with only the four required variables set and the database
    /// somewhere temporary. Mirrors `base_env()` in this module's tests.
    ///
    /// The model endpoint is a port nothing listens on, so anything that
    /// reaches for the model under test fails at once with a connection
    /// error and no packet leaves the machine.
    #[cfg(test)]
    pub fn for_test(db_path: &str) -> Self {
        Self::from_lookup(|k| match k {
            "TELEGRAM_BOT_TOKEN" => Some("tok".to_string()),
            "ALLOWED_TELEGRAM_USER_IDS" => Some("111".to_string()),
            "MINIMAX_API_KEY" => Some("mk".to_string()),
            "MINIMAX_BASE_URL" => Some("http://127.0.0.1:1".to_string()),
            "NOMINATIM_BASE_URL" => Some("http://127.0.0.1:1".to_string()),
            "KAGI_API_KEY" => Some("kk".to_string()),
            "SCOUT_DB_PATH" => Some(db_path.to_string()),
            _ => None,
        })
        .expect("the four required variables are all set")
    }

    /// `for_test`, with one variable pointed somewhere else — a stand-in
    /// server for the one service the test is about.
    #[cfg(test)]
    pub fn for_test_with(db_path: &str, key: &'static str, value: &str) -> Self {
        let value = value.to_string();
        let base = Self::for_test(db_path);
        Self::from_lookup(|k| {
            if k == key {
                return Some(value.clone());
            }
            match k {
                "TELEGRAM_BOT_TOKEN" => Some("tok".to_string()),
                "ALLOWED_TELEGRAM_USER_IDS" => Some("111".to_string()),
                "MINIMAX_API_KEY" => Some("mk".to_string()),
                "MINIMAX_BASE_URL" => Some(base.llm_base_url.clone()),
                "NOMINATIM_BASE_URL" => Some(base.nominatim_base_url.clone()),
                "KAGI_API_KEY" => Some("kk".to_string()),
                "SCOUT_DB_PATH" => Some(db_path.to_string()),
                _ => None,
            }
        })
        .expect("the same variables for_test sets")
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |k: &str| -> Result<String> {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .with_context(|| format!("missing required env var {k}"))
        };

        // Parsed in order so the first entry can act as the default admin.
        let allowed = parse_user_ids(&required("ALLOWED_TELEGRAM_USER_IDS")?)?;
        let admin_user_ids = match get("SCOUT_ADMIN_USER_IDS").filter(|v| !v.trim().is_empty()) {
            Some(raw) => {
                let admins = parse_user_ids(&raw)?;
                if let Some(stranger) = admins.iter().find(|id| !allowed.contains(id)) {
                    bail!("SCOUT_ADMIN_USER_IDS contains {stranger}, who is not in ALLOWED_TELEGRAM_USER_IDS");
                }
                admins.into_iter().collect()
            }
            None => HashSet::from([allowed[0]]),
        };
        let allowed_user_ids: HashSet<i64> = allowed.into_iter().collect();

        let secondhand_sites = get("SECONDHAND_SITES")
            .unwrap_or_else(|| "ebay.com,marktplaats.nl,vinted.com".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let non_empty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let bol_credentials = match (non_empty("BOL_CLIENT_ID"), non_empty("BOL_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some((id, secret)),
            (None, None) => None,
            _ => bail!("BOL_CLIENT_ID and BOL_CLIENT_SECRET must be set together"),
        };
        let ebay_credentials = match (non_empty("EBAY_CLIENT_ID"), non_empty("EBAY_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some((id, secret)),
            (None, None) => None,
            _ => bail!("EBAY_CLIENT_ID and EBAY_CLIENT_SECRET must be set together"),
        };

        // A rate, not a percentage: 0.03 is 3%. Anything at or above 1
        // would more than double every fare, which is a typo rather than a
        // pricing decision.
        let duffel_links_enabled = non_empty("DUFFEL_LINKS_ENABLED")
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
        let duffel_markup_rate = match non_empty("DUFFEL_MARKUP_RATE") {
            Some(raw) => {
                let rate: f64 = raw
                    .trim()
                    .parse()
                    .with_context(|| format!("DUFFEL_MARKUP_RATE is not a number: {raw:?}"))?;
                if !(0.0..1.0).contains(&rate) {
                    bail!(
                        "DUFFEL_MARKUP_RATE must be a rate between 0 and 1 \
                         (0.03 means 3%), got {rate}"
                    );
                }
                if rate > 0.0 && !duffel_links_enabled {
                    bail!(
                        "DUFFEL_MARKUP_RATE is set but DUFFEL_LINKS_ENABLED is not: without a \
                         checkout of your own, nobody ever charges that fee, so marking prices \
                         up would only overstate them"
                    );
                }
                rate
            }
            None => 0.0,
        };

        // A cap of zero would mute every invited member without saying so,
        // which is a typo rather than a policy: closing a round is how you
        // stop admitting people, and revoking is how you remove one.
        let invite_daily_requests = match non_empty("INVITE_DAILY_REQUESTS") {
            Some(raw) => {
                let n: i64 = raw
                    .trim()
                    .parse()
                    .with_context(|| format!("INVITE_DAILY_REQUESTS is not a number: {raw:?}"))?;
                if n < 1 {
                    bail!("INVITE_DAILY_REQUESTS must be at least 1, got {n}");
                }
                n
            }
            None => 20,
        };

        // The new name first, the MiniMax name after it, so both spellings
        // of a deployment's `.env` read the same provider.
        let either = |new: &str, old: &str| non_empty(new).or_else(|| non_empty(old));
        let llm_api_key = either("LLM_API_KEY", "MINIMAX_API_KEY")
            .with_context(|| "missing required env var LLM_API_KEY (MINIMAX_API_KEY is read too)")?;
        let llm_base_url = either("LLM_BASE_URL", "MINIMAX_BASE_URL")
            .unwrap_or_else(|| crate::agent::DEFAULT_BASE_URL.to_string());
        let llm_model = non_empty("LLM_MODEL").unwrap_or_else(|| crate::agent::DEFAULT_MODEL.to_string());

        Ok(Self {
            telegram_bot_token: required("TELEGRAM_BOT_TOKEN")?,
            allowed_user_ids,
            admin_user_ids,
            flight_model: non_empty("FLIGHT_MODEL").unwrap_or_else(|| llm_model.clone()),
            llm_api_key,
            llm_base_url,
            llm_model,
            kagi_api_key: required("KAGI_API_KEY")?,
            perplexity_api_key: non_empty("PERPLEXITY_API_KEY"),
            db_path: get("SCOUT_DB_PATH").unwrap_or_else(|| "scout.duckdb".to_string()),
            nominatim_base_url: get("NOMINATIM_BASE_URL")
                .filter(|v| !v.trim().is_empty())
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://nominatim.openstreetmap.org".to_string()),
            nominatim_email: get("NOMINATIM_EMAIL").filter(|v| !v.trim().is_empty()),
            secondhand_sites,
            bol_credentials,
            bol_country: get("BOL_COUNTRY").unwrap_or_else(|| "NL".to_string()),
            ebay_credentials,
            ebay_marketplace: get("EBAY_MARKETPLACE").unwrap_or_else(|| "EBAY_NL".to_string()),
            duffel_api_key: non_empty("DUFFEL_API_KEY"),
            duffel_markup_rate,
            duffel_links_enabled,
            ignav_api_key: non_empty("IGNAV_API_KEY"),
            invite_daily_requests,
            pdftoppm: non_empty("SCOUT_PDFTOPPM").unwrap_or_else(|| "pdftoppm".to_string()),
        })
    }
}

/// Ordered so callers can treat the first id as "whoever set this up".
fn parse_user_ids(raw: &str) -> Result<Vec<i64>> {
    let ids = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let id = s
                .parse::<i64>()
                .with_context(|| format!("invalid Telegram user id: {s:?}"))?;
            if id < 1 {
                bail!("invalid Telegram user id (must be positive): {s}");
            }
            Ok(id)
        })
        .collect::<Result<Vec<i64>>>()?;
    if ids.is_empty() {
        bail!("user id list must contain at least one id");
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_env() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("TELEGRAM_BOT_TOKEN", "tok"),
            ("ALLOWED_TELEGRAM_USER_IDS", "111, 222"),
            ("MINIMAX_API_KEY", "mk"),
            ("KAGI_API_KEY", "kk"),
        ])
    }

    fn load(env: &HashMap<&str, &str>) -> Result<Config> {
        Config::from_lookup(|k| env.get(k).map(|v| v.to_string()))
    }

    #[test]
    fn the_flight_agent_runs_on_the_main_model_unless_told_otherwise() {
        assert_eq!(load(&base_env()).unwrap().flight_model, crate::agent::DEFAULT_MODEL);

        let mut env = base_env();
        env.insert("FLIGHT_MODEL", "some-other-model");
        assert_eq!(load(&env).unwrap().flight_model, "some-other-model");

        env.insert("FLIGHT_MODEL", " ");
        assert_eq!(load(&env).unwrap().flight_model, crate::agent::DEFAULT_MODEL);
    }

    #[test]
    fn the_provider_is_three_settings_and_the_minimax_names_still_read() {
        let cfg = load(&base_env()).unwrap();
        assert_eq!(cfg.llm_api_key, "mk", "MINIMAX_API_KEY is read when LLM_API_KEY is unset");
        assert_eq!(cfg.llm_base_url, crate::agent::DEFAULT_BASE_URL);
        assert_eq!(cfg.llm_model, crate::agent::DEFAULT_MODEL);

        let mut env = base_env();
        env.insert("LLM_API_KEY", "lk");
        env.insert("LLM_BASE_URL", "https://llm.example/v1");
        env.insert("LLM_MODEL", "other-model");
        env.insert("MINIMAX_BASE_URL", "https://proxy.example/v1");
        let cfg = load(&env).unwrap();
        assert_eq!(
            (cfg.llm_api_key.as_str(), cfg.llm_base_url.as_str(), cfg.llm_model.as_str()),
            ("lk", "https://llm.example/v1", "other-model"),
            "the LLM_ names win where both are set"
        );
        assert_eq!(cfg.flight_model, "other-model", "the flight desk follows the main model");

        let mut env = base_env();
        env.remove("MINIMAX_API_KEY");
        let err = load(&env).expect_err("no key under either name");
        assert!(err.to_string().contains("LLM_API_KEY"), "{err}");
        env.insert("LLM_API_KEY", "lk");
        assert_eq!(load(&env).unwrap().llm_api_key, "lk");
    }

    #[test]
    fn parses_full_config_with_defaults() {
        let cfg = load(&base_env()).unwrap();
        assert_eq!(cfg.telegram_bot_token, "tok");
        assert_eq!(cfg.allowed_user_ids, HashSet::from([111, 222]));
        assert_eq!(cfg.db_path, "scout.duckdb");
        assert_eq!(
            cfg.secondhand_sites,
            vec!["ebay.com", "marktplaats.nl", "vinted.com"]
        );
    }

    #[test]
    fn optional_overrides_are_used() {
        let mut env = base_env();
        env.insert("SCOUT_DB_PATH", "/tmp/x.duckdb");
        env.insert("SECONDHAND_SITES", "a.com , b.nl");
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.db_path, "/tmp/x.duckdb");
        assert_eq!(cfg.secondhand_sites, vec!["a.com", "b.nl"]);
    }

    #[test]
    fn missing_required_var_fails_with_its_name() {
        let mut env = base_env();
        env.remove("KAGI_API_KEY");
        let err = load(&env).unwrap_err().to_string();
        assert!(err.contains("KAGI_API_KEY"), "got: {err}");
    }

    #[test]
    fn empty_required_var_fails() {
        let mut env = base_env();
        env.insert("MINIMAX_API_KEY", "  ");
        assert!(load(&env).is_err());
    }

    #[test]
    fn malformed_user_id_fails() {
        let mut env = base_env();
        env.insert("ALLOWED_TELEGRAM_USER_IDS", "111,abc");
        assert!(load(&env).is_err());
    }

    #[test]
    fn the_model_endpoint_defaults_to_minimax_and_can_be_pointed_elsewhere() {
        assert_eq!(
            load(&base_env()).unwrap().llm_base_url,
            crate::agent::DEFAULT_BASE_URL
        );

        let mut env = base_env();
        env.insert("MINIMAX_BASE_URL", "http://127.0.0.1:1");
        assert_eq!(load(&env).unwrap().llm_base_url, "http://127.0.0.1:1");

        // Blank counts as unset, like every other optional here — an empty
        // line in a .env must not point the agent at nowhere.
        env.insert("MINIMAX_BASE_URL", "  ");
        assert_eq!(
            load(&env).unwrap().llm_base_url,
            crate::agent::DEFAULT_BASE_URL
        );
    }

    #[test]
    fn empty_allowlist_fails() {
        let mut env = base_env();
        env.insert("ALLOWED_TELEGRAM_USER_IDS", " ");
        assert!(load(&env).is_err());
    }

    #[test]
    fn non_positive_user_id_fails() {
        let mut env = base_env();
        env.insert("ALLOWED_TELEGRAM_USER_IDS", "-100123,111");
        assert!(load(&env).is_err());

        env.insert("ALLOWED_TELEGRAM_USER_IDS", "0,111");
        assert!(load(&env).is_err());
    }

    #[test]
    fn first_allowed_id_is_the_default_admin() {
        // Whoever set the bot up is listed first; they get the cross-user
        // /stat view without any extra configuration.
        let cfg = load(&base_env()).unwrap();
        assert_eq!(cfg.admin_user_ids, HashSet::from([111]));
        assert!(!cfg.admin_user_ids.contains(&222));
    }

    #[test]
    fn explicit_admin_ids_override_the_default() {
        let mut env = base_env();
        env.insert("SCOUT_ADMIN_USER_IDS", "222");
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.admin_user_ids, HashSet::from([222]));

        env.insert("SCOUT_ADMIN_USER_IDS", "111, 222");
        assert_eq!(
            load(&env).unwrap().admin_user_ids,
            HashSet::from([111, 222])
        );
    }

    #[test]
    fn an_admin_must_also_be_allowed() {
        // Otherwise the id can never reach /stat and the config silently
        // does nothing.
        let mut env = base_env();
        env.insert("SCOUT_ADMIN_USER_IDS", "333");
        let err = load(&env).unwrap_err().to_string();
        assert!(err.contains("333"), "got: {err}");

        // Blank is the same as unset: fall back to the first allowed id.
        env.insert("SCOUT_ADMIN_USER_IDS", "  ");
        assert_eq!(load(&env).unwrap().admin_user_ids, HashSet::from([111]));
    }

    #[test]
    fn the_duffel_key_is_optional_and_turns_on_flight_search() {
        // Absent is the normal case: everything else in Scout still works,
        // and the model is never shown a tool that cannot run.
        assert!(load(&base_env()).unwrap().duffel_api_key.is_none());

        let mut env = base_env();
        env.insert("DUFFEL_API_KEY", "duffel_test_abc");
        assert_eq!(
            load(&env).unwrap().duffel_api_key.as_deref(),
            Some("duffel_test_abc")
        );

        // Blank is the same as unset, not an empty bearer token.
        env.insert("DUFFEL_API_KEY", "   ");
        assert!(load(&env).unwrap().duffel_api_key.is_none());
    }

    #[test]
    fn a_markup_without_a_checkout_to_charge_it_is_refused() {
        // Duffel Links is off by default — a new account gets 403
        // unavailable_feature until they enable it. Marking prices up while
        // everyone still books at the airline would simply overstate every
        // fare, so the two settings have to travel together.
        let mut env = base_env();
        env.insert("DUFFEL_API_KEY", "duffel_live_x");
        env.insert("DUFFEL_MARKUP_RATE", "0.03");
        let err = load(&env).unwrap_err().to_string();
        assert!(err.contains("DUFFEL_LINKS_ENABLED"), "got: {err}");

        env.insert("DUFFEL_LINKS_ENABLED", "true");
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.duffel_markup_rate, 0.03);
        assert!(cfg.duffel_links_enabled);

        // Links without a markup is perfectly reasonable: sell at cost.
        let mut env = base_env();
        env.insert("DUFFEL_API_KEY", "duffel_live_x");
        env.insert("DUFFEL_LINKS_ENABLED", "true");
        assert_eq!(load(&env).unwrap().duffel_markup_rate, 0.0);
    }

    #[test]
    fn the_markup_defaults_to_nothing_and_is_refused_if_it_is_not_a_rate() {
        assert_eq!(load(&base_env()).unwrap().duffel_markup_rate, 0.0);

        let mut env = base_env();
        // A markup needs a checkout to charge it; see the test above.
        env.insert("DUFFEL_LINKS_ENABLED", "true");
        env.insert("DUFFEL_MARKUP_RATE", "0.03");
        assert_eq!(load(&env).unwrap().duffel_markup_rate, 0.03);

        // A percentage typed as a percentage would charge 300%.
        env.insert("DUFFEL_MARKUP_RATE", "3");
        assert!(load(&env).is_err(), "3 means 300% and is almost certainly a typo for 0.03");

        env.insert("DUFFEL_MARKUP_RATE", "-0.1");
        assert!(load(&env).is_err());

        env.insert("DUFFEL_MARKUP_RATE", "three percent");
        assert!(load(&env).is_err());
    }

    #[test]
    fn the_daily_cap_defaults_to_twenty_and_refuses_nonsense() {
        assert_eq!(load(&base_env()).unwrap().invite_daily_requests, 20);

        let mut env = base_env();
        env.insert("INVITE_DAILY_REQUESTS", "50");
        assert_eq!(load(&env).unwrap().invite_daily_requests, 50);

        // Blank is unset, not zero.
        env.insert("INVITE_DAILY_REQUESTS", "   ");
        assert_eq!(load(&env).unwrap().invite_daily_requests, 20);

        // Zero would silently mute everyone who was invited. Closing a
        // round is how you stop admitting people; this is not that.
        env.insert("INVITE_DAILY_REQUESTS", "0");
        assert!(load(&env).is_err());

        env.insert("INVITE_DAILY_REQUESTS", "-5");
        assert!(load(&env).is_err());

        env.insert("INVITE_DAILY_REQUESTS", "twenty");
        assert!(load(&env).is_err());
    }

    #[test]
    fn ebay_credentials_all_or_nothing() {
        let cfg = load(&base_env()).unwrap();
        assert!(cfg.ebay_credentials.is_none());
        assert_eq!(cfg.ebay_marketplace, "EBAY_NL");

        let mut env = base_env();
        env.insert("EBAY_CLIENT_ID", "app-id");
        env.insert("EBAY_CLIENT_SECRET", "cert-id");
        env.insert("EBAY_MARKETPLACE", "EBAY_DE");
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.ebay_credentials, Some(("app-id".to_string(), "cert-id".to_string())));
        assert_eq!(cfg.ebay_marketplace, "EBAY_DE");

        let mut env = base_env();
        env.insert("EBAY_CLIENT_ID", "app-id-only");
        assert!(load(&env).is_err());
    }
}
