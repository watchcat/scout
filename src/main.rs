mod agent;
mod bot;
mod config;
mod draft;
mod scheduler;
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

    let deps = AgentDeps {
        llm,
        kagi,
        http,
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
