mod bot;
mod draft;
mod arrivals;
mod membership;
mod mini_app;
mod mirror;
mod progress;
mod scheduler;
mod scope;
mod text;
mod webhook;

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

/// Where Telegram should deliver updates, when it should. Unset means a
/// local run, which Telegram cannot reach, so the bot asks instead.
///
/// A URL whose path is not the one we serve is refused here rather than
/// registered: Telegram would accept it, post every update to a 404, and
/// the only sign would be a bot that had gone quiet.
fn webhook_url() -> Option<url::Url> {
    parse_webhook_url(std::env::var("TELEGRAM_WEBHOOK_URL").ok())
}

fn parse_webhook_url(raw: Option<String>) -> Option<url::Url> {
    let raw = raw.filter(|v| !v.trim().is_empty())?;
    match url::Url::parse(raw.trim()) {
        Ok(url) if url.scheme() == "https" && url.path() == webhook::PATH => Some(url),
        _ => {
            tracing::error!(url = %raw, path = webhook::PATH, "TELEGRAM_WEBHOOK_URL must be https and end in the webhook path; polling instead");
            None
        }
    }
}

/// Tells Telegram where to deliver, and keeps trying until it has listened.
///
/// Not fatal on failure, and not awaited before the bot starts: a webhook
/// set by the previous pod is still set, so a Telegram API hiccup at
/// start-up leaves updates flowing to the right place regardless. What
/// this call changes is only the address and the secret, and both are the
/// same every time unless the domain or the token moved.
///
/// `allowed_updates` said out loud: over polling the dispatcher asks for
/// what its handlers read, but a webhook takes whatever it was registered
/// with, and Telegram's default leaves out reactions.
async fn register_webhook(bot: Bot, url: url::Url) {
    use teloxide::payloads::SetWebhookSetters;
    use teloxide::prelude::Requester;
    use teloxide::types::AllowedUpdate;
    let secret = webhook::secret(bot.token());
    let mut wait = std::time::Duration::from_secs(2);
    loop {
        let request = bot
            .set_webhook(url.clone())
            .secret_token(secret.clone())
            .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::MessageReaction, AllowedUpdate::CallbackQuery]);
        match request.await {
            Ok(_) => break,
            Err(e) => {
                tracing::warn!(error = %e, retry_in = wait.as_secs(), "could not register the webhook");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(300));
            }
        }
    }
    // What Telegram thinks, once: a backlog or a last error here is the
    // first thing to read when the bot seems to have stopped hearing.
    match bot.get_webhook_info().await {
        Ok(info) => tracing::info!(
            url = %url,
            pending = info.pending_update_count,
            last_error = info.last_error_message.as_deref().unwrap_or("none"),
            "Telegram delivers here"
        ),
        Err(e) => tracing::warn!(error = %e, "webhook registered; could not read it back"),
    }
}

