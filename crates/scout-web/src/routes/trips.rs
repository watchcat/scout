//! The visual trip planner's small authenticated web surface.
//!
//! It reads the itinerary Scout already stores, lets the traveller keep it,
//! settle an existing candidate, and add or remove a leg. Searching and
//! pricing still happen in chat, where the flight agent can validate live
//! provider data — those spend money against a live provider and stay a
//! flight-agent responsibility.

use super::chat::{admitted_account, csrf_header_ok};
use super::sorry;
use crate::{trip_pdf, AuthState};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat/trips", get(list))
        .route("/chat/trips/pdf", post(pdf))
        .route("/chat/trips/keep", post(keep))
        .route("/chat/trips/choice", post(choose))
        .route("/chat/trips/segment", post(add_leg).delete(remove_leg))
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

#[derive(serde::Deserialize)]
struct PdfIn {
    trip: String,
}

async fn pdf(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<PdfIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !auth.pdf_by_account.allow(&account_id.to_string()) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let plan = match scout_core::trips::find(&auth.core, account_id, &body.trip).await {
        Ok(Some(plan)) => plan,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not read trip for PDF");
            return sorry();
        }
    };
    let filename = trip_pdf::filename(&plan.trip.name);
    match trip_pdf::render(&plan).await {
        Ok(bytes) => {
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            );
            response.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .expect("the PDF filename is ASCII and header-safe"),
            );
            response
        }
        Err(trip_pdf::Error::Busy) => StatusCode::TOO_MANY_REQUESTS.into_response(),
        Err(trip_pdf::Error::Timeout) => StatusCode::GATEWAY_TIMEOUT.into_response(),
        Err(trip_pdf::Error::InputTooLarge) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, trip = %plan.trip.name, "could not render trip PDF");
            sorry()
        }
    }
}

