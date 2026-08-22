use anyhow::Result;
use scout_core::core::Core;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;

const TICK: Duration = Duration::from_secs(15 * 60);

/// Background loop: every 15 minutes deliver due reminders. A failed send
/// leaves next_due unchanged so the next tick retries it.
pub async fn run(bot: Bot, core: Arc<Core>) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        if let Err(e) = tick(&bot, &core).await {
            tracing::error!(error = %e, "reminder tick failed");
        }
    }
}

async fn tick(bot: &Bot, core: &Core) -> Result<()> {
    for delivery in core.due_deliveries("telegram").await? {
        let Ok(chat) = delivery.address.parse::<i64>() else {
            tracing::error!(id = delivery.id, address = %delivery.address,
                "unparseable telegram address; skipping");
            continue;
        };
        match bot.send_message(ChatId(chat), &delivery.text).await {
            // Never `?` here. An acknowledgement that fails has already cost
            // someone a delivered message, and abandoning the tick would
            // leave everyone behind them unreminded while re-sending this one
            // every quarter of an hour until the write succeeds.
            Ok(_) => {
                if let Err(e) = core.delivery_done("telegram", delivery.id).await {
                    tracing::error!(id = delivery.id, error = %e,
                        "delivered but not acked; it will be sent again");
                }
            }
            Err(e) => tracing::warn!(id = delivery.id, error = %e,
                "reminder send failed; it stays due"),
        }
    }
    Ok(())
}
