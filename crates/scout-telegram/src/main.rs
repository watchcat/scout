mod bot;
mod draft;
mod progress;
mod scheduler;
mod scope;
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

/// How long to let the front door finish what it is already serving.
///
/// One run is bounded by `RUN_BUDGET` (300s), and the deployment allows 330
/// in total — so this leaves the bot's own drain the remainder rather than
/// racing it to the SIGKILL.
const WEB_DRAIN: std::time::Duration = std::time::Duration::from_secs(300);

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
    let front_door = tokio::spawn(async move {
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

    // The dispatcher drains Telegram's handlers and returns. Returning from
    // here would drop the runtime and kill the front door's in-flight
    // requests along with it — so the 330-second grace period the
    // deployment provisions was being spent entirely on the bot, and a
    // browser answer was cut off the instant Telegram had nothing left to
    // finish. Which is milliseconds, when nobody is talking to the bot.
    //
    // Measured: a deploy killed a browser run that had already streamed its
    // whole answer, and the reader was told it would be saved to history.
    // It was not — the task that saves it died with the process.
    //
    // Bounded because Kubernetes sends SIGKILL at the grace period whatever
    // this does, and a wedged stream must not be the reason we get there.
    if let Err(e) = tokio::time::timeout(WEB_DRAIN, front_door).await {
        tracing::warn!(error = %e, "the front door did not drain in time; closing anyway");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_front_door_is_drained_before_the_process_exits() {
        // Asserted from the source because nothing here is reachable
        // without a bot token and a bound port. The original code spawned
        // the server and never looked at the handle again, so returning
        // from `main` dropped the runtime and cut every browser run in
        // flight — while the deployment was paying for 330 seconds of
        // grace that only the bot could spend.
        let src = include_str!("main.rs");
        // Stop at the test module. Below this point the file contains this
        // test's own needles, and searching there let the assertion match
        // the string it is written with — measured: deleting the drain
        // entirely left this test green.
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let dispatcher = src.find("bot::run(telegram, app).await").expect("the bot must run");
        let drained = src.rfind("front_door").expect("the front door must be awaited");
        assert!(
            drained > dispatcher,
            "the front door must be drained after the dispatcher, not abandoned when it returns"
        );
    }
}
