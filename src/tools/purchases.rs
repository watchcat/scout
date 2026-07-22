use crate::store::{NewPurchase, Purchase, Store};
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

/// Store-backed tools run blocking DuckDB calls via spawn_blocking and
/// surface any failure as this string error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StoreToolError(pub String);

pub(crate) fn internal(e: impl std::fmt::Display) -> StoreToolError {
    StoreToolError(e.to_string())
}

pub struct RecordPurchaseTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for RecordPurchaseTool {
    const NAME: &'static str = "record_purchase";
    type Error = StoreToolError;
    type Args = NewPurchase;
    type Output = Purchase;

    fn description(&self) -> String {
        "Record that the user bought something. Call when the user says they \
         purchased an item. Dates are YYYY-MM-DD."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "item": {"type": "string", "description": "what was bought"},
                "store": {"type": "string", "description": "shop or site it was bought from"},
                "url": {"type": "string", "description": "product link, if known"},
                "price": {"type": "number", "description": "amount paid, if known"},
                "currency": {"type": "string", "description": "e.g. USD, EUR, PLN"},
                "notes": {"type": "string"},
                "purchased_at": {"type": "string", "description": "YYYY-MM-DD; omit if today"}
            },
            "required": ["item", "store"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        tokio::task::spawn_blocking(move || store.record_purchase(user_id, args))
            .await
            .map_err(internal)?
            .map_err(internal)
    }
}

#[derive(Deserialize)]
pub struct QueryPurchasesArgs {
    pub search_term: Option<String>,
    pub limit: Option<usize>,
}

pub struct QueryPurchasesTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for QueryPurchasesTool {
    const NAME: &'static str = "query_purchases";
    type Error = StoreToolError;
    type Args = QueryPurchasesArgs;
    type Output = Vec<Purchase>;

    fn description(&self) -> String {
        "Look up the user's past purchases, newest first. ALWAYS call this \
         before searching when the user wants to find or buy something, to \
         spot repeat purchases. Omit search_term to list recent purchases."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "search_term": {"type": "string", "description": "substring matched against item, store and notes"},
                "limit": {"type": "integer", "description": "max results, default 10"}
            }
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        let limit = args.limit.unwrap_or(10).min(50);
        tokio::task::spawn_blocking(move || {
            store.query_purchases(user_id, args.search_term.as_deref(), limit)
        })
        .await
        .map_err(internal)?
        .map_err(internal)
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

    #[tokio::test]
    async fn record_then_query_scoped_to_tool_user() {
        let (store, _d) = setup();
        let record = RecordPurchaseTool { store: store.clone(), user_id: 7 };
        let recorded = record
            .call(NewPurchase {
                item: "coffee beans".into(),
                store: "Amazon".into(),
                url: None,
                price: Some(12.5),
                currency: Some("EUR".into()),
                notes: None,
                purchased_at: Some("2026-06-28".into()),
            })
            .await
            .unwrap();
        assert_eq!(recorded.item, "coffee beans");

        let query = QueryPurchasesTool { store: store.clone(), user_id: 7 };
        let found = query
            .call(QueryPurchasesArgs { search_term: Some("coffee".into()), limit: None })
            .await
            .unwrap();
        assert_eq!(found.len(), 1);

        // A different user's tool sees nothing.
        let other = QueryPurchasesTool { store, user_id: 8 };
        let found = other
            .call(QueryPurchasesArgs { search_term: None, limit: None })
            .await
            .unwrap();
        assert!(found.is_empty());
    }
}
