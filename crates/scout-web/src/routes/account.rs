//! The page a signed-in visitor lands on, and the two things they can do
//! from it: attach the way in they do not have yet, and leave.
//!
//! Every `POST` here happens while there is a session, so unlike the forms
//! in `auth.rs` the form token is minted for a particular account and
//! checked against that account — see `session::csrf_for`. An unbound
//! token would be one anybody could fetch from the public `/sign-in` page
//! and then spend on somebody else's behalf.

use super::{see_other, signed_in_as, sorry, stale_form};
use crate::{pages, session, AuthState};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use scout_core::identity;
use serde::Deserialize;

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/account", get(account))
        .route("/account/link/email", post(link_email))
        .route("/sign-out", post(sign_out))
        // The same layer the other half carries: a `POST` from somebody
        // else's page is refused before the handler sees it.
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

#[derive(Deserialize)]
struct Landed {
    /// How the attempt that redirected here went, if it came from one.
    #[serde(default)]
    linked: String,
}

async fn account(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Query(landed): Query<Landed>,
) -> Response {
    // No cookie, a forged one and an expired one are one case, which is
    // what makes "signed out" and "tampered with" indistinguishable.
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        return see_other("/sign-in");
    };

    let standing = match identity::standing(&auth.core, account_id).await {
        Ok(standing) => standing,
        Err(e) => {
            tracing::error!(error = %e, "could not read an account's standing");
            return sorry();
        }
    };

    // Bound to this account, so it is no use on anyone else's session.
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    // The button on this page is drawn for this account, and the state in
    // its `data-auth-url` names it — see `auth::widget`.
    let widget = super::auth::widget(&auth, Some(account_id));

    Html(pages::account(&pages::Account {
        member: standing.member,
        kinds: &standing.kinds,
        chat_url: auth.core.return_url(),
        widget: widget.as_deref(),
        csrf: &csrf,
        note: note(&landed.linked),
    }))
    .into_response()
}

/// The sentence for an outcome carried back from `/auth/telegram` or from
/// the button on an emailed link.
///
/// Matched against a fixed set and never echoed. The value arrives in a
/// query string, which is to say from whoever wrote the link — rendering
/// it back would let someone mail a member `/account?linked=` followed by
/// a sentence of their own choosing, on our page, under our name. An
/// unrecognised value says nothing at all rather than guessing.
fn note(linked: &str) -> Option<&'static str> {
    match linked {
        "yes" => Some("Telegram is linked. You can sign in either way now."),
        "already" => Some("That Telegram account was already this one."),
        "taken" => Some(
            "That Telegram account belongs to a different Scout account, \
             so nothing was changed.",
        ),
        // The same three, for an address attached by spending a link. Told
        // apart from the Telegram three by name rather than by a second
        // parameter, because the value has to survive a redirect either
        // way and one query string is easier to read than two.
        "email" => Some("That address is linked. You can sign in either way now."),
        "email-already" => Some("That address was already this one."),
        "email-taken" => Some(
            "That address belongs to a different Scout account, \
             so nothing was changed.",
        ),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Requested {
    #[serde(default)]
    email: String,
    #[serde(default)]
    csrf: String,
}

/// Asks for a link while signed in, so the token carries the account and
/// spending it attaches the address instead of minting a second account.
async fn link_email(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Form(form): Form<Requested>,
) -> Response {
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        return see_other("/sign-in");
    };
    if !session::csrf_ok_for(&auth.cfg.session_key, &form.csrf, account_id) {
        return stale_form();
    }
    super::auth::offer_a_link(&auth, &headers, &form.email, Some(account_id)).await
}

#[derive(Deserialize)]
struct Token {
    #[serde(default)]
    csrf: String,
}

