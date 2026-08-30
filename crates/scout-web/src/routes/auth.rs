//! Two ways in: an emailed link, and Telegram's login widget.
//!
//! Mounted only when `AuthConfig::from_env` found every key, so every
//! handler here can assume it has somewhere to mail a link to.
//!
//! Four rules run through all of it. The answer to "email me a link" is
//! one page with one wording, whoever asked and whatever they typed. The
//! `GET` on the emailed link changes nothing. An expired token and one
//! that never existed are told apart nowhere a visitor can see. And a
//! widget payload that does not verify is refused without saying which of
//! the three checks it failed.

use super::{see_other, signed_in_as, sorry, stale_form};
use crate::{pages, session, telegram_login, AuthState};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use scout_core::identity::{self, LinkOutcome, SignIn, TokenOutcome};
use serde::Deserialize;

/// How long a mailed link is worth anything. Stated in the mail, so the
/// two have to move together.
const LINK_TTL_SECS: i64 = 900;

/// How long a session lasts. Long, because there is no way to renew one
/// short of another round trip through a mailbox, and short enough that a
/// laptop left in a café is not signed in forever. A session cannot be
/// revoked (see `session.rs`), so this number is also the longest a
/// stolen cookie is worth stealing.
const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;

/// The signed-in half of the site, over its own state.
pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/sign-in", get(sign_in_page))
        .route("/sign-in/email", post(request_link))
        .route("/auth/email", get(confirm_page).post(confirm))
        .route("/auth/telegram", get(telegram_callback))
        .with_state(auth)
}

async fn sign_in_page(State(auth): State<AuthState>) -> Html<String> {
    Html(pages::sign_in(&session::csrf(&auth.cfg.session_key), widget(&auth).as_deref()))
}

/// Telegram's login button, when the bot could name itself at start-up.
///
/// The username comes off `return_url`, which is `getMe`'s answer, rather
/// than from a variable of its own: a configured username is a second copy
/// of a fact the token already decides, and the two would drift the first
/// time a bot was renamed. `None` when `getMe` failed — a widget naming no
/// bot draws nothing, and one naming the wrong bot signs people into it.
pub(crate) fn widget(auth: &AuthState) -> Option<String> {
    let username = auth.core.return_url()?.rsplit('/').next()?;
    if username.is_empty() {
        return None;
    }
    let base = auth.cfg.base_url.trim_end_matches('/');
    Some(pages::telegram_widget(username, &format!("{base}/auth/telegram")))
}

#[derive(Deserialize)]
struct Requested {
    /// Defaulted rather than required so that a body with no `email` at
    /// all gets the same page as one with a bad address, instead of
    /// axum's 422. Every way of getting this form wrong looks the same
    /// from outside.
    #[serde(default)]
    email: String,
    #[serde(default)]
    csrf: String,
}

/// Mails a link — or quietly does not — and says the same thing either way.
async fn request_link(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Form(form): Form<Requested>,
) -> Response {
    if !session::csrf_ok(&auth.cfg.session_key, &form.csrf) {
        return stale_form();
    }
    // `None`: nobody is signed in, so the token this issues is a way in
    // rather than an address being attached to an account.
    offer_a_link(&auth, &headers, &form.email, None).await
}

/// The half of "email me a link" that is the same whether or not there is
/// a session — shared with `/account/link/email`, which differs only in
/// passing an account id.
///
/// Nothing below the first line branches on whether the address is known:
/// a token is issued and mailed for a stranger exactly as for a member,
/// because signing in is also how signing up happens. The only reasons not
/// to send are the shape of the address and the rate limits, and none of
/// them changes what the visitor is told.
pub(crate) async fn offer_a_link(
    auth: &AuthState,
    headers: &HeaderMap,
    typed: &str,
    account_id: Option<i64>,
) -> Response {
    // Trimmed and lower-cased before it becomes an identity, so that
    // `Ada@example.com` and `ada@example.com` are one account rather than
    // two that cannot see each other.
    let address = typed.trim().to_lowercase();

    // The limits count the signed-in request too. A session is not a
    // reason to be trusted with somebody else's inbox: `link` will refuse
    // an address that is not theirs, but the mail has already gone out by
    // then, and that mail is the thing being rationed.
    let ip = client_ip(headers);
    let send = deliverable(&address)
        && auth.by_address.allow(&address)
        && ip.as_deref().is_none_or(|ip| auth.by_ip.allow(ip));
    if send {
        mail_a_link(auth, address, account_id).await;
    }

    Html(pages::check_your_inbox()).into_response()
}

