//! Two ways in: an emailed link, and Telegram's login widget.
//!
//! Mounted only when `AuthConfig::from_env` found every key, so every
//! handler here can assume it has somewhere to mail a link to.
//!
//! Four rules run through all of it. The answer to "email me a link" is
//! one page with one wording, whoever asked and whatever they typed. The
//! `GET` on the emailed link changes nothing. An expired token and one
//! that never existed are told apart nowhere a visitor can see. And a
//! widget payload that does not check out is refused without saying which
//! of the four checks — hash, fields, age, and the `state` that says this
//! browser asked for it — it failed.

use super::{see_other, signed_in_as, sorry, stale_form};
use crate::{pages, ratelimit, session, telegram_login, AuthState};
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
        // Every `POST` below has to have come from a page of ours. See
        // `routes::from_our_own_pages` for what that can and cannot
        // establish, and why the form tokens stay as well.
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

async fn sign_in_page(State(auth): State<AuthState>, headers: HeaderMap) -> Html<String> {
    // Signed in and looking at `/sign-in` is unusual but allowed, and the
    // widget's state has to name whoever is actually holding the browser
    // or their own press of the button would be refused.
    let widget = widget(&auth, signed_in_as(&auth, &headers));
    Html(pages::sign_in(&session::csrf(&auth.cfg.session_key), widget.as_deref()))
}

/// Telegram's login button, when the bot could name itself at start-up.
///
/// The username comes off `return_url`, which is `getMe`'s answer, rather
/// than from a variable of its own: a configured username is a second copy
/// of a fact the token already decides, and the two would drift the first
/// time a bot was renamed. `None` when `getMe` failed — a widget naming no
/// bot draws nothing, and one naming the wrong bot signs people into it.
///
/// `account_id` is whoever is looking at the page this button is drawn on,
/// and `None` when that is nobody.
pub(crate) fn widget(auth: &AuthState, account_id: Option<i64>) -> Option<String> {
    let username = auth.core.return_url()?.rsplit('/').next()?;
    if username.is_empty() {
        return None;
    }
    let base = auth.cfg.base_url.trim_end_matches('/');
    // `data-auth-url` is where Telegram sends the browser back to, and
    // Telegram appends its own fields to whatever is already there — so a
    // query parameter put here comes back on the callback, and is the only
    // part of that request neither Telegram nor a stranger can produce.
    //
    // Bound to the browser this page is being drawn for. Signed out is
    // account 0, which is what `csrf` already means by "nobody": that a
    // state fetched from the public `/sign-in` cannot be replayed onto a
    // request carrying somebody's session is the whole of the defence,
    // since `/sign-in` hands one to anybody who asks.
    //
    // Over the `.csrf` key rather than the session key, so this can never
    // itself be presented as a session cookie — the same reason form
    // tokens use it. It is in fact indistinguishable from a form token for
    // the same account, and that costs nothing: only that account's own
    // browser ever holds either.
    let state = session::csrf_for(&auth.cfg.session_key, account_id.unwrap_or(0));
    Some(pages::telegram_widget(username, &format!("{base}/auth/telegram?state={state}")))
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
    //
    // Counted on `ratelimit`'s keys rather than on what was typed and what
    // the header said: `victim+1@` and `victim+2@` are one inbox, and a
    // /128 out of an IPv6 client's own /64 is one client.
    let send = deliverable(&address)
        && auth.by_address.allow(&ratelimit::address_key(&address))
        && auth.by_ip.allow(&client_bucket(headers));
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
                // One of the two held nothing, so they were the same person
                // twice and are now one account. The session has to follow
                // the survivor, which may not be the account that asked.
                Ok(LinkOutcome::Merged { account_id }) => {
                    signed_in_at(&auth, account_id, "/account?linked=merged")
                }
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
    // Ours out of Telegram's before anything is checked. The HMAC covers
    // every field Telegram put in the URL and none that we did, so feeding
    // `state` to `verify` would break every genuine signature.
    let (ours, theirs): (Vec<_>, Vec<_>) =
        fields.into_iter().partition(|(k, _)| k == "state");

    // Whose browser this is, decided before anything this request carries
    // gets to act. A `GET` that changes something needs a token minted by
    // us for this browser, or `SameSite=Lax` — which sends the session
    // cookie on a top-level cross-site navigation — lets a stranger hand a
    // signed-in visitor a payload of their own and have it attached to the
    // visitor's account.
    let account_id = signed_in_as(&auth, &headers);
    let state_ok = ours
        .iter()
        .any(|(_, v)| session::csrf_ok_for(&auth.cfg.session_key, v, account_id.unwrap_or(0)));

    // One refusal for a forged hash, a missing field, an hour-old replay
    // and a state that is not ours. Saying which would tell someone
    // assembling a payload how far they had got.
    let refused =
        || (StatusCode::BAD_REQUEST, Html(pages::telegram_refused())).into_response();
    if !state_ok {
        return refused();
    }
    let Some(telegram_id) = telegram_login::verify(&auth.cfg.bot_token, &theirs) else {
        return refused();
    };
    let telegram_id = telegram_id.to_string();

    // A session already in hand turns this into "also let me in this way"
    // rather than "let me in". Signing in instead would quietly swap the
    // account under someone who had just asked to attach one — and if the
    // Telegram id belonged to a second account of theirs, it would strand
    // whatever was in the first.
    let Some(account_id) = account_id else {
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
        // See `confirm`: the survivor is usually the Telegram account,
        // because the session asking is the empty one the email sign-in
        // minted. `signed_in_at` is what stops the old cookie outliving it.
        Ok(LinkOutcome::Merged { account_id: survivor }) => {
            signed_in_at(&auth, survivor, "/account?linked=merged")
        }
        Err(e) => {
            tracing::error!(error = %e, "could not link a Telegram id");
            sorry()
        }
    }
}

