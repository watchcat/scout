use anyhow::{bail, Context, Result};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub allowed_user_ids: HashSet<i64>,
    pub minimax_api_key: String,
    pub kagi_api_key: String,
    pub db_path: String,
    pub secondhand_sites: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |k: &str| -> Result<String> {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .with_context(|| format!("missing required env var {k}"))
        };

        let allowed_user_ids = parse_user_ids(&required("ALLOWED_TELEGRAM_USER_IDS")?)?;

        let secondhand_sites = get("SECONDHAND_SITES")
            .unwrap_or_else(|| "ebay.com,marktplaats.nl,vinted.com".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            telegram_bot_token: required("TELEGRAM_BOT_TOKEN")?,
            allowed_user_ids,
            minimax_api_key: required("MINIMAX_API_KEY")?,
            kagi_api_key: required("KAGI_API_KEY")?,
            db_path: get("SCOUT_DB_PATH").unwrap_or_else(|| "scout.duckdb".to_string()),
            secondhand_sites,
        })
    }
}

fn parse_user_ids(raw: &str) -> Result<HashSet<i64>> {
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
        .collect::<Result<HashSet<i64>>>()?;
    if ids.is_empty() {
        bail!("ALLOWED_TELEGRAM_USER_IDS must contain at least one user id");
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
}