/// Files a token and hands the mail off to a task of its own.
async fn mail_a_link(auth: &AuthState, address: String, account_id: Option<i64>) {
    // 32 random bytes, hex. Stored as its SHA-256 and never otherwise:
    // a leaked `login_tokens` table then holds nothing anyone can spend,
    // for the same reason a password file holds hashes.
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    if let Err(e) = identity::issue_token(
        &auth.core, &hashed(&token), &address, account_id, LINK_TTL_SECS,
    ).await {
        tracing::error!(error = %e, "could not file a login token");
        return;
    }

    let link = format!("{}/auth/email?t={token}", auth.cfg.base_url.trim_end_matches('/'));
    let mailer = auth.mailer.clone();

    // Sent from a task of its own, so the response does not wait on a
    // third party and does not report what it said. `email.rs` offers the
    // caller a failure to pass on, and this caller cannot take it up:
    // Resend refuses a malformed address and accepts a plausible one, so
    // "we could not send that" would answer, out loud, whether an address
    // is real. It goes to the log, where it is the operator's problem.
    tokio::spawn(async move {
        if let Err(e) = mailer.send(&address, &link).await {
            tracing::error!(error = %e, "could not mail a sign-in link");
        }
    });
}

#[derive(Deserialize)]
struct Link {
    #[serde(default)]
    t: String,
}

/// The page the emailed link lands on. Consumes nothing.
async fn confirm_page(State(auth): State<AuthState>, Query(link): Query<Link>) -> Response {
    (
        // The token is in this page's own URL, so every link the page
        // offers would carry it in `Referer` — into somebody else's logs.
        [(header::REFERRER_POLICY, "no-referrer")],
        Html(pages::confirm(&link.t, &session::csrf(&auth.cfg.session_key))),
    )
        .into_response()
}

#[derive(Deserialize)]
struct Confirmed {
    #[serde(default)]
    t: String,
    #[serde(default)]
    csrf: String,
}

/// Spends the token behind the button.
async fn confirm(State(auth): State<AuthState>, Form(form): Form<Confirmed>) -> Response {
    if !session::csrf_ok(&auth.cfg.session_key, &form.csrf) {
        return stale_form();
    }

    let outcome = match identity::consume_token(&auth.core, &hashed(&form.t)).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!(error = %e, "could not spend a login token");
            return sorry();
        }
    };

    match outcome {
        TokenOutcome::Valid { email, account_id: None } => {
            match identity::sign_in(&auth.core, "email", &email).await {
                // Queued is signed in too. They have an account, and the
                // page they land on is the one that says where they stand;
                // turning them away with no session would be the dead end
                // this work exists to close.
                Ok(SignIn::In { account_id } | SignIn::Queued { account_id }) => {
                    signed_in(&auth, account_id)
                }
                Err(e) => {
                    tracing::error!(error = %e, "could not sign in an address");
                    sorry()
                }
            }
        }
        // Issued while somebody was already signed in, so this is an
        // address being attached rather than a way in — no new session.
        // Task 13's `/account` shows how it went; a refusal here must not
        // move the identity, which `link_identity` is what guarantees.
        TokenOutcome::Valid { email, account_id: Some(id) } => {
            // Every outcome says which it was. The token is spent either
            // way, so an unannounced refusal is a link that looks like it
            // worked and cannot be tried again — and the page it lands on
            // would show no email identity with no explanation for it.
            match identity::link(&auth.core, id, "email", &email).await {
                Ok(LinkOutcome::Linked) => see_other("/account?linked=email"),
                Ok(LinkOutcome::AlreadyYours) => see_other("/account?linked=email-already"),
                // Refused, and the identity has not moved: `link_identity`
                // is what guarantees that two sign-ups stay two accounts.
                Ok(LinkOutcome::TakenByAnother) => see_other("/account?linked=email-taken"),
                Err(e) => {
                    tracing::error!(error = %e, "could not link an address");
                    sorry()
                }
            }
        }
        TokenOutcome::AlreadyUsed => Html(pages::link_already_used()).into_response(),
        // One page for both. Answering "we have never seen that token"
        // separately from "that one has expired" would confirm which
        // tokens have existed.
        TokenOutcome::Expired | TokenOutcome::Unknown => {
            Html(pages::link_dead()).into_response()
        }
    }
}

