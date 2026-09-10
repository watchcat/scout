//! The visual trip planner's small authenticated web surface.
//!
//! It reads the itinerary Scout already stores and lets the traveller settle
//! an existing candidate. Searching, adding routes and pricing still happen in
//! chat, where the flight agent can validate live provider data.

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
        .route("/chat/trips/choice", post(choose))
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
        let mut request = Request::builder()
            .method("POST")
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
}
