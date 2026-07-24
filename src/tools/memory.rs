use super::purchases::{internal, StoreToolError};
use crate::store::Store;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

const MAX_VALUE_CHARS: usize = 500;

/// Reduce key drift ("Delivery Country" / "delivery country" / "delivery_country").
pub fn normalize_key(key: &str) -> String {
    key.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
}

#[derive(Deserialize)]
pub struct RememberFactArgs {
    pub key: String,
    pub value: String,
}

pub struct RememberFactTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for RememberFactTool {
    const NAME: &'static str = "remember_fact";
    type Error = StoreToolError;
    type Args = RememberFactArgs;
    type Output = String;

    fn description(&self) -> String {
        "Save a durable fact about the user to their long-term profile \
         (delivery country, sizes, budget style, preferred shops/brands, \
         second-hand preference...). Overwrites the previous value for the \
         same key. Use short snake_case keys like delivery_country."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "short snake_case fact name, e.g. delivery_country"},
                "value": {"type": "string", "description": "the fact's value, e.g. NL"}
            },
            "required": ["key", "value"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let key = normalize_key(&args.key);
        let value = args.value.trim().to_string();
        if key.is_empty() || value.is_empty() {
            return Err(StoreToolError("key and value must be non-empty".to_string()));
        }
        if value.chars().count() > MAX_VALUE_CHARS {
            return Err(StoreToolError(format!(
                "value too long (max {MAX_VALUE_CHARS} chars) — store a short fact, not a document"
            )));
        }
        let store = self.store.clone();
        let user_id = self.user_id;
        {
            let (key, value) = (key.clone(), value.clone());
            tokio::task::spawn_blocking(move || store.upsert_fact(user_id, &key, &value))
                .await
                .map_err(internal)?
                .map_err(internal)?;
        }
        Ok(format!("saved: {key} = {value}"))
    }
}

#[derive(Deserialize)]
pub struct ForgetFactArgs {
    pub key: String,
}

pub struct ForgetFactTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for ForgetFactTool {
    const NAME: &'static str = "forget_fact";
    type Error = StoreToolError;
    type Args = ForgetFactArgs;
    type Output = String;

    fn description(&self) -> String {
        "Remove a fact from the user's long-term profile by key. Use when the \
         user says a stored fact is wrong or asks to forget it."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "the fact's key, as shown in the profile"}
            },
            "required": ["key"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let key = normalize_key(&args.key);
        let store = self.store.clone();
        let user_id = self.user_id;
        let removed = {
            let key = key.clone();
            tokio::task::spawn_blocking(move || store.forget_fact(user_id, &key))
                .await
                .map_err(internal)?
                .map_err(internal)?
        };
        Ok(if removed {
            format!("forgotten: {key}")
        } else {
            format!("no stored fact named {key}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("t.duckdb")).unwrap();
        (store, dir)
    }

    #[test]
    fn keys_are_normalized() {
        assert_eq!(normalize_key("  Delivery Country "), "delivery_country");
        assert_eq!(normalize_key("shoe_size"), "shoe_size");
        assert_eq!(normalize_key("Budget  Style"), "budget_style");
    }

    #[tokio::test]
    async fn remember_upserts_normalized_and_forget_removes() {
        let (store, _d) = setup();
        let remember = RememberFactTool { store: store.clone(), user_id: 7 };
        let out = remember
            .call(RememberFactArgs { key: "Delivery Country".into(), value: " NL ".into() })
            .await
            .unwrap();
        assert_eq!(out, "saved: delivery_country = NL");
        assert_eq!(
            store.list_facts(7).unwrap(),
            vec![("delivery_country".to_string(), "NL".to_string())]
        );

        let forget = ForgetFactTool { store: store.clone(), user_id: 7 };
        assert_eq!(
            forget.call(ForgetFactArgs { key: "delivery country".into() }).await.unwrap(),
            "forgotten: delivery_country"
        );
        assert!(store.list_facts(7).unwrap().is_empty());
        assert_eq!(
            forget.call(ForgetFactArgs { key: "delivery_country".into() }).await.unwrap(),
            "no stored fact named delivery_country"
        );
    }

    #[tokio::test]
    async fn remember_rejects_empty_and_oversized() {
        let (store, _d) = setup();
        let tool = RememberFactTool { store, user_id: 7 };
        assert!(tool
            .call(RememberFactArgs { key: "  ".into(), value: "x".into() })
            .await
            .is_err());
        assert!(tool
            .call(RememberFactArgs { key: "notes".into(), value: "x".repeat(501) })
            .await
            .is_err());
    }
}