/// Sets the session cookie and sends them on.
fn signed_in(auth: &AuthState, account_id: i64) -> Response {
    signed_in_at(auth, account_id, "/account")
}

/// The same, landing somewhere that says what just happened.
///
/// A merge is the one path that re-issues a session it did not create. The
/// account that survives is usually *not* the one the visitor was signed in
/// as — signing in by email mints an empty account, so the person who then
/// adds Telegram is the empty side — and leaving the old cookie in place
/// would leave them holding a session for an account that no longer exists.
fn signed_in_at(auth: &AuthState, account_id: i64, location: &str) -> Response {
    let cookie = session::mint(&auth.cfg.session_key, account_id, SESSION_TTL_SECS);
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, location.to_string()),
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

/// The bucket every request with no usable forwarded address shares.
///
/// One bucket for all of them, rather than no bucket at all. The previous
/// answer was "no IP limit", on the reasoning that a shared bucket would
/// take sign-in away from everyone the first time a proxy stopped setting
/// the header — but that reasoning only weighs one of the two failures.
/// Behind the ingress every request carries `X-Forwarded-For`, so a
/// request without one means the deployment is misconfigured or is being
/// reached past the proxy; and "misconfigured" must not be the state in
/// which the mail limit switches off. Of the two costs, sign-in throttled
/// to the per-IP quota until someone fixes the ingress is loud, bounded
/// and recoverable, and an unmetered path to sending mail is neither.
///
/// It cannot collide with a real key: `ratelimit::ip_key` only ever
/// returns something that parsed as an address.
const NO_CLIENT_ADDRESS: &str = "no-forwarded-for";

/// Which per-IP bucket this request counts against.
///
/// The socket address is the ingress controller's and is the same for
/// everybody, so a forwarded header is the only thing that tells callers
/// apart.
///
/// **The last entry, not the first.** `X-Forwarded-For` is appended to:
/// each proxy adds the address it received the connection from. So the
/// first entry is whatever the original client claimed — which a client
/// writes itself, and can make a fresh one of per request — and the last
/// is the only entry written by something we trust, the proxy directly in
/// front of us. Taking the first is right only for an edge that *replaces*
/// the header rather than appending to it, which is not a property this
/// code can check from here. Reading the last means the value we count on
/// is the one our own ingress put there, and a client prepending anything
/// it likes buys nothing.
///
/// If it is repeated as several header lines, the last line's last entry
/// is the one the nearest proxy wrote, for the same reason.
///
/// Anything that does not parse as an address — including a header a
/// client got to write with nothing in front of it — falls through to the
/// shared bucket rather than becoming a key of its own, or junk would buy
/// one bucket per junk string.
fn client_bucket(headers: &HeaderMap) -> String {
    client_ip(headers).unwrap_or_else(|| NO_CLIENT_ADDRESS.to_string())
}