/// The "Trips" button beside the input, for every private chat.
///
/// Set on every start rather than once by hand in BotFather, so the button
/// follows the domain the deployment serves. Retried the way the webhook
/// is, and not fatal: the button a previous start set is still there.
async fn install_menu_button(bot: Bot, launch: url::Url) {
    use teloxide::payloads::SetChatMenuButtonSetters;
    use teloxide::prelude::Requester;
    let mut wait = std::time::Duration::from_secs(2);
    loop {
        match bot.set_chat_menu_button().menu_button(mini_app::menu_button(&launch)).await {
            Ok(_) => {
                tracing::info!(url = %launch, "the Trips button opens the Mini App");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_in = wait.as_secs(), "could not set the Trips button");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(300));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let token = telegram_token()?;
    let telegram = Bot::new(token.clone());

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
    let members: Arc<dashmap::DashSet<i64>> = Arc::new(core.members()?.into_iter().collect());
    let population = core.population();
    tracing::info!(
        founders = population.founders,
        admins = population.admins,
        members = members.len(),
        daily_cap = population.daily_cap,
        schema = core.schema_version()?,
        "who may talk to this bot"
    );

    // The web admits people too, and cannot reach this set. Without this
    // a person who signed in by email and linked Telegram was a member in
    // the table and a stranger at the gate until the next deploy.
    tokio::spawn(membership::watch(core.clone(), members.clone()));
    tokio::spawn(scheduler::run(telegram.clone(), core.clone()));
    // Before the mirror starts: a nudge about a booking carries an Open
    // <trip> button when the Mini App is on, and the mirror is what sends it.
    // From the site's own address, which the front door already reads, so
    // the button and the page cannot point at two different hosts.
    let mini_app = std::env::var("SCOUT_BASE_URL").ok().and_then(|b| mini_app::launch_url(&b));
    tokio::spawn(mirror::run(telegram.clone(), core.clone(), mini_app.clone()));
    // Backups belong to core, not to this channel: they must keep happening
    // whether or not Telegram is running.
    tokio::spawn(core.clone().run_maintenance());

    // The web front door. Same process as the bot because DuckDB is
    // single-writer; W4 is where it moves out. A failure here must not stop
    // the bot: the page going dark is worse than nothing, but a bot that
    // will not start because a port is taken is worse than that.
    //
    // With a webhook the front door is also how Telegram reaches the bot,
    // so a front door that did not open is a deaf bot. Nothing extra is
    // needed for that: `/healthz` is served by the same server, the
    // liveness probe fails, and Kubernetes restarts the pod.
    let (telegram_routes, listener) = match webhook_url() {
        Some(url) => {
            let (routes, listener) = webhook::intake(webhook::secret(&token));
            tokio::spawn(register_webhook(telegram.clone(), url));
            (routes, Some(listener))
        }
        None => {
            tracing::info!("no TELEGRAM_WEBHOOK_URL; asking Telegram for updates instead");
            (axum::Router::new(), None)
        }
    };
    let web_core = core.clone();
    let bind = std::env::var("SCOUT_WEB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let front_door = tokio::spawn(async move {
        if let Err(e) = scout_web::serve(web_core, &bind, telegram_routes).await {
            tracing::error!(error = %e, "the front door did not open");
        }
    });

    match &mini_app {
        Some(launch) => {
            tokio::spawn(install_menu_button(telegram.clone(), launch.clone()));
        }
        None => tracing::info!("no https SCOUT_BASE_URL; the trips Mini App has no button"),
    }

    let app = Arc::new(bot::App {
        core,
        mini_app,
        chats: DashMap::new(),
        replies: DashMap::new(),
        streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        members,
    });

    tracing::info!("scout is up");
    bot::run(telegram, app, listener).await;

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
    use super::parse_webhook_url;

    #[test]
    fn only_an_https_address_at_our_path_is_registered() {
        let ok = parse_webhook_url(Some("https://goodscout.fyi/telegram/webhook".into()));
        assert_eq!(ok.unwrap().as_str(), "https://goodscout.fyi/telegram/webhook");
        assert!(parse_webhook_url(None).is_none());
        assert!(parse_webhook_url(Some("  ".into())).is_none());
        // Telegram refuses plain http itself; refusing it here says why.
        assert!(parse_webhook_url(Some("http://goodscout.fyi/telegram/webhook".into())).is_none());
        // Accepted by Telegram, a 404 for every update: the quiet failure.
        assert!(parse_webhook_url(Some("https://goodscout.fyi/".into())).is_none());
        assert!(parse_webhook_url(Some("https://goodscout.fyi/telegram/webhook/".into())).is_none());
    }

    #[test]
    fn the_gate_is_actually_told_when_the_web_admits_someone() {
        // Same shape as the mirror check below: `membership::watch` is
        // tested on its own, and nothing but this says it is ever spawned.
        let src = include_str!("main.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert!(
            src.contains("membership::watch("),
            "membership changes on the web never reach the Telegram gate"
        );
    }

    #[test]
    fn the_mirror_queue_is_actually_drained() {
        // Nothing else would say. Without this spawn every queue write
        // still succeeds, the toggle still reports success, the outbox
        // fills up, and not one message reaches a phone — with no error
        // anywhere, because writing to a queue nobody reads is not an
        // error until somebody looks.
        //
        // Stops at the test module for the same reason its neighbour does:
        // the file below contains the string being searched for.
        let src = include_str!("main.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert!(
            src.contains("mirror::run("),
            "the mirror queue is written and never drained"
        );
    }

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
        let dispatcher = src.find("bot::run(telegram, app, listener).await").expect("the bot must run");
        let drained = src.rfind("front_door").expect("the front door must be awaited");
        assert!(
            drained > dispatcher,
            "the front door must be drained after the dispatcher, not abandoned when it returns"
        );
    }
}
