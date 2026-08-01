mod agent;
mod bot;
mod config;
mod draft;
mod links;
mod progress;
mod scheduler;
mod stats;
mod store;
mod text;
mod tools;
mod vision;

use agent::AgentDeps;
use anyhow::Result;
use config::Config;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use store::Store;
use teloxide::Bot;
use tools::kagi::{KagiClient, KAGI_API_BASE};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let store = Store::open(&cfg.db_path)?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let kagi = KagiClient::new(http.clone(), cfg.kagi_api_key.clone(), KAGI_API_BASE.to_string());
    let llm = agent::llm_client(&cfg.minimax_api_key)?;
    let ebay = cfg.ebay_credentials.clone().map(|(id, secret)| {
        tools::ebay::EbayClient::new(
            http.clone(),
            id,
            secret,
            cfg.ebay_marketplace.clone(),
            tools::ebay::EBAY_API_BASE.to_string(),
        )
    });
    tracing::info!(ebay_api = ebay.is_some(), "eBay Browse API verification");
    let marktplaats = tools::marktplaats::MarktplaatsClient::new(
        http.clone(),
        tools::marktplaats::MARKTPLAATS_BASE.to_string(),
    );

    let perplexity = cfg.perplexity_api_key.clone().map(|key| {
        tools::perplexity::PerplexityClient::new(
            http.clone(),
            key,
            tools::perplexity::PERPLEXITY_API_BASE.to_string(),
        )
    });
    tracing::info!(
        perplexity = perplexity.is_some(),
        "second search engine (Kagi always on)"
    );

    let deps = AgentDeps {
        llm,
        kagi,
        perplexity,
        http,
        ebay,
        marktplaats,
        store: store.clone(),
        secondhand_sites: cfg.secondhand_sites.clone(),
    };

    let telegram = Bot::new(cfg.telegram_bot_token.clone());
    tokio::spawn(scheduler::run(telegram.clone(), store));

    let app = Arc::new(bot::App {
        cfg,
        deps,
        chats: DashMap::new(),
    });

    tracing::info!("scout is up");
    bot::run(telegram, app).await;
    Ok(())
}