/// Where Telegram's widget sends the browser back to.
///
/// `Vec<(String, String)>` rather than a struct: the payload's fields
/// depend on what the person has set on their Telegram profile —
/// `username`, `last_name` and `photo_url` are each sometimes absent —
/// and the signature covers every field that *is* there. Naming them in a
/// struct would mean a new optional field, added by Telegram, silently
/// dropping out of the string we check the HMAC over, and every sign-in
/// failing at once.
async fn telegram_callback(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Query(fields): Query<Vec<(String, String)>>,
) -> Response {
    // One refusal for a forged hash, a missing field and an hour-old
    // replay. Saying which would tell someone assembling a payload how
    // far they had got.
    let Some(telegram_id) = telegram_login::verify(&auth.cfg.bot_token, &fields) else {
        return (StatusCode::BAD_REQUEST, Html(pages::telegram_refused())).into_response();
    };
    let telegram_id = telegram_id.to_string();

    // A session already in hand turns this into "also let me in this way"
    // rather than "let me in". Signing in instead would quietly swap the
    // account under someone who had just asked to attach one — and if the
    // Telegram id belonged to a second account of theirs, it would strand
    // whatever was in the first.
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        return match identity::sign_in(&auth.core, "telegram", &telegram_id).await {
            // Queued is signed in too, for the reason `confirm` gives.
            Ok(SignIn::In { account_id } | SignIn::Queued { account_id }) => {
                signed_in(&auth, account_id)
            }
            Err(e) => {
                tracing::error!(error = %e, "could not sign in a Telegram id");
                sorry()
            }
        };
    };

    match identity::link(&auth.core, account_id, "telegram", &telegram_id).await {
        Ok(LinkOutcome::Linked) => see_other("/account?linked=yes"),
        Ok(LinkOutcome::AlreadyYours) => see_other("/account?linked=already"),
        // Refused, and the identity has not moved: `link_identity` is what
        // guarantees that two sign-ups stay two accounts.
        Ok(LinkOutcome::TakenByAnother) => see_other("/account?linked=taken"),
        Err(e) => {
            tracing::error!(error = %e, "could not link a Telegram id");
            sorry()
        }
    }
}

/// Sets the session cookie and sends them on.
fn signed_in(auth: &AuthState, account_id: i64) -> Response {
    let cookie = session::mint(&auth.cfg.session_key, account_id, SESSION_TTL_SECS);
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/account".to_string()),
            (header::SET_COOKIE, session::set_cookie(&cookie, SESSION_TTL_SECS)),
        ],
    )
        .into_response()
}

