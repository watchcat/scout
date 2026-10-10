//! `/join/{token}`: what a share link opens. Public — the visitor may not
//! have an account yet — and the one page that admits someone to Scout
//! outside an invite round, which is what a share link is for.

use super::{see_other, signed_in_as, sorry};
use crate::routes::trips::is_link_token;
use crate::{pages, session, AuthState};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use scout_core::sharing::{Joined, LinkState};

/// Set by the join page for a visitor who is not signed in, so that
/// signing in brings them back to it. Only ever read as a token's shape.
pub(crate) const JOIN_COOKIE: &str = "__Host-scout_join";

pub fn routes(auth: AuthState) -> Router {
    Router::new().route("/join/{token}", get(page).post(join)).with_state(auth)
}

/// Where signing in lands: back on the join page the visitor came from,
/// when its cookie holds a token's shape, else the chat. The path is built
/// from the token, so the cookie cannot point anywhere else.
pub(crate) fn after_sign_in(headers: &HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|jar| session::read_cookie(jar, JOIN_COOKIE))
        .filter(|t| is_link_token(t))
        .map(|t| format!("/join/{t}"))
        .unwrap_or_else(|| "/chat".to_string())
}

#[derive(serde::Deserialize, Default)]
struct Surface {
    #[serde(rename = "in")]
    surface: Option<String>,
}

fn dates(preview: &scout_core::sharing::Preview) -> String {
    match (&preview.first, &preview.last) {
        (Some(first), Some(last)) if first != last => format!("{first} to {last}."),
        (Some(first), _) => format!("{first}."),
        _ => String::new(),
    }
}

fn dead(state: &LinkState, owner: &str) -> Response {
    let (headline, sentence) = match state {
        LinkState::Expired => ("This link has expired", format!("Links last 14 days. Ask {owner} for a new one.")),
        LinkState::Off => ("This link was turned off", format!("Ask {owner} for a new one.")),
        _ => ("This link does not work", "Check that you copied all of it, or ask for a new one.".to_string()),
    };
    (StatusCode::NOT_FOUND, Html(pages::join_dead(headline, &sentence))).into_response()
}

/// Inside the Mini App the page is framed by Telegram, which only the
/// Mini App's policy allows.
fn framed(response: Response, in_telegram: bool) -> Response {
    if in_telegram {
        super::telegram_app::framable(response)
    } else {
        response
    }
}

fn trip_path(trip: &str, in_telegram: bool) -> String {
    let trip: String = form_urlencoded::byte_serialize(trip.as_bytes()).collect();
    if in_telegram {
        format!("/chat?in=telegram&trip={trip}")
    } else {
        format!("/chat?trip={trip}")
    }
}

async fn page(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(q): Query<Surface>,
) -> Response {
    let in_telegram = q.surface.as_deref() == Some("telegram");
    if !is_link_token(&token) {
        return framed(dead(&LinkState::Unknown, "whoever sent it"), in_telegram);
    }
    let preview = match scout_core::sharing::preview(&auth.core, &token).await {
        Ok(Some(preview)) => preview,
        Ok(None) => return framed(dead(&LinkState::Unknown, "whoever sent it"), in_telegram),
        Err(e) => {
            tracing::error!(error = %e, "could not read a share link");
            return sorry();
        }
    };
    let owner = preview.people.first().cloned().unwrap_or_else(|| "the owner".into());
    if !matches!(preview.state, LinkState::Live(_)) {
        return framed(dead(&preview.state, &owner), in_telegram);
    }
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        let widget = super::auth::widget(&auth, None);
        let html = pages::join_signed_out(
            &preview.trip,
            &dates(&preview),
            &preview.people,
            &session::csrf(&auth.cfg.session_key),
            widget.as_deref(),
        );
        let cookie = format!("{JOIN_COOKIE}={token}; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Lax");
        return framed(([(header::SET_COOKIE, cookie)], Html(html)).into_response(), in_telegram);
    };
    // Already on this trip — not merely on one of the same name — goes
    // straight to it: the same people, in the same order, with them in it.
    if let Ok(Some(plan)) = scout_core::trips::find(&auth.core, account_id, &preview.trip).await {
        let same = plan.people.iter().map(|p| &p.name).eq(preview.people.iter());
        if same && plan.people.iter().any(|p| p.you) {
            return see_other(&trip_path(&plan.trip.name, in_telegram));
        }
    }
    let name = scout_core::sharing::suggested_name(&auth.core, account_id)
        .await
        .unwrap_or_else(|_| "Traveller".into());
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    let html = pages::join_form(&preview.trip, &token, &csrf, &name, None, None, in_telegram);
    framed(Html(html).into_response(), in_telegram)
}

#[derive(serde::Deserialize)]
struct JoinIn {
    csrf: String,
    name: String,
    rename: Option<String>,
    #[serde(rename = "in")]
    surface: Option<String>,
}

