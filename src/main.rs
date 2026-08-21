mod bot;
mod progress;
mod scheduler;

use anyhow::Result;
use dashmap::DashMap;
use scout_core::config::Config;
use std::sync::Arc;
use teloxide::Bot;
use tracing_subscriber::EnvFilter;

/// The adapter's own credential, read straight from the environment.
///
/// 2b-2b divides `Config` in two and this is the first piece to move.
fn telegram_token() -> Result<String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("TELEGRAM_BOT_TOKEN is not set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let telegram = Bot::new(telegram_token()?);

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

    let core = Arc::new(scout_core::core::Core::start(cfg, return_url)?);

    // The gate reads this set on every update, so it is built once here
    // from the table that survives restarts.
    let members: dashmap::DashSet<i64> = core.members()?.into_iter().collect();
    let (founders, admins, daily_cap) = core.population();
    tracing::info!(
        founders,
        admins,
        members = members.len(),
        daily_cap,
        schema = core.schema_version()?,
        "who may talk to this bot"
    );

    tokio::spawn(scheduler::run(telegram.clone(), core.store()));

    let app = Arc::new(bot::App {
        core,
        chats: DashMap::new(),
        replies: DashMap::new(),
        streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        members,
    });

    tracing::info!("scout is up");
    bot::run(telegram, app).await;
    Ok(())
}
