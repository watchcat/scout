//! The chat page: the seat that pays for it, and what was already said.
//!
//! `POST /chat/ask` and `POST /chat/reset` are a later task's — this file
//! is the read-only half: the page itself, the script it loads, and the
//! transcript it starts from.

use super::{see_other, signed_in_as, sorry};
use crate::{session, AuthState};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use scout_core::identity;

const TEMPLATE: &str = include_str!("../chat.html");
const CLIENT: &str = include_str!("../chat.js");
const CSRF_TOKEN: &str = "<!--CSRF-->";

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat", get(chat))
        .route("/chat.js", get(client))
        .route("/chat/history", get(history))
        // Nothing here changes state yet, but the layer goes on with the
        // routes rather than waiting for the first `POST` that needs it —
        // the same reason it is on `account::routes` rather than threaded
        // through individual handlers.
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

/// The account this request may spend model calls as, or the redirect that
/// says why not.
///
/// Shared between `chat` and `history`: both refuse a signed-out visitor
/// and a queued one identically, and a page that gated one way while its
/// own history endpoint gated another would be a door with two locks that
/// disagree.
async fn admitted_account(auth: &AuthState, headers: &HeaderMap) -> Result<i64, Response> {
    let Some(account_id) = signed_in_as(auth, headers) else {
        return Err(see_other("/sign-in"));
    };
    let standing = match identity::standing(&auth.core, account_id).await {
        Ok(standing) => standing,
        Err(e) => {
            tracing::error!(error = %e, "could not read an account's standing");
            return Err(sorry());
        }
    };
    if !standing.member {
        // The chat costs real model calls, and a queued account has not
        // been admitted to spend them. `/account` already explains where
        // they stand, so it does the explaining.
        return Err(see_other("/account"));
    }
    Ok(account_id)
}

async fn chat(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    Html(TEMPLATE.replace(CSRF_TOKEN, &csrf)).into_response()
}

async fn client() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], CLIENT)
}

async fn history(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::transcript(&auth.core, account_id).await {
        Ok(turns) => axum::Json(turns).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not read a transcript");
            sorry()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::http::StatusCode;

    const DAY: i64 = 86_400;

    /// `test_app`, with a round open so a sign-in can actually admit
    /// someone rather than queuing them.
    async fn test_app_with_a_round()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let (app, core, dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        (app, core, dir)
    }

    /// Signs in a Telegram id against an open round and returns the
    /// account id, panicking if the round had no room — every test that
    /// calls this one wants a member, not a queued visitor.
    async fn admitted(core: &scout_core::core::Core, telegram_id: &str) -> i64 {
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(core, "telegram", telegram_id).await.unwrap()
        else {
            panic!("the round has room, so this should have admitted");
        };
        account_id
    }

    /// Seeds a two-message exchange under the `"direct"` scope, the same
    /// scope `/chat` reads from.
    ///
    /// `scout_core::session::save_history` is `pub(crate)` to scout-core,
    /// and so is `Core::store()` — the module that holds `Store` is not
    /// `pub` at all, so there is no path from here to write a message
    /// directly. `scout_core::session::seed_exchange` was added to close
    /// that gap: it is the one door through `Store` that this crate did
    /// not already have, kept narrow (an exchange, not a `Store` handle)
    /// so the privacy boundary the crate's top-level doc comment describes
    /// stays intact everywhere else.
    async fn seed_conversation(core: &scout_core::core::Core, account_id: i64, you: &str, scout: &str) {
        scout_core::session::seed_exchange(core, account_id, "direct", you, scout).await.unwrap();
    }

    #[tokio::test]
    async fn a_signed_out_visitor_is_sent_to_sign_in() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/chat").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/sign-in");
    }

    #[tokio::test]
    async fn someone_still_waiting_is_sent_to_the_page_that_says_so() {
        // The chat costs real model calls, and a queued account has not
        // been admitted to spend them. `/account` already explains where
        // they stand, so it does the explaining.
        let (app, core, _dir) = test_app().await;
        let scout_core::identity::SignIn::Queued { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "777").await.unwrap()
        else {
            panic!("no round is open, so this should have queued");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let res = get_with_cookie(&app, "/chat", &cookie).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/account");
    }

    #[tokio::test]
    async fn a_member_gets_a_page_that_loads_its_script_from_us() {
        // The CSP has no 'unsafe-inline' for scripts, so an inline block
        // would be refused by the browser and the server would never know.
        // This test is the thing that notices.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(page.contains(r#"src="/chat.js""#), "the page loads no client");
        assert!(!page.contains("<script>"), "an inline script would be refused by our own CSP");
    }

    #[tokio::test]
    async fn the_client_script_is_served_as_javascript() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/chat.js").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()[axum::http::header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/javascript"),
            "a module served as the wrong type is refused by the browser"
        );
    }

    #[tokio::test]
    async fn history_is_empty_for_someone_who_has_never_spoken_and_creates_nothing() {
        // A page visit must not write rows.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let body = body_of(get_with_cookie(&app, "/chat/history", &cookie).await).await;
        assert_eq!(body, "[]");

        assert!(
            scout_core::session::transcript(&core, account_id).await.unwrap().is_empty(),
            "asking for history minted a conversation"
        );
    }

    #[tokio::test]
    async fn history_returns_what_was_said() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let body = body_of(get_with_cookie(&app, "/chat/history", &cookie).await).await;
        assert!(body.contains("cheapest beans"), "got: {body}");
        assert!(body.contains(r#""You""#), "the role is not on the wire: {body}");
    }
}
