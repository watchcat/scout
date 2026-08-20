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

    let renderer = tools::browser::find_chrome().map(tools::browser::Renderer::new);
    tracing::info!(headless_browser = renderer.is_some(), "fallback renderer");

    let bol = cfg.bol_credentials.clone().map(|(id, secret)| {
        tools::bol::BolClient::new(
            http.clone(),
            id,
            secret,
            cfg.bol_country.clone(),
            tools::bol::BOL_API_BASE.to_string(),
            tools::bol::BOL_LOGIN_BASE.to_string(),
        )
    });
    tracing::info!(bol_api = bol.is_some(), "bol.com Catalog API");

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

    let duffel = cfg.duffel_api_key.clone().map(|key| {
        tools::duffel::DuffelClient::new(
            http.clone(),
            key,
            tools::duffel::DUFFEL_API_BASE.to_string(),
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
    let mut deps = AgentDeps {
        llm,
        kagi,
        renderer,
        bol,
        perplexity,
        http,
        ebay,
        duffel,
        marktplaats,
        store: store.clone(),
        secondhand_sites: cfg.secondhand_sites.clone(),
        // Filled in below, once the bot has told us its username.
        return_url: None,
        links_enabled: cfg.duffel_links_enabled,
        shown: std::sync::Arc::new(tools::shown::ShownFlights::default()),
        ignav: cfg.ignav_api_key.clone().map(|key| {
            tools::ignav::IgnavClient::new(
                http_for_ignav,
                key,
                tools::ignav::IGNAV_API_BASE.to_string(),
            )
        }),
    };

    let telegram = Bot::new(cfg.telegram_bot_token.clone());

    // Duffel Links needs somewhere to send the traveller afterwards, and
    // the bot's own chat is the only address Scout owns. Asked for at
    // startup rather than configured, so it cannot drift from the token.
    let return_url = match teloxide::prelude::Requester::get_me(&telegram).await {
        Ok(me) => me.username.as_ref().map(|u| format!("https://t.me/{u}")),
        Err(e) => {
            tracing::warn!(error = %e, "could not read the bot's username; booking links disabled");
            None
        }
    };
    deps.return_url = return_url;

    // The gate reads this set on every update, so it is built once here
    // from the table that survives restarts.
    let members: dashmap::DashSet<i64> = store.active_members()?.into_iter().collect();
    tracing::info!(
        founders = cfg.allowed_user_ids.len(),
        members = members.len(),
        daily_cap = cfg.invite_daily_requests,
        schema = store.schema_version()?,
        "who may talk to this bot"
    );

    tokio::spawn(scheduler::run(telegram.clone(), store));

    let app = Arc::new(bot::App {
        cfg,
        deps,
        chats: DashMap::new(),
        replies: DashMap::new(),
        streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        members,
    });

    tracing::info!("scout is up");
    bot::run(telegram, app).await;
    Ok(())
}