fn client_ip(headers: &HeaderMap) -> Option<String> {
    let mut lines = headers.get_all("x-forwarded-for").iter();
    let raw = match lines.next_back() {
        Some(v) => v,
        None => headers.get("x-real-ip")?,
    };
    ratelimit::ip_key(raw.to_str().ok()?.rsplit(',').next()?)
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
        assert_eq!(res.headers()["referrer-policy"], "strict-origin");

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

    /// Somebody else's site.
    const ELSEWHERE: &str = "https://evil.example";

    #[tokio::test]
    async fn a_post_from_somebody_else_s_page_is_refused_however_good_its_form_token() {
        // Login CSRF. The pre-session form token is minted by a `GET` that
        // is served to strangers, is good for fifteen minutes and is not
        // one-time — so an attacker holds a valid one by asking for it. It
        // proves that Scout exists, not that the `POST` came from Scout.
        //
        // With one harvested, the attacker asks for a link to their own
        // address and auto-submits the confirm form from a page the victim
        // visits. The victim's browser is handed a session cookie for the
        // *attacker's* account, and everything the victim attaches
        // afterwards — their address, their Telegram — lands there.
        let (app, core, _dir) = test_app().await;
        issue(&core, "tok-mallory").await;
        let harvested = form_token(&app, "/sign-in").await;

        let spent = post_with_headers(
            &app,
            "/auth/email",
            &format!("csrf={harvested}&t=tok-mallory"),
            &[("origin", ELSEWHERE)],
        )
        .await;
        assert_eq!(spent.status(), StatusCode::BAD_REQUEST, "a cross-site post was acted on");
        assert!(
            spent.headers().get("set-cookie").is_none(),
            "a cross-site post planted a session in the victim's browser"
        );
        // Refused *before acting*: the token is still there to spend.
        assert!(matches!(
            scout_core::identity::consume_token(&core, &hash("tok-mallory")).await.unwrap(),
            scout_core::identity::TokenOutcome::Valid { .. }
        ));

        // The other half of the same attack: a link mailed to whoever the
        // attacker names, from a form the visitor never saw.
        let asked = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={harvested}&email=mallory%40example.com"),
            &[("origin", ELSEWHERE)],
        )
        .await;
        assert_eq!(asked.status(), StatusCode::BAD_REQUEST);

        // `Referer` when there is no `Origin`: less to go on, and enough.
        let by_referer = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={harvested}&email=mallory%40example.com"),
            &[("referer", "https://evil.example/a-page-of-theirs")],
        )
        .await;
        assert_eq!(by_referer.status(), StatusCode::BAD_REQUEST);

        // A host that merely starts the same way is a different site.
        let lookalike = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={harvested}&email=mallory%40example.com"),
            &[("origin", "https://example.com.evil.example")],
        )
        .await;
        assert_eq!(lookalike.status(), StatusCode::BAD_REQUEST);

        // Our own page still posts. Without this the four refusals above
        // would pass against a route that refused everything.
        let ours = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={harvested}&email=ada%40example.com"),
            &[("origin", "https://example.com")],
        )
        .await;
        assert_eq!(ours.status(), StatusCode::OK);

        // A host is compared case-insensitively, which is how origins are
        // equal. Refusing this would refuse a real visitor.
        let shouting = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={harvested}&email=ada%40example.com"),
            &[("origin", "https://EXAMPLE.com")],
        )
        .await;
        assert_eq!(shouting.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_post_that_names_no_origin_at_all_is_refused() {
        // This is only correct because `security_headers` sends
        // `strict-origin` rather than `no-referrer`. Under `no-referrer`,
        // Fetch has the browser send `Origin: null` on a form post from our
        // own page — the rule covers requests that are neither GET nor HEAD
        // and not in `cors` mode, which is every form here — so a nameless
        // request was indistinguishable from our own and had to be allowed.
        // An attacker who set `no-referrer` on their page then looked
        // exactly like us, which is login CSRF for anyone who read the
        // spec.
        //
        // `null` and absent must read the same way, so both are here.
        // Changing the header back without changing `from_our_own_pages`
        // makes this test fail, which is the point of it.
        let (app, core, _dir) = test_app().await;
        issue(&core, "tok-quiet").await;
        let csrf = form_token(&app, "/auth/email?t=tok-quiet").await;

        let opaque = post_with_headers(
            &app,
            "/sign-in/email",
            &format!("csrf={csrf}&email=ada%40example.com"),
            &[("origin", "null")],
        )
        .await;
        assert_eq!(opaque.status(), StatusCode::BAD_REQUEST, "`Origin: null` was allowed");

        let silent = post_with_headers(
            &app,
            "/auth/email",
            &format!("csrf={csrf}&t=tok-quiet"),
            &[],
        )
        .await;
        assert_eq!(silent.status(), StatusCode::BAD_REQUEST, "a nameless POST was allowed");

        // Not vacuous: the same request naming our own origin succeeds, and
        // the token is still unspent for it to succeed with.
        let ours = post_form(&app, "/auth/email", &format!("csrf={csrf}&t=tok-quiet")).await;
        assert_eq!(ours.status(), StatusCode::SEE_OTHER);
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

    /// The `state` out of the widget's `data-auth-url` on a rendered page.
    ///
    /// Read back out of the real markup, like the form tokens above, so a
    /// page that stopped carrying one fails these tests rather than
    /// quietly stopping being checked. This is exactly the round trip the
    /// widget makes: Telegram appends its own fields to this URL.
    fn widget_state(html: &str) -> String {
        let (_, rest) = html.split_once(r#"data-auth-url=""#).unwrap_or_else(|| {
            panic!("the page draws no widget");
        });
        let url = rest.split_once('"').unwrap().0;
        url.split_once("state=")
            .unwrap_or_else(|| panic!("the widget's auth url carries no state: {url}"))
            .1
            .to_string()
    }

    /// The app with a bot name, so the widget — and its state — exists.
    async fn app_with_a_widget()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir) {
        test_app_named(Some("https://t.me/goodscoutbot")).await
    }

    #[tokio::test]
    async fn a_widget_payload_that_does_not_verify_says_only_that() {
        let (app, _core, _dir) = app_with_a_widget().await;
        let now = chrono::Utc::now().timestamp().to_string();
        // A real state off the real page, so what these cases exercise is
        // the payload's own checks and not the state.
        let state = widget_state(&body_of(get(&app, "/sign-in").await).await);

        // Edited after signing: the id is the field worth editing, since
        // it is the whole of who you are claiming to be.
        let forged = widget_query(&[("id", "777"), ("auth_date", &now)])
            .replace("id=777", "id=1");
        let res = get(&app, &format!("/auth/telegram?{forged}&state={state}")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(res.headers().get("set-cookie").is_none(), "a forged payload set a cookie");

        // An hour-old payload is genuinely signed and still refused.
        let old = (chrono::Utc::now().timestamp() - 3600).to_string();
        let replayed = widget_query(&[("id", "777"), ("auth_date", &old)]);
        let stale = get(&app, &format!("/auth/telegram?{replayed}&state={state}")).await;
        assert_eq!(stale.status(), StatusCode::BAD_REQUEST);

        // And a payload that is beyond reproach, arriving with no state at
        // all. Which of the four checks failed is not a thing somebody
        // assembling a request gets to learn, so this reads the same as the
        // other two — byte for byte.
        let genuine = widget_query(&[("id", "777"), ("auth_date", &now)]);
        let stateless = get(&app, &format!("/auth/telegram?{genuine}")).await;
        assert_eq!(stateless.status(), StatusCode::BAD_REQUEST);

        let (a, b, c) =
            (body_of(res).await, body_of(stale).await, body_of(stateless).await);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    /// The account behind a sign-in, admitted or queued.
    async fn account_for(
        core: &scout_core::core::Core,
        kind: &'static str,
        id: &'static str,
    ) -> i64 {
        match scout_core::identity::sign_in(core, kind, id).await.unwrap() {
            scout_core::identity::SignIn::In { account_id }
            | scout_core::identity::SignIn::Queued { account_id } => account_id,
        }
    }

    #[tokio::test]
    async fn a_widget_payload_the_victim_never_asked_for_does_not_attach_to_them() {
        // The takeover. `SameSite=Lax` sends the session cookie on a
        // top-level cross-site navigation, so an attacker who has just
        // pressed the real widget and copied their own freshly signed
        // payload out of the address bar can — inside the sixty seconds it
        // stays good — navigate a signed-in victim to
        // `/auth/telegram?<that payload>`. Every check the handler had to
        // make passed: the HMAC is Telegram's own and the session is the
        // victim's. The attacker's Telegram id ended up attached to the
        // victim's account, and pressing the widget again put the attacker
        // inside it.
        //
        // What was missing is the only thing neither party could forge: a
        // token minted by us, for this browser, on a page of ours.
        let (app, core, _dir) = test_app().await;
        let victim = account_for(&core, "email", "victim@example.com").await;
        let cookie = crate::session::mint(TEST_KEY, victim, 86_400);

        let now = chrono::Utc::now().timestamp().to_string();
        let attacker = widget_query(&[("id", "999"), ("first_name", "Mallory"), ("auth_date", &now)]);

        let res = get_with_cookie(&app, &format!("/auth/telegram?{attacker}"), &cookie).await;
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "a payload carrying no state of ours was acted on"
        );
        assert!(
            !scout_core::identity::standing(&core, victim).await.unwrap()
                .kinds.contains(&"telegram".to_string()),
            "a Telegram identity the victim never asked for was attached to their account"
        );
    }

    #[tokio::test]
    async fn a_state_minted_for_a_signed_out_browser_is_no_use_on_a_session() {
        // The state has to be bound to the session or it is a hoop the
        // attacker walks through on the way to the same takeover: anybody
        // may fetch `/sign-in`, so anybody may have a valid state.
        let (app, core, _dir) = app_with_a_widget().await;
        let harvested = widget_state(&body_of(get(&app, "/sign-in").await).await);

        let victim = account_for(&core, "email", "victim@example.com").await;
        let cookie = crate::session::mint(TEST_KEY, victim, 86_400);
        let now = chrono::Utc::now().timestamp().to_string();
        let attacker =
            widget_query(&[("id", "999"), ("first_name", "Mallory"), ("auth_date", &now)]);

        let res =
            get_with_cookie(&app, &format!("/auth/telegram?{attacker}&state={harvested}"), &cookie)
                .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a state for nobody worked on a session");
        assert!(
            !scout_core::identity::standing(&core, victim).await.unwrap()
                .kinds.contains(&"telegram".to_string()),
            "a Telegram identity the victim never asked for was attached to their account"
        );

        // Not vacuous: the state off the victim's *own* account page, on
        // the victim's own session, links. The refusal above is about
        // which browser the state was minted for and nothing else.
        let theirs =
            widget_state(&body_of(get_with_cookie(&app, "/account", &cookie).await).await);
        let asked =
            get_with_cookie(&app, &format!("/auth/telegram?{attacker}&state={theirs}"), &cookie)
                .await;
        assert_eq!(asked.status(), StatusCode::SEE_OTHER);
        assert_eq!(asked.headers()["location"], "/account?linked=yes");
    }

    #[tokio::test]
    async fn the_widget_signs_you_in_when_you_are_out_and_refuses_an_identity_in_use() {
        let (app, core, _dir) = app_with_a_widget().await;
        let now = chrono::Utc::now().timestamp().to_string();
        let payload = widget_query(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &now)]);

        // Signed out: a session, and the account page. The state is the
        // one the sign-in page's own button carries, which is the whole
        // journey a real press makes.
        let state = widget_state(&body_of(get(&app, "/sign-in").await).await);
        let res = get(&app, &format!("/auth/telegram?{payload}&state={state}")).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/account");
        let set = res.headers()["set-cookie"].to_str().unwrap().to_string();
        let value = set.split_once('=').unwrap().1.split_once(';').unwrap().0;
        let telegram_account = crate::session::verify(TEST_KEY, value)
            .expect("the cookie it set does not verify as a session");

        // Both accounts have to have been *used*, or they are two empty
        // halves of one person and linking merges them instead. That is
        // the case the next test covers; this one is the genuine clash.
        let other = scout_core::identity::sign_in(&core, "telegram", "888").await.unwrap();
        let (scout_core::identity::SignIn::In { account_id: other }
        | scout_core::identity::SignIn::Queued { account_id: other }) = other;
        // Logged against the accounts, not the Telegram ids: `log_request`
        // is account-keyed, and the two are different numbers.
        core.log_request(telegram_account, "text").await.unwrap();
        core.log_request(other, "text").await.unwrap();
        assert_ne!(other, telegram_account);
        let cookie = crate::session::mint(TEST_KEY, other, 86_400);

        // Minted rather than read off the page: `/account` draws no widget
        // for an account that already has Telegram (see `pages::account`),
        // so this account has no button to press. The state is the same
        // value that button would have carried, which is what the callback
        // actually checks.
        let state = crate::session::csrf_for(TEST_KEY, other);
        let linked =
            get_with_cookie(&app, &format!("/auth/telegram?{payload}&state={state}"), &cookie)
                .await;
        assert_eq!(linked.status(), StatusCode::SEE_OTHER);
        // Taken, not linked: 777 already belongs to an account that has
        // used Scout. Two real accounts stay two accounts.
        assert_eq!(linked.headers()["location"], "/account?linked=taken");
        assert!(linked.headers().get("set-cookie").is_none(), "linking minted a session");
        // `other` is itself a Telegram account, so "has a telegram kind"
        // proves nothing here. What must hold is that 777 still answers to
        // the account it already belonged to.
        assert_eq!(
            account_for(&core, "telegram", "777").await,
            telegram_account,
            "an identity owned by someone else was moved"
        );
    }

    #[tokio::test]
    async fn pressing_telegram_from_an_email_account_with_nothing_in_it_merges_the_two() {
        // The journey every early user takes: they already use Scout on
        // Telegram, they sign in on the web with an address Scout has never
        // seen — which mints a second, empty account — and then press the
        // Telegram button from it.
        let (app, core, _dir) = app_with_a_widget().await;
        let telegram_account =
            account_for(&core, "telegram", "777").await;
        core.log_request(telegram_account, "text").await.unwrap();

        // Signed in by email, as the empty account that sign-in minted.
        let web = account_for(&core, "email", "ada@example.com").await;
        assert_ne!(web, telegram_account);
        let cookie = crate::session::mint(TEST_KEY, web, 86_400);

        let now = chrono::Utc::now().timestamp().to_string();
        let payload = widget_query(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &now)]);
        let state =
            widget_state(&body_of(get_with_cookie(&app, "/account", &cookie).await).await);
        let merged =
            get_with_cookie(&app, &format!("/auth/telegram?{payload}&state={state}"), &cookie)
                .await;

        assert_eq!(merged.status(), StatusCode::SEE_OTHER);
        assert_eq!(merged.headers()["location"], "/account?linked=merged");

        // The session must have moved to the surviving account. Without
        // this the visitor walks away holding a cookie for an account that
        // has just been deleted, and every later page reads as a stranger.
        let set = merged
            .headers()
            .get("set-cookie")
            .expect("a merge that changed the account did not re-issue the session")
            .to_str()
            .unwrap()
            .to_string();
        let value = set.split_once('=').unwrap().1.split_once(';').unwrap().0;
        let now_signed_in_as = crate::session::verify(TEST_KEY, value)
            .expect("the re-issued cookie does not verify");
        assert_eq!(
            now_signed_in_as, telegram_account,
            "the session followed the empty account rather than the surviving one"
        );

        // And that one account answers to both.
        let standing = scout_core::identity::standing(&core, telegram_account).await.unwrap();
        assert_eq!(standing.kinds, vec!["email".to_string(), "telegram".to_string()]);
    }

    #[tokio::test]
    async fn the_login_widget_appears_only_when_the_bot_can_name_itself() {
        // `getMe` failed at start-up: no username, so no widget. A button
        // naming no bot draws nothing and one naming the wrong bot signs
        // people into it, so the honest answer is the email form alone.
        let (nameless, _core, _dir) = test_app().await;
        let page = body_of(get(&nameless, "/sign-in").await).await;
        assert!(!page.contains("telegram-widget.js"));

        let (app, _core, _dir) = app_with_a_widget().await;
        let page = body_of(get(&app, "/sign-in").await).await;
        assert!(page.contains("telegram-widget.js"), "the sign-in page has no widget");
        assert!(page.contains(r#"data-telegram-login="goodscoutbot""#));
        // Absolute, and pointing at the route that receives it: Telegram
        // refuses a relative one, and refuses it in its own popup where
        // our logs never see it.
        assert!(page.contains(r#"data-auth-url="https://example.com/auth/telegram?state="#));

        // And the state on it names nobody, because nobody is signed in.
        // Nothing here needs escaping — it is `mint`'s output, which is
        // digits, dots and base64url — but a state that did would arrive
        // back mangled, so this also says it survives the round trip.
        let state = widget_state(&page);
        assert!(crate::session::csrf_ok(TEST_KEY, &state), "the widget's state is not ours");
        assert!(
            !crate::session::csrf_ok_for(TEST_KEY, &state, 1),
            "a signed-out state named an account"
        );
        assert_eq!(crate::pages::escape(&state), state, "the state needed escaping");
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

        assert_eq!(headers["referrer-policy"], "strict-origin");
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
            // `/account` shows standing and a live form token, and the
            // `/auth/email` page's own URL carries a login token. A stored
            // copy of either is a credential someone else can press Back
            // to. Nothing this half serves wants caching, so it is asserted
            // on all of them rather than on the two that would hurt most.
            assert_eq!(res.headers()["cache-control"], "no-store", "{what}");
        }

        // And not on the public page, which has no script, no form and
        // nothing to steal. A policy it does not need is one that gets
        // loosened for a reason that was never about it.
        assert!(get(&app, "/").await.headers().get("content-security-policy").is_none());
    }

    /// An address, as a form body carries it.
    ///
    /// `+` means a space in a form encoding, so a tagged address written
    /// raw would arrive as `victim 1@gmail.com` and be refused as
    /// undeliverable — the test would pass for the wrong reason.
    fn encoded(email: &str) -> String {
        email.replace('+', "%2B").replace('@', "%40")
    }

    /// Asks for a link the way a browser would, from a named address.
    ///
    /// The answer is deliberately the same page whatever happens, so these
    /// tests count what went out rather than reading statuses.
    async fn ask(app: &axum::Router, csrf: &str, email: &str, forwarded: &[(&str, &str)]) {
        let mut headers = vec![("origin", "https://example.com")];
        headers.extend_from_slice(forwarded);
        let res = post_with_headers(
            app,
            "/sign-in/email",
            &format!("csrf={csrf}&email={}", encoded(email)),
            &headers,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "the form itself was refused");
    }

    /// How many messages actually went out.
    ///
    /// `mail_a_link` spawns, so the response comes back before the message
    /// does. Yielding rather than sleeping, for the reason `mailed_link`
    /// gives: on a current-thread runtime a spawned task runs at the next
    /// yield, and `Mailer::Kept` has no await inside it.
    async fn mail_sent(sent: &Sent) -> usize {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        let n = sent.lock().unwrap().len();
        n
    }

    #[tokio::test]
    async fn a_mail_bomb_cannot_buy_more_buckets_by_respelling_one_address() {
        // The per-address limit was per *string*. `victim+1@gmail.com` and
        // `v.i.c.t.i.m@gmail.com` are one inbox and were three buckets, so
        // "3 per 15 minutes" bounded nothing a targeted mail bomb cared
        // about: forty requests meant forty messages, capped only by the
        // per-IP limit that the next test is about.
        let (app, _core, _dir, sent) = test_app_keeping_mail().await;
        let csrf = form_token(&app, "/sign-in").await;
        let from = |ip| [("x-forwarded-for", ip)];

        for i in 0..40 {
            ask(&app, &csrf, &format!("victim+{i}@gmail.com"), &from("198.51.100.1")).await;
        }
        assert_eq!(mail_sent(&sent).await, 3, "the documented per-address cap did not hold");

        // A second IP, so what refuses these is the address and not the
        // address it came from. Dots are the other spelling of the same
        // inbox at this provider.
        for local in ["v.ictim", "vi.ctim", "vic.tim", "v.i.c.t.i.m"] {
            ask(&app, &csrf, &format!("{local}@gmail.com"), &from("198.51.100.2")).await;
        }
        assert_eq!(mail_sent(&sent).await, 3, "a dotted spelling bought a fresh bucket");

        // Not vacuous, twice over. A different person at the same provider
        // still gets their mail...
        ask(&app, &csrf, "ada@gmail.com", &from("198.51.100.3")).await;
        assert_eq!(mail_sent(&sent).await, 4);
        // ...and dots are only ignored where the provider ignores them:
        // these are two different people at a company that takes them
        // literally, and both are served.
        ask(&app, &csrf, "ada@example.com", &from("198.51.100.4")).await;
        ask(&app, &csrf, "a.da@example.com", &from("198.51.100.5")).await;
        assert_eq!(mail_sent(&sent).await, 6, "dots were stripped where they mean something");
    }

    #[tokio::test]
    async fn an_honest_visitor_is_still_allowed_exactly_what_is_documented() {
        // The positive control for all of the above: every refusal in this
        // file would pass against a sign-in form that mailed nothing at
        // all. Three per address per fifteen minutes, ten per IP per hour,
        // as the design says.
        let (app, _core, _dir, sent) = test_app_keeping_mail().await;
        let csrf = form_token(&app, "/sign-in").await;
        let home = [("x-forwarded-for", "203.0.113.4")];

        for _ in 0..10 {
            ask(&app, &csrf, "ada@example.com", &home).await;
        }
        assert_eq!(mail_sent(&sent).await, 3, "an honest visitor got the wrong number of links");

        // Ten per hour from one address, over distinct inboxes — a family
        // or an office behind one NAT, which is what the IP limit is for.
        for i in 0..20 {
            ask(&app, &csrf, &format!("person{i}@example.com"), &home).await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "the documented per-IP cap did not hold");
    }

    #[tokio::test]
    async fn one_ipv6_client_is_one_bucket_and_not_a_sixty_four_of_them() {
        // Any IPv6 client is handed at least a /64 — 18 quintillion
        // addresses — so keying on the full /128 gave one ordinary client
        // one bucket per request. Forty distinct addresses out of one /64
        // meant forty messages against a cap of ten.
        let (app, _core, _dir, sent) = test_app_keeping_mail().await;
        let csrf = form_token(&app, "/sign-in").await;

        for i in 0..40 {
            let ip = format!("2001:db8:1::{i:x}");
            ask(&app, &csrf, &format!("v{i}@example.com"), &[("x-forwarded-for", &ip)]).await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "a /64 was worth more than one client");

        // A different /64 is a different client, and is served.
        ask(&app, &csrf, "ada@example.com", &[("x-forwarded-for", "2001:db8:2::1")]).await;
        assert_eq!(mail_sent(&sent).await, 11);
    }

    #[tokio::test]
    async fn a_forwarded_header_the_client_wrote_does_not_buy_a_bucket() {
        // `X-Forwarded-For` is appended to, so the *first* entry is
        // whatever the client claimed and the last is what our own proxy
        // saw. Reading the first meant a client could mint a new bucket per
        // request by writing a new address in front of it — the ten-an-hour
        // cap, with an unlimited supply of hours.
        let (app, _core, _dir, sent) = test_app_keeping_mail().await;
        let csrf = form_token(&app, "/sign-in").await;

        for i in 0..40 {
            let spoofed = format!("192.0.2.{i}, 203.0.113.9");
            ask(&app, &csrf, &format!("v{i}@example.com"), &[("x-forwarded-for", &spoofed)]).await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "a client-written entry was counted on");

        // A second header line is the same trick with more layers: the
        // nearest proxy wrote the last one.
        for i in 40..60 {
            let ip = format!("192.0.2.{i}");
            ask(
                &app,
                &csrf,
                &format!("v{i}@example.com"),
                &[("x-forwarded-for", &ip), ("x-forwarded-for", "203.0.113.9")],
            )
            .await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "a second header line bought a fresh bucket");

        // Not vacuous: a request the proxy really did forward from
        // somewhere else is served.
        ask(&app, &csrf, "ada@example.com", &[("x-forwarded-for", "203.0.113.10")]).await;
        assert_eq!(mail_sent(&sent).await, 11);
    }

    #[tokio::test]
    async fn requests_with_no_forwarded_address_share_one_bucket() {
        // `is_none_or` used to make a missing header mean "allowed", so a
        // path that reached us without one had no IP limit at all. Behind
        // the ingress every request carries the header; a request without
        // one means something is misconfigured, and misconfigured must not
        // be the state in which the mail limit switches off.
        let (app, _core, _dir, sent) = test_app_keeping_mail().await;
        let csrf = form_token(&app, "/sign-in").await;

        for i in 0..40 {
            ask(&app, &csrf, &format!("v{i}@example.com"), &[]).await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "a missing header meant no limit");

        // Junk falls in the same bucket rather than becoming one of its
        // own, or a header full of nonsense buys a bucket per nonsense.
        for i in 40..60 {
            let junk = format!("not-an-address-{i}");
            ask(&app, &csrf, &format!("v{i}@example.com"), &[("x-forwarded-for", &junk)]).await;
        }
        assert_eq!(mail_sent(&sent).await, 10, "an unparseable header bought a bucket");

        // Not vacuous: a real forwarded address is a different bucket and
        // is served.
        ask(&app, &csrf, "ada@example.com", &[("x-forwarded-for", "203.0.113.11")]).await;
        assert_eq!(mail_sent(&sent).await, 11);
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
