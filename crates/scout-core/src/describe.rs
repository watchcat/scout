//! How a tool call reads to a person.
//!
//! This lives with the agent rather than with the renderer because every
//! channel wants the same sentence: a browser should not invent its own
//! wording for "opening bol.com". The event carries the finished text.

/// What to show while a tool runs. The arguments carry the interesting part
/// — which query, which page — so the user can see the search actually
/// widening rather than a generic spinner.
pub fn describe(tool: &str, args: &serde_json::Value) -> String {
    let s = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    match tool {
        "search_web" => {
            let langs = args
                .get("also_queries")
                .and_then(|v| v.as_array())
                .map(|a| a.len() + 1)
                .unwrap_or(1);
            match (s("query"), langs) {
                (q, 1) if !q.is_empty() => format!("🔎 searching: {q}"),
                (q, n) if !q.is_empty() => format!("🔎 searching in {n} languages: {q}"),
                _ => "🔎 searching".to_string(),
            }
        }
        "search_secondhand" => format!("🛒 second-hand: {}", s("query")),
        "fetch_page" => match host(&s("url")) {
            Some(host) => format!("📄 opening {host}"),
            None => "📄 opening a page".to_string(),
        },
        "compare_prices" => {
            let offers = args.get("offers").and_then(|v| v.as_array()).map_or(0, |a| a.len());
            let unit = s("unit_name");
            match (offers, unit.is_empty()) {
                (0, _) => "🧮 comparing prices".to_string(),
                (n, true) => format!("🧮 comparing {n} offers"),
                (n, false) => format!("🧮 comparing {n} offers per {unit}"),
            }
        }
        "query_purchases" => "📚 checking your purchase history".to_string(),
        "record_purchase" => "💾 saving the purchase".to_string(),
        "remember_fact" | "forget_fact" => "💾 updating your profile".to_string(),
        "create_reminder" | "cancel_reminder" | "list_reminders" => "⏰ reminders".to_string(),
        other => format!("⚙️ {other}"),
    }
}

fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split('/').next()?.trim_start_matches("www.");
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_calls_read_as_plain_progress() {
        assert_eq!(
            describe("search_web", &json!({"query": "wasmiddel"})),
            "🔎 searching: wasmiddel"
        );
        assert_eq!(
            describe(
                "search_web",
                &json!({"query": "detergent", "also_queries": ["wasmiddel", "Waschmittel"]})
            ),
            "🔎 searching in 3 languages: detergent"
        );
        assert_eq!(
            describe("fetch_page", &json!({"url": "https://www.bol.com/nl/nl/p/x/123/"})),
            "📄 opening bol.com"
        );
        assert_eq!(
            describe("compare_prices", &json!({"unit_name": "wash", "offers": [1, 2, 3]})),
            "🧮 comparing 3 offers per wash"
        );
        assert_eq!(
            describe("query_purchases", &json!({"search_term": "ariel"})),
            "📚 checking your purchase history"
        );
    }

    #[test]
    fn missing_or_odd_arguments_never_panic() {
        assert_eq!(describe("search_web", &json!({})), "🔎 searching");
        assert_eq!(describe("fetch_page", &json!({"url": "not a url"})), "📄 opening a page");
        assert_eq!(describe("compare_prices", &json!({})), "🧮 comparing prices");
        assert_eq!(describe("brand_new_tool", &json!(null)), "⚙️ brand_new_tool");
    }
}
