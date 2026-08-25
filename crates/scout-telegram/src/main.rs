mod bot;
mod draft;
mod progress;
mod scheduler;
mod text;

use anyhow::Result;
use dashmap::DashMap;
use scout_core::config::Config;
use std::sync::Arc;
use teloxide::Bot;
use tracing_subscriber::EnvFilter;

/// The adapter's own credential, read straight from the environment.
///
/// 2b-2b divides `Config` in two and this is the first piece to move. Blank
/// counts as unset, exactly as `Config`'s own `required()` has it, so that
/// deletion can be a pure deletion: otherwise a whitespace token would stop
/// failing at start-up and start failing at the first API call instead.
fn telegram_token() -> Result<String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("TELEGRAM_BOT_TOKEN is not set"))
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
    let population = core.population();
    tracing::info!(
        founders = population.founders,
        admins = population.admins,
        members = members.len(),
        daily_cap = population.daily_cap,
        schema = core.schema_version()?,
        "who may talk to this bot"
    );

    tokio::spawn(scheduler::run(telegram.clone(), core.clone()));
    // Backups belong to core, not to this channel: they must keep happening
    // whether or not Telegram is running.
    tokio::spawn(core.clone().run_maintenance());

    // The web front door. Same process as the bot because DuckDB is
    // single-writer; W4 is where it moves out. A failure here must not stop
    // the bot: the page going dark is worse than nothing, but a bot that
    // will not start because a port is taken is worse than that.
    let web_core = core.clone();
    let bind = std::env::var("SCOUT_WEB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    tokio::spawn(async move {
        if let Err(e) = scout_web::serve(web_core, &bind).await {
            tracing::error!(error = %e, "the front door did not open");
        }
    });

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
