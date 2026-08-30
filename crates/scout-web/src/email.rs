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
pub async fn send(api_key: &str, from: &str, to: &str, link: &str) -> anyhow::Result<()> {
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
