//! Sending the sign-in link, through Resend.

/// The plain-text message. Separate from sending so it can be asserted on
/// without a network.
pub fn body(link: &str) -> String {
    format!(
        "Sign in to Scout by opening this link:\n\n\
         {link}\n\n\
         It works once and expires in 15 minutes.\n\n\
         If you did not ask to sign in, ignore this — nothing has happened \
         to any account.\n"
    )
}

/// Sends the link. `Err` when Resend would not take it, so the caller can
/// tell the visitor the truth rather than claim success into a void.
/// Where sign-in mail goes.
///
/// An enum rather than a trait object because there are two cases and there
/// is no prospect of a third: it either goes to Resend or it does not go.
#[derive(Clone)]
pub enum Mailer {
    Resend { api_key: String, from: String },
    /// Writes the link to the log instead of sending it.
    ///
    /// The tests use this. Without it, exercising the sign-in form fires a
    /// real HTTPS request at api.resend.com from `cargo test` — which makes
    /// the suite fail on a train, leak an address to a third party from a
    /// unit test, and depend on someone else's uptime. The rest of this
    /// repository's tests bind no socket and reach no network, and this is
    /// how that stays true here.
    Discard,
    /// Keeps the link instead of sending it, so a test can follow it.
    ///
    /// `#[cfg(test)]`, so it does not exist in a built binary: a mailer
    /// that silently kept every message in memory is not something anyone
    /// should be able to configure by accident. The link is the only place
    /// a login token ever appears in full, so a test that wants to spend
    /// one has no other way to read it.
    #[cfg(test)]
    Kept(std::sync::Arc<std::sync::Mutex<Vec<String>>>),
}

impl Mailer {
    pub async fn send(&self, to: &str, link: &str) -> anyhow::Result<()> {
        match self {
            Mailer::Resend { api_key, from } => send(api_key, from, to, link).await,
            Mailer::Discard => {
                tracing::info!(link, "sign-in link not sent: no mailer configured");
                Ok(())
            }
            #[cfg(test)]
            Mailer::Kept(sent) => {
                sent.lock().unwrap().push(link.to_string());
                Ok(())
            }
        }
    }
}

async fn send(api_key: &str, from: &str, to: &str, link: &str) -> anyhow::Result<()> {
    let res = reqwest::Client::new()
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "from": from,
            "to": [to],
            "subject": "Sign in to Scout",
            "text": body(link),
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let detail = res.text().await.unwrap_or_default();
        anyhow::bail!("resend refused the message: {status} {detail}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_carries_the_link_and_says_what_it_is_for() {
        let body = body("https://goodscout.fyi/auth/email?t=abc");
        assert!(body.contains("https://goodscout.fyi/auth/email?t=abc"));
        // Someone who did not ask for this must be told what happened
        // rather than left to guess.
        assert!(body.to_lowercase().contains("did not"));
        assert!(body.contains("15 minutes"), "the expiry is not stated");
    }
}