async fn list(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::trips::list(&auth.core, account_id).await {
        Ok(trips) => axum::Json(trips).into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not list trips");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct KeepIn {
    trip: String,
}

/// Keeps a trip without a conversation.
///
/// The chat path for this is a brief to the flight desk, which needs a
/// model call and a turn to come back. A Keep button is the traveller
/// saying exactly one thing, and asking a language model to interpret it is
/// how "Save this trip" became `record_purchase` in production.
async fn keep(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<KeepIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::keep(&auth.core, account_id, &body.trip).await {
        Ok(Some(plan)) => axum::Json(plan).into_response(),
        // Somebody else's trip and a name nobody used are the same answer,
        // as everywhere else here: the account scoping is in the store, and
        // a 403 would confirm that the trip exists.
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not keep a trip");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct ChoiceIn {
    trip: String,
    position: i64,
    candidate: i64,
}

async fn choose(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<ChoiceIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::choose(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        body.candidate,
    )
    .await
    {
        Ok(scout_core::trips::Selection::Chosen(trip)) => axum::Json(trip).into_response(),
        Ok(scout_core::trips::Selection::TripNotFound)
        | Ok(scout_core::trips::Selection::CandidateNotFound) => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not choose a trip option");
            sorry()
        }
    }
}

/// Turns a leg edit into a response. Shared so add and remove cannot drift
/// into disagreeing about what a stale tab is told.
fn leg_response(out: scout_core::trips::LegEdit) -> Response {
    use scout_core::trips::LegEdit;
    match out {
        LegEdit::Done(plan) => axum::Json(plan).into_response(),
        LegEdit::TripNotFound => StatusCode::NOT_FOUND.into_response(),
        // Not an error: the reader's copy is simply older than the trip.
        LegEdit::SegmentChanged => StatusCode::CONFLICT.into_response(),
        LegEdit::Invalid(message) => (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct AddLegIn {
    trip: String,
    position: Option<i64>,
    origin: String,
    destination: String,
    departure_date: String,
}

async fn add_leg(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<AddLegIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::add_leg(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        &body.origin,
        &body.destination,
        &body.departure_date,
    )
    .await
    {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not add a trip leg");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct RemoveLegIn {
    trip: String,
    position: i64,
    origin: String,
    destination: String,
    departure_date: Option<String>,
}

async fn remove_leg(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<RemoveLegIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::remove_leg(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        &body.origin,
        &body.destination,
        body.departure_date.as_deref(),
    )
    .await
    {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not remove a trip leg");
            sorry()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const DAY: i64 = 86_400;

    async fn setup() -> (
        axum::Router,
        std::sync::Arc<scout_core::core::Core>,
        tempfile::TempDir,
        i64,
        String,
        String,
    ) {
        let (app, core, dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "777")
                .await
                .unwrap()
        else {
            panic!("the open round should admit the account");
        };
        scout_core::trips::seed_trip_for_tests(&core, account_id, "October")
            .await
            .unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        (app, core, dir, account_id, cookie, csrf)
    }

    async fn post_json(
        app: &axum::Router,
        uri: &str,
        cookie: &str,
        csrf: Option<&str>,
        body: &str,
    ) -> Response {
        method_json(app, "POST", uri, cookie, csrf, body).await
    }

    // DELETE carries a body here: `remove_leg`'s guard is checked against the
    // route and date the caller expects, not decoration, so a bodyless
    // DELETE has nothing to send it.
    async fn delete_json(
        app: &axum::Router,
        uri: &str,
        cookie: &str,
        csrf: Option<&str>,
        body: &str,
    ) -> Response {
        method_json(app, "DELETE", uri, cookie, csrf, body).await
    }

    async fn method_json(
        app: &axum::Router,
        method: &str,
        uri: &str,
        cookie: &str,
        csrf: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("origin", "https://example.com")
            .header("content-type", "application/json")
            .header("cookie", format!("{}={cookie}", crate::session::COOKIE));
        if let Some(csrf) = csrf {
            request = request.header("x-scout-csrf", csrf);
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_trip_list_contains_the_durable_itinerary_and_readiness() {
        let (app, _core, _dir, _account, cookie, _csrf) = setup().await;
        let res = get_with_cookie(&app, "/chat/trips", &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();

        assert_eq!(body[0]["name"], "October");
        assert_eq!(body[0]["segments"][0]["origin"], "AMS");
        assert_eq!(
            body[0]["segments"][0]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(body[0]["not_ready"].as_str().unwrap().contains("2 options"));
    }

    #[tokio::test]
    async fn a_trip_pdf_is_a_named_private_download() {
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        let refused = post_json(
            &app,
            "/chat/trips/pdf",
            &cookie,
            None,
            r#"{"trip":"October"}"#,
        )
        .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        if !crate::trip_pdf::available() {
            return;
        }
        let res = post_json(
            &app,
            "/chat/trips/pdf",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/pdf");
        assert_eq!(
            res.headers()["content-disposition"],
            "attachment; filename=\"october-itinerary.pdf\""
        );
        assert_eq!(res.headers()["cache-control"], "no-store");
        let bytes = axum::body::to_bytes(res.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
    }

    #[tokio::test]
    async fn one_account_cannot_export_another_accounts_trip() {
        let (app, core, _dir, _owner, _cookie, _csrf) = setup().await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "888")
                .await
                .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json(
            &app,
            "/chat/trips/pdf",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn keeping_a_trip_needs_a_session_and_a_csrf_token() {
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        let uri = "/chat/trips/keep";
        let body = r#"{"trip":"October"}"#;

        // Nothing that is a session at all: a Keep button is behind sign-in
        // like every other write on this router.
        let anonymous = post_json(&app, uri, "not-a-session", Some(&csrf), body).await;
        assert_eq!(anonymous.status(), StatusCode::SEE_OTHER);

        // And a stolen cookie without the header gets the same refusal it
        // gets on `choose`.
        let refused = post_json(&app, uri, &cookie, None, body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn keeping_a_draft_makes_it_kept_and_the_response_carries_the_trip() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let before = scout_core::trips::find(&core, account_id, "October")
            .await
            .unwrap()
            .unwrap();
        assert!(
            !before.trip.kept,
            "the fixture is the draft a flight search leaves behind"
        );

        let res = post_json(
            &app,
            "/chat/trips/keep",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["name"], "October");
        assert_eq!(
            response["kept"], true,
            "the client can repaint from this body without a second fetch",
        );

        // Asserted against the store, not just the response: the response
        // is what the route claims happened, the store is what actually did.
        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert!(trip.trip.kept);
    }

    #[tokio::test]
    async fn one_account_cannot_keep_another_accounts_trip() {
        let (app, core, _dir, owner, _cookie, _csrf) = setup().await;
        let scout_core::identity::SignIn::In {
            account_id: stranger,
        } = scout_core::identity::sign_in(&core, "telegram", "888")
            .await
            .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);

        let res = post_json(
            &app,
            "/chat/trips/keep",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let theirs = scout_core::trips::find(&core, owner, "October")
            .await
            .unwrap()
            .unwrap();
        assert!(
            !theirs.trip.kept,
            "keeping the owner's trip is still the owner's to do",
        );
    }

    #[tokio::test]
    async fn keeping_a_trip_twice_is_fine() {
        // A traveller pressing Keep again — on a tab that had not
        // repainted, or because nothing obvious happened the first time —
        // is repeating themselves, not making a mistake.
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        for _ in 0..2 {
            let res = post_json(
                &app,
                "/chat/trips/keep",
                &cookie,
                Some(&csrf),
                r#"{"trip":"October"}"#,
            )
            .await;
            assert_eq!(res.status(), StatusCode::OK);
            let plan: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
            assert_eq!(plan["kept"], true);
        }
    }

    #[tokio::test]
    async fn choosing_a_candidate_updates_the_trip_and_requires_csrf() {
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        let uri = "/chat/trips/choice";

        let refused = post_json(
            &app,
            uri,
            &cookie,
            None,
            r#"{"trip":"October","position":1,"candidate":2}"#,
        )
        .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        let res = post_json(
            &app,
            uri,
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"candidate":2}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(body["segments"][0]["candidates"][1]["chosen"], true);
        assert!(body["not_ready"].is_null());
    }

    #[tokio::test]
    async fn one_account_cannot_select_another_accounts_trip() {
        let (app, core, _dir, _owner, _cookie, _csrf) = setup().await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "888")
                .await
                .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json(
            &app,
            "/chat/trips/choice",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"candidate":1}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_option_removed_after_the_page_loaded_is_not_found() {
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        let res = post_json(
            &app,
            "/chat/trips/choice",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"candidate":99}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn one_account_cannot_read_another_accounts_trips() {
        let (app, core, _dir, _owner, _cookie, _csrf) = setup().await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "888")
                .await
                .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let res = get_with_cookie(&app, "/chat/trips", &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn adding_a_leg_requires_csrf_and_the_leg_actually_lands_in_the_store() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let uri = "/chat/trips/segment";
        let body = r#"{"trip":"October","position":null,"origin":"LIS","destination":"FCO","departure_date":"2026-10-20"}"#;

        // Same gate, same status as `choose`: a stolen cookie without the
        // CSRF header must not be able to edit an itinerary either.
        let refused = post_json(&app, uri, &cookie, None, body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        let res = post_json(&app, uri, &cookie, Some(&csrf), body).await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["segments"][1]["origin"], "LIS");
        assert_eq!(response["segments"][1]["destination"], "FCO");

        // Asserted against the store, not just the response: the response
        // is what the route claims happened, the store is what actually did.
        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert_eq!(trip.trip.segments.len(), 2);
        assert_eq!(trip.trip.segments[1].origin, "LIS");
    }

    #[tokio::test]
    async fn a_bad_airport_code_from_the_browser_is_a_message_not_a_five_hundred() {
        let (app, _core, _dir, _account, cookie, csrf) = setup().await;
        let res = post_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":null,"origin":"Amsterdam","destination":"FCO","departure_date":"2026-10-20"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = body_of(res).await;
        assert!(
            text.contains("3-letter IATA"),
            "the reader is told what to fix, got: {text}",
        );
    }

    #[tokio::test]
    async fn a_stale_remove_is_a_conflict_and_changes_nothing() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        // The trip's only leg is AMS -> LIS. Naming a different destination
        // for the same position is exactly what a tab that hasn't reloaded
        // since somebody else edited the trip would send.
        //
        // This body is exactly what `removeLegBody('October', { position: 1,
        // origin: 'AMS', destination: 'FCO', departure_date: null })` in
        // chat.js produces — same keys, same explicit `null` — so a change
        // to either side that broke the other would show up here rather
        // than only in production.
        let res = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"origin":"AMS","destination":"FCO","departure_date":null}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::CONFLICT);
        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert_eq!(
            trip.trip.segments.len(),
            1,
            "the guard refused before touching anything"
        );
        assert_eq!(trip.trip.segments[0].destination, "LIS");
    }

    #[tokio::test]
    async fn a_stale_insert_is_also_a_conflict() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        // The trip has one leg, so the only positions it has are 1 (in
        // front) and 2 (append). Position 5 is a tab that drew a much
        // longer trip than this one currently is.
        let res = post_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":5,"origin":"LIS","destination":"FCO","departure_date":"2026-10-20"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::CONFLICT);
        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert_eq!(
            trip.trip.segments.len(),
            1,
            "the stale insert added nothing"
        );
    }

    #[tokio::test]
    async fn one_account_cannot_add_a_leg_to_another_accounts_trip() {
        let (app, core, _dir, owner, _cookie, _csrf) = setup().await;
        // Owner also has a trip called "Atlantic loop" so the name alone
        // cannot be what lets the stranger through: the lookup has to be
        // scoped by account, not just by name.
        scout_core::trips::seed_trip_for_tests(&core, owner, "Atlantic loop")
            .await
            .unwrap();
        let scout_core::identity::SignIn::In {
            account_id: stranger,
        } = scout_core::identity::sign_in(&core, "telegram", "888")
            .await
            .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);

        let res = post_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"Atlantic loop","position":null,"origin":"LIS","destination":"FCO","departure_date":"2026-10-20"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let owners = scout_core::trips::list(&core, owner).await.unwrap();
        let theirs = owners
            .iter()
            .find(|p| p.trip.name == "Atlantic loop")
            .unwrap();
        assert_eq!(
            theirs.trip.segments.len(),
            1,
            "the stranger's request never reached the owner's trip"
        );
    }

    #[tokio::test]
    async fn one_account_cannot_remove_a_leg_from_another_accounts_trip() {
        let (app, core, _dir, owner, _cookie, _csrf) = setup().await;
        scout_core::trips::seed_trip_for_tests(&core, owner, "Atlantic loop")
            .await
            .unwrap();
        let scout_core::identity::SignIn::In {
            account_id: stranger,
        } = scout_core::identity::sign_in(&core, "telegram", "888")
            .await
            .unwrap()
        else {
            panic!("the round should admit the second account");
        };
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);

        let res = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"Atlantic loop","position":1,"origin":"AMS","destination":"LIS","departure_date":"2026-10-12"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let owners = scout_core::trips::list(&core, owner).await.unwrap();
        let theirs = owners
            .iter()
            .find(|p| p.trip.name == "Atlantic loop")
            .unwrap();
        assert_eq!(
            theirs.trip.segments.len(),
            1,
            "the stranger's request never reached the owner's trip"
        );
    }

    #[tokio::test]
    async fn a_removed_leg_is_gone_and_the_response_carries_the_updated_trip_for_a_repaint() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let res = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"origin":"AMS","destination":"LIS","departure_date":"2026-10-12"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(
            response["segments"].as_array().unwrap().is_empty(),
            "the client can repaint from this body without a second fetch",
        );

        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert!(trip.trip.segments.is_empty());
    }
}