async fn join(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<JoinIn>,
) -> Response {
    let in_telegram = form.surface.as_deref() == Some("telegram");
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        return see_other(&format!("/join/{}", if is_link_token(&token) { token.as_str() } else { "" }));
    };
    if !is_link_token(&token) || !session::csrf_ok_for(&auth.cfg.session_key, &form.csrf, account_id) {
        return framed((StatusCode::BAD_REQUEST, Html(pages::stale_form())).into_response(), in_telegram);
    }
    let joined =
        match scout_core::sharing::join(&auth.core, account_id, &token, &form.name, form.rename.as_deref()).await {
            Ok(joined) => joined,
            Err(e) => {
                tracing::error!(error = %e, account_id, "could not join a trip");
                return sorry();
            }
        };
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    let again = |trip: &str, clash: &str, problem: Option<&str>| {
        let html = pages::join_form(trip, &token, &csrf, &form.name, Some(clash), problem, in_telegram);
        framed(Html(html).into_response(), in_telegram)
    };
    let refused = |status: StatusCode, headline: &str, sentence: &str| {
        framed((status, Html(pages::join_dead(headline, sentence))).into_response(), in_telegram)
    };
    match joined {
        Joined::Joined { trip, seated } => {
            tracing::info!(account_id, seated, "joined a shared trip");
            let mut response = see_other(&trip_path(&trip, in_telegram));
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_static("__Host-scout_join=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax"),
            );
            response
        }
        Joined::Already { trip } => see_other(&trip_path(&trip, in_telegram)),
        Joined::Link(state) => framed(dead(&state, "the person who sent it"), in_telegram),
        Joined::Revoked => refused(
            StatusCode::FORBIDDEN,
            "You no longer have access to Scout",
            "Your seat was removed, so a link cannot add you.",
        ),
        Joined::Full => refused(
            StatusCode::FORBIDDEN,
            "This link is full",
            "It has let in the 10 new people it can. Ask the person who sent it for a new one.",
        ),
        Joined::Clash { name, yours: true } => again(&name, &name, None),
        Joined::Clash { name, yours: false } => refused(
            StatusCode::CONFLICT,
            "You are already on a trip with this name",
            &format!("Leave {name} first, then open this link again."),
        ),
        Joined::RenameTaken { name } => {
            again(&name, &name, Some("You already have a trip with that name too. Pick another."))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn shared() -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir, String) {
        let (app, core, dir) = test_app().await;
        // One seat, which the owner takes: anyone else signing in after is
        // queued, and only the link can let them in.
        open_round(&core, "autumn", 1).await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "777").await.unwrap()
        else {
            panic!("the open round should admit the account");
        };
        scout_core::trips::seed_trip_for_tests(&core, account_id, "October").await.unwrap();
        let link = scout_core::sharing::link(&core, account_id, "October", "j".repeat(22)).await.unwrap().unwrap();
        (app, core, dir, link.token)
    }

    #[tokio::test]
    async fn a_stranger_sees_the_trip_and_both_ways_in_and_is_remembered_for_after() {
        let (app, _core, _dir, token) = shared().await;
        let res = app.oneshot(Request::get(format!("/join/{token}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res.headers()["set-cookie"].to_str().unwrap().to_string();
        assert!(cookie.starts_with(&format!("__Host-scout_join={token};")), "{cookie}");
        let page = body_of(res).await;
        assert!(page.contains("Join October"));
        assert!(page.contains("action=\"/sign-in/email\""));
    }

    #[test]
    fn signing_in_with_the_join_cookie_returns_to_the_join_page_and_nowhere_else() {
        let with = |value: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("cookie", format!("__Host-scout_join={value}").parse().unwrap());
            h
        };
        assert_eq!(super::after_sign_in(&with(&"j".repeat(22))), format!("/join/{}", "j".repeat(22)));
        assert_eq!(super::after_sign_in(&with("..%2F..%2Fevil")), "/chat");
        assert_eq!(super::after_sign_in(&axum::http::HeaderMap::new()), "/chat");
    }

    #[tokio::test]
    async fn a_signed_in_newcomer_joins_takes_a_seat_and_lands_on_the_trip() {
        let (app, core, _dir, token) = shared().await;
        let scout_core::identity::SignIn::Queued { account_id: stranger } =
            scout_core::identity::sign_in(&core, "telegram", "5150").await.unwrap()
        else {
            panic!("the round is full, so a newcomer waits");
        };
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);
        let page = app
            .clone()
            .oneshot(
                Request::get(format!("/join/{token}"))
                    .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(body_of(page).await.contains("Join trip"));
        let res = app
            .clone()
            .oneshot(
                Request::post(format!("/join/{token}"))
                    .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                    .header("origin", "https://example.com")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}&name=Masha")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/chat?trip=October");
        assert!(scout_core::identity::standing(&core, stranger).await.unwrap().member, "the link gave a seat");
        // Opening the link again goes straight to the trip.
        let again = app
            .oneshot(
                Request::get(format!("/join/{token}"))
                    .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(again.headers()["location"], "/chat?trip=October");
    }

    #[tokio::test]
    async fn a_dead_link_says_so_and_offers_no_sign_in() {
        let (app, _core, _dir, _token) = shared().await;
        let res =
            app.oneshot(Request::get(format!("/join/{}", "z".repeat(22))).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let page = body_of(res).await;
        assert!(page.contains("This link does not work"));
        assert!(!page.contains("/sign-in/email"));
    }
}