/// The hex SHA-256 of a token: what the database sees.
fn hashed(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether this is worth handing to Resend at all.
///
/// Not a validator. The only test of an address that means anything is
/// whether mail arrives, and that answer is deliberately not shown to the
/// person who typed it. This is a bound on what goes into a rate-limit key
/// and a way of not spending a database write on `"hello"`.
fn deliverable(address: &str) -> bool {
    address.len() <= 254
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
        && matches!(address.split_once('@'), Some((user, domain))
            if !user.is_empty()
                && domain.len() > 2
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.'))
}

/// The visitor's address, as the proxy in front of us reports it.
///
/// The socket address is the ingress controller's and is the same for
/// everybody, so a forwarded header is the only thing that tells callers
/// apart. With no such header there is no IP bucket at all, rather than
/// one bucket shared by the whole internet: a shared bucket would silently
/// take sign-in away from everyone the first time a proxy stopped setting
/// the header, and the per-address limit still stands either way.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        // The first entry is the client; the rest are the proxies it came
        // through, and a client can put whatever it likes in front of it.
        .map(|v| v.split(',').next().unwrap_or_default().trim().to_string())
        .filter(|ip| !ip.is_empty())
}

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::http::StatusCode;

    /// The form token out of a rendered page.
    ///
    /// Read back out of the real markup rather than minted here, so these
    /// tests go through the same door a browser does — a page that stopped
    /// carrying a token would fail them rather than quietly stop being
    /// checked.
    fn hidden(html: &str, name: &str) -> String {
        let marker = format!(r#"name="{name}" value=""#);
        let (_, rest) = html.split_once(&marker).unwrap_or_else(|| {
            panic!("the page has no hidden `{name}` field");
        });
        rest.split_once('"').unwrap().0.to_string()
    }

    async fn form_token(app: &axum::Router, uri: &str) -> String {
        hidden(&body_of(get(app, uri).await).await, "csrf")
    }

    #[tokio::test]
    async fn a_get_on_the_emailed_link_consumes_nothing() {
        // Corporate mail scanners follow links before the human does. If
        // GET consumed the token, the scanner would burn it and the human
        // would be told the link had expired — a failure that appears only
        // for users at exactly the organisations hardest to reproduce.
        let (app, core, _dir) = test_app().await;
        let token = "tok-scanner";
        issue(&core, token).await;

        let res = get(&app, &format!("/auth/email?t={token}")).await;
        assert_eq!(res.status(), StatusCode::OK);
        // The token is in this page's URL; `Referer` must not carry it on.
        assert_eq!(res.headers()["referrer-policy"], "no-referrer");

        // Still spendable afterwards.
        assert!(matches!(
            scout_core::identity::consume_token(&core, &hash(token)).await.unwrap(),
            scout_core::identity::TokenOutcome::Valid { .. }
        ));
    }

    #[tokio::test]
    async fn requesting_a_link_says_the_same_thing_for_any_address() {
        // Answering differently for a known address turns the form into a
        // membership oracle.
        let (app, core, _dir) = test_app().await;
        // Known here means known: an account that has already signed in.
        scout_core::identity::sign_in(&core, "email", "known@example.com").await.unwrap();

        let csrf = form_token(&app, "/sign-in").await;
        let known =
            post_form(&app, "/sign-in/email", &format!("csrf={csrf}&email=known%40example.com")).await;
        let unknown =
            post_form(&app, "/sign-in/email", &format!("csrf={csrf}&email=nobody%40example.com")).await;
        // Both accepted, not both refused: two identical 400s would pass
        // this test while proving nothing.
        assert_eq!(known.status(), StatusCode::OK);
        assert_eq!(known.status(), unknown.status());
        assert_eq!(body_of(known).await, body_of(unknown).await);

        // And an address that could never receive anything reads the same
        // as one that could, so a typo is not a signal either.
        let junk = post_form(&app, "/sign-in/email", &format!("csrf={csrf}&email=not-an-address")).await;
        assert_eq!(junk.status(), StatusCode::OK);
        assert_eq!(
            body_of(junk).await,
            body_of(post_form(&app, "/sign-in/email", &format!("csrf={csrf}&email=a%40b.com")).await)
                .await
        );
    }

    #[tokio::test]
    async fn a_post_with_no_form_token_is_refused_before_anything_happens() {
        // `SameSite=Lax` withholds the session cookie from a cross-site
        // POST, which is a promise the browser makes and not one the site
        // makes. This is the site's half.
        let (app, core, _dir) = test_app().await;
        issue(&core, "tok-forged").await;

        let asked = post_form(&app, "/sign-in/email", "email=a%40example.com").await;
        assert_eq!(asked.status(), StatusCode::BAD_REQUEST);

        let confirmed = post_form(&app, "/auth/email", "t=tok-forged").await;
        assert_eq!(confirmed.status(), StatusCode::BAD_REQUEST);
        // Refused *before acting*: the token is still there to spend.
        assert!(matches!(
            scout_core::identity::consume_token(&core, &hash("tok-forged")).await.unwrap(),
            scout_core::identity::TokenOutcome::Valid { .. }
        ));
    }

    #[tokio::test]
    async fn the_button_behind_the_link_signs_in_once() {
        let (app, core, _dir) = test_app().await;
        issue(&core, "tok-button").await;
        let csrf = form_token(&app, "/auth/email?t=tok-button").await;

        let first = post_form(&app, "/auth/email", &format!("csrf={csrf}&t=tok-button")).await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        assert_eq!(first.headers()["location"], "/account");
        let set = first.headers()["set-cookie"].to_str().unwrap().to_string();
        let value = set.split_once('=').unwrap().1.split_once(';').unwrap().0;
        assert!(
            crate::session::verify(TEST_KEY, value).is_some(),
            "the cookie it set does not verify as a session"
        );

        // Once. The second press of the same button is the second reader
        // of a forwarded email.
        let second = post_form(&app, "/auth/email", &format!("csrf={csrf}&t=tok-button")).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert!(body_of(second).await.contains("already been used"));
    }

    /// A widget payload as a query string, signed with the test bot token.
    fn widget_query(fields: &[(&str, &str)]) -> String {
        crate::telegram_login::signed_like_telegram("123456:test-bot-token", fields)
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    #[tokio::test]
    async fn a_widget_payload_that_does_not_verify_says_only_that() {
        let (app, _core, _dir) = test_app().await;
        let now = chrono::Utc::now().timestamp().to_string();

        // Edited after signing: the id is the field worth editing, since
        // it is the whole of who you are claiming to be.
        let forged = widget_query(&[("id", "777"), ("auth_date", &now)])
            .replace("id=777", "id=1");
        let res = get(&app, &format!("/auth/telegram?{forged}")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(res.headers().get("set-cookie").is_none(), "a forged payload set a cookie");

        // An hour-old payload is genuinely signed and still refused.
        let old = (chrono::Utc::now().timestamp() - 3600).to_string();
        let replayed = widget_query(&[("id", "777"), ("auth_date", &old)]);
        let stale = get(&app, &format!("/auth/telegram?{replayed}")).await;
        assert_eq!(stale.status(), StatusCode::BAD_REQUEST);

        // Byte for byte the same page: which check failed is not a thing
        // somebody assembling a payload gets to learn.
        assert_eq!(body_of(res).await, body_of(stale).await);
    }

    #[tokio::test]
    async fn the_widget_signs_you_in_when_you_are_out_and_links_when_you_are_in() {
        let (app, core, _dir) = test_app().await;
        let now = chrono::Utc::now().timestamp().to_string();
        let payload = widget_query(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &now)]);

        // Signed out: a session, and the account page.
        let res = get(&app, &format!("/auth/telegram?{payload}")).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/account");
        let set = res.headers()["set-cookie"].to_str().unwrap().to_string();
        let value = set.split_once('=').unwrap().1.split_once(';').unwrap().0;
        let telegram_account = crate::session::verify(TEST_KEY, value)
            .expect("the cookie it set does not verify as a session");

        // Signed in as somebody else: a link, no new session, and the
        // account under the cookie is the one that changes.
        let other = scout_core::identity::sign_in(&core, "email", "other@example.com")
            .await
            .unwrap();
        let scout_core::identity::SignIn::Queued { account_id: other } = other else {
            panic!("no round is open, so this should have queued");
        };
        assert_ne!(other, telegram_account);
        let cookie = crate::session::mint(TEST_KEY, other, 86_400);

        let linked = get_with_cookie(&app, &format!("/auth/telegram?{payload}"), &cookie).await;
        assert_eq!(linked.status(), StatusCode::SEE_OTHER);
        // Taken, not linked: 777 already belongs to the account the first
        // request made. Two sign-ups stay two accounts.
        assert_eq!(linked.headers()["location"], "/account?linked=taken");
        assert!(linked.headers().get("set-cookie").is_none(), "linking minted a session");
        assert!(
            !scout_core::identity::standing(&core, other).await.unwrap()
                .kinds.contains(&"telegram".to_string()),
            "an identity owned by someone else was moved"
        );
    }

    #[tokio::test]
    async fn the_login_widget_appears_only_when_the_bot_can_name_itself() {
        // `getMe` failed at start-up: no username, so no widget. A button
        // naming no bot draws nothing and one naming the wrong bot signs
        // people into it, so the honest answer is the email form alone.
        let (nameless, _core, _dir) = test_app().await;
        let page = body_of(get(&nameless, "/sign-in").await).await;
        assert!(!page.contains("telegram-widget.js"));

        let (app, _core, _dir) = test_app_named(Some("https://t.me/goodscoutbot")).await;
        let page = body_of(get(&app, "/sign-in").await).await;
        assert!(page.contains("telegram-widget.js"), "the sign-in page has no widget");
        assert!(page.contains(r#"data-telegram-login="goodscoutbot""#));
        // Absolute, and pointing at the route that receives it: Telegram
        // refuses a relative one, and refuses it in its own popup where
        // our logs never see it.
        assert!(page.contains(r#"data-auth-url="https://example.com/auth/telegram""#));
    }

    #[tokio::test]
    async fn the_sign_in_page_allows_telegram_and_nothing_else() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/sign-in").await;
        let headers = res.headers();
        let csp = headers["content-security-policy"].to_str().unwrap();

        // Every external source the policy names, as a set.
        //
        // The plan asked for a count of `https://` occurrences instead,
        // and said it had to be one. It cannot be: the widget's script is
        // served from `telegram.org` and the button it draws is an iframe
        // from `oauth.telegram.org`, and a CSP host source matches one
        // host exactly. Asserting the set keeps what the count was for —
        // an origin belonging to anyone but Telegram has to be added on
        // this line, by somebody who can say why.
        let external: std::collections::BTreeSet<&str> = csp
            .split([' ', ';'])
            .filter(|token| token.starts_with("https://"))
            .collect();
        assert_eq!(
            external,
            ["https://oauth.telegram.org", "https://telegram.org"].into_iter().collect()
        );

        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "DENY");
    }

    #[tokio::test]
    async fn every_page_of_the_signed_in_half_carries_the_headers() {
        // One layer over the whole half rather than four tuples in nine
        // handlers, because the handler that forgot them would be the one
        // that mattered. This is what says the layer is actually on.
        let (app, _core, _dir) = test_app().await;
        let cookie = crate::session::mint(TEST_KEY, 1, 86_400);
        for (res, what) in [
            (get(&app, "/sign-in").await, "/sign-in"),
            (get(&app, "/auth/email?t=nope").await, "/auth/email"),
            (get_with_cookie(&app, "/account", &cookie).await, "/account"),
            (post_form(&app, "/sign-in/email", "email=a%40b.com").await, "a refused POST"),
        ] {
            assert!(
                res.headers().contains_key("content-security-policy"),
                "{what} went out with no policy"
            );
            assert_eq!(res.headers()["x-frame-options"], "DENY", "{what}");
        }

        // And not on the public page, which has no script, no form and
        // nothing to steal. A policy it does not need is one that gets
        // loosened for a reason that was never about it.
        assert!(get(&app, "/").await.headers().get("content-security-policy").is_none());
    }

    #[tokio::test]
    async fn an_expired_link_and_one_that_never_existed_read_alike() {
        let (app, core, _dir) = test_app().await;
        // Issued already dead, which is what `ttl_secs` being signed is for.
        scout_core::identity::issue_token(&core, &hash("tok-stale"), "a@example.com", None, -1)
            .await
            .unwrap();

        let csrf = form_token(&app, "/auth/email?t=tok-stale").await;
        let expired = post_form(&app, "/auth/email", &format!("csrf={csrf}&t=tok-stale")).await;
        let never = post_form(&app, "/auth/email", &format!("csrf={csrf}&t=tok-invented")).await;

        assert_eq!(expired.status(), never.status());
        // Byte for byte: telling these apart would confirm which tokens
        // have existed, which is a slow way of asking who has signed in.
        assert_eq!(body_of(expired).await, body_of(never).await);
    }
}