/// Ends the session by removing the cookie that is the whole of it.
///
/// Nothing is deleted server-side because there is nothing there: a
/// session is a signature, not a row (see `session.rs`). The consequence
/// is that a cookie already copied elsewhere keeps working until it
/// expires, and this button cannot change that — rotating
/// `SCOUT_SESSION_KEY` is what can.
async fn sign_out(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Form(form): Form<Token>,
) -> Response {
    match signed_in_as(&auth, &headers) {
        // A token bound to somebody else's account does not sign this one
        // out. Being logged out by a forged form is a nuisance rather than
        // a breach, but the binding is free and this is where it is
        // demonstrated.
        Some(account_id) if !session::csrf_ok_for(&auth.cfg.session_key, &form.csrf, account_id) => {
            return stale_form()
        }
        _ => {}
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/".to_string()),
            (header::SET_COOKIE, session::clear_cookie()),
        ],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::http::StatusCode;
    use scout_core::identity::{self, SignIn};

    const DAY: i64 = 86_400;

    /// The form token out of the rendered account page, read back out of
    /// the markup so these tests go through the same door a browser does.
    fn hidden(html: &str) -> String {
        let (_, rest) = html
            .split_once(r#"name="csrf" value=""#)
            .expect("the account page has no hidden `csrf` field");
        rest.split_once('"').unwrap().0.to_string()
    }

    async fn form_token(app: &axum::Router, cookie: &str) -> String {
        hidden(&body_of(get_with_cookie(app, "/account", cookie).await).await)
    }

    #[tokio::test]
    async fn signing_in_at_an_open_round_admits_and_a_full_one_queues() {
        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 1).await;

        let first = identity::sign_in(&core, "email", "a@example.com").await.unwrap();
        assert!(matches!(first, SignIn::In { .. }));

        // Capacity is one, so the next person queues rather than being
        // turned away with nothing — the dead end W1 left, closed.
        let second = identity::sign_in(&core, "email", "b@example.com").await.unwrap();
        assert!(matches!(second, SignIn::Queued { .. }));

        // And the account page says which is which, which is the whole of
        // what a queued person gets today.
        let SignIn::Queued { account_id } = second else { unreachable!() };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let page = body_of(get_with_cookie(&_app, "/account", &cookie).await).await;
        assert!(page.contains("on the list"), "a queued visitor was not told so");
    }

    #[tokio::test]
    async fn account_needs_a_session_and_sign_out_ends_it() {
        let (app, core, _dir) = test_app().await;
        assert_eq!(get(&app, "/account").await.status(), StatusCode::SEE_OTHER);
        assert_eq!(get(&app, "/account").await.headers()["location"], "/sign-in");

        // A cookie we did not sign is no cookie at all — the same 303, so
        // "signed out" and "tampered with" are indistinguishable.
        let forged = crate::session::mint(b"somebody else's key entirely", 1, DAY);
        assert_eq!(
            get_with_cookie(&app, "/account", &forged).await.status(),
            StatusCode::SEE_OTHER
        );

        let SignIn::Queued { account_id } =
            identity::sign_in(&core, "email", "a@example.com").await.unwrap()
        else {
            panic!("no round is open, so this should have queued");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        assert_eq!(get_with_cookie(&app, "/account", &cookie).await.status(), StatusCode::OK);

        let csrf = form_token(&app, &cookie).await;
        let out = post_with_cookie(&app, "/sign-out", &cookie, &format!("csrf={csrf}")).await;
        assert_eq!(out.status(), StatusCode::SEE_OTHER);
        assert_eq!(out.headers()["location"], "/");
        let set = out.headers()["set-cookie"].to_str().unwrap();
        assert!(set.contains("Max-Age=0"), "sign out did not clear the cookie");
        // The name on the wire, not just the constant: a browser refuses a
        // `__Host-` cookie that is missing any of `Secure`, `Path=/` or a
        // `Domain`, and refusing this one means the session survives sign
        // out. `session.rs` pins the three attributes; this pins that the
        // header a real response emits is the prefixed one.
        assert!(set.starts_with("__Host-scout_session="), "the emitted cookie lost its prefix");
    }

    #[tokio::test]
    async fn a_form_token_from_one_session_does_nothing_to_another() {
        // The attack the binding closes: `/sign-in` is public, so anybody
        // can fetch a form token from it. Unbound, that token would pass
        // the check on every signed-in POST, for every account.
        let (app, core, _dir) = test_app().await;
        identity::sign_in(&core, "email", "mine@example.com").await.unwrap();
        identity::sign_in(&core, "email", "theirs@example.com").await.unwrap();
        let mine = crate::session::mint(TEST_KEY, 1, DAY);
        let theirs = crate::session::mint(TEST_KEY, 2, DAY);

        let anonymous = {
            let page = body_of(get(&app, "/sign-in").await).await;
            let (_, rest) = page.split_once(r#"name="csrf" value=""#).unwrap();
            rest.split_once('"').unwrap().0.to_string()
        };
        let hers = form_token(&app, &theirs).await;

        for token in [&anonymous, &hers] {
            let out = post_with_cookie(&app, "/sign-out", &mine, &format!("csrf={token}")).await;
            assert_eq!(out.status(), StatusCode::BAD_REQUEST, "a token from elsewhere signed us out");
            assert!(out.headers().get("set-cookie").is_none(), "the cookie was cleared anyway");

            let linked = post_with_cookie(
                &app,
                "/account/link/email",
                &mine,
                &format!("csrf={token}&email=new%40example.com"),
            )
            .await;
            assert_eq!(linked.status(), StatusCode::BAD_REQUEST);
        }

        // Our own token still works, so the tests above are not passing
        // because everything is refused.
        let ours = form_token(&app, &mine).await;
        let out = post_with_cookie(&app, "/sign-out", &mine, &format!("csrf={ours}")).await;
        assert_eq!(out.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn linking_an_address_while_signed_in_attaches_it_rather_than_making_an_account() {
        // The whole point of `issue_token`'s `account_id`: the link mailed
        // from `/account` must attach the address to the account that
        // asked, not mint a second one that cannot see the first.
        let (app, core, _dir, sent) = test_app_keeping_mail().await;
        open_round(&core, "autumn", 5).await;
        let SignIn::In { account_id } =
            identity::sign_in(&core, "telegram", "777").await.unwrap()
        else {
            panic!("the round has room");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let csrf = form_token(&app, &cookie).await;
        let asked = post_with_cookie(
            &app,
            "/account/link/email",
            &cookie,
            &format!("csrf={csrf}&email=Ada%40Example.com"),
        )
        .await;
        assert_eq!(asked.status(), StatusCode::OK);

        // Follow the link the way its recipient would.
        let link = mailed_link(&sent).await;
        let token = link.split_once("?t=").expect("the mail carries no token").1.to_string();
        let confirm_csrf = {
            let page = body_of(get(&app, &format!("/auth/email?t={token}")).await).await;
            let (_, rest) = page.split_once(r#"name="csrf" value=""#).unwrap();
            rest.split_once('"').unwrap().0.to_string()
        };
        let spent =
            post_form(&app, "/auth/email", &format!("csrf={confirm_csrf}&t={token}")).await;

        // No new session: they had one. And the address is now theirs,
        // lower-cased on the way in so `Ada@` and `ada@` are one identity.
        assert_eq!(spent.status(), StatusCode::SEE_OTHER);
        assert_eq!(spent.headers()["location"], "/account?linked=email");
        assert!(spent.headers().get("set-cookie").is_none(), "linking minted a session");
        assert_eq!(
            identity::standing(&core, account_id).await.unwrap().kinds,
            vec!["email".to_string(), "telegram".to_string()]
        );
    }

    #[tokio::test]
    async fn an_address_that_is_somebody_else_s_says_so_instead_of_looking_like_a_success() {
        // The button spends the token whatever the answer, so a silent
        // redirect leaves a member on a page with no address on it, no
        // sentence saying why, and a link that will not work twice.
        let (app, core, _dir, sent) = test_app_keeping_mail().await;
        // The address already proves somebody else's account.
        identity::sign_in(&core, "email", "taken@example.com").await.unwrap();
        let SignIn::Queued { account_id } =
            identity::sign_in(&core, "telegram", "777").await.unwrap()
        else {
            panic!("no round is open, so this should have queued");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let csrf = form_token(&app, &cookie).await;
        post_with_cookie(
            &app,
            "/account/link/email",
            &cookie,
            &format!("csrf={csrf}&email=taken%40example.com"),
        )
        .await;

        let link = mailed_link(&sent).await;
        let token = link.split_once("?t=").expect("the mail carries no token").1.to_string();
        let page = body_of(get(&app, &format!("/auth/email?t={token}")).await).await;
        let (_, rest) = page.split_once(r#"name="csrf" value=""#).unwrap();
        let confirm_csrf = rest.split_once('"').unwrap().0.to_string();
        let spent =
            post_form(&app, "/auth/email", &format!("csrf={confirm_csrf}&t={token}")).await;

        assert_eq!(spent.status(), StatusCode::SEE_OTHER);
        assert_eq!(spent.headers()["location"], "/account?linked=email-taken");
        // And the page it lands on actually says it, rather than the note
        // being a value nothing renders.
        let landed =
            body_of(get_with_cookie(&app, "/account?linked=email-taken", &cookie).await).await;
        assert!(landed.contains("different Scout account"), "the refusal was not shown");

        // The identity has not moved: two sign-ups stay two accounts.
        assert_eq!(
            identity::standing(&core, account_id).await.unwrap().kinds,
            vec!["telegram".to_string()]
        );
    }

    #[tokio::test]
    async fn a_post_from_another_site_does_not_reach_this_half_either() {
        // The layer is on both routers, and this is what says so. Being
        // signed out by somebody else's page is a nuisance; having a link
        // mailed from your account to an address you did not type is not.
        let (app, core, _dir) = test_app().await;
        identity::sign_in(&core, "email", "mine@example.com").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, 1, DAY);
        let csrf = form_token(&app, &cookie).await;
        let jar = format!("{}={cookie}", crate::session::COOKIE);
        let elsewhere = [("cookie", jar.as_str()), ("origin", "https://evil.example")];

        let out = post_with_headers(&app, "/sign-out", &format!("csrf={csrf}"), &elsewhere).await;
        assert_eq!(out.status(), StatusCode::BAD_REQUEST);
        assert!(out.headers().get("set-cookie").is_none(), "a cross-site post cleared the cookie");

        let linked = post_with_headers(
            &app,
            "/account/link/email",
            &format!("csrf={csrf}&email=mallory%40example.com"),
            &elsewhere,
        )
        .await;
        assert_eq!(linked.status(), StatusCode::BAD_REQUEST);

        // From our own page, with the same token, both still work — so the
        // refusals above are about where the post came from.
        let ours = [("cookie", jar.as_str()), ("origin", "https://example.com")];
        let asked = post_with_headers(
            &app,
            "/account/link/email",
            &format!("csrf={csrf}&email=ada%40example.com"),
            &ours,
        )
        .await;
        assert_eq!(asked.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unsigned_in_visitor_cannot_ask_for_a_link_from_the_account_page() {
        // Not a security hole so much as a route that would otherwise
        // mint tokens for nobody, and answer as though it had not.
        let (app, _core, _dir) = test_app().await;
        let res = post_form(&app, "/account/link/email", "csrf=x&email=a%40example.com").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/sign-in");
    }

    #[tokio::test]
    async fn the_outcome_in_the_query_string_is_one_of_ours_or_nothing() {
        // `/auth/telegram` redirects here with `?linked=`. That value is
        // in a URL, which means anyone can write it — and mail it to a
        // member. Only a name we recognise says anything.
        assert!(super::note("yes").unwrap().contains("linked"));
        assert_eq!(super::note("<b>you have been hacked</b>"), None);
        assert_eq!(super::note(""), None);

        let (app, core, _dir) = test_app().await;
        identity::sign_in(&core, "email", "a@example.com").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, 1, DAY);
        let page = body_of(
            get_with_cookie(&app, "/account?linked=%3Cscript%3Ealert(1)%3C%2Fscript%3E", &cookie)
                .await,
        )
        .await;
        assert!(!page.contains("<script>alert"), "the query string reached the page");
    }
}
