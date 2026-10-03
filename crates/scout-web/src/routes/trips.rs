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
        // DELETE on the collection, addressed by the name in the body,
        // because that is how every trip on this router is addressed —
        // there are no per-trip URLs, and a traveller's name for a trip is
        // not a path segment. `delete_trip` says what stops that from being
        // the same request as removing a leg.
        .route("/chat/trips", get(list).delete(delete_trip))
        .route("/chat/trips/pdf", post(pdf))
        .route("/chat/trips/keep", post(keep))
        .route("/chat/trips/choice", post(choose))
        .route("/chat/trips/segment", post(add_leg).delete(remove_leg))
        .route("/chat/trips/item-note", post(note_item))
        .route("/chat/trips/item-held", post(hold_item))
        .route("/chat/trips/item-move", post(move_item))
        .route("/chat/trips/item-warning", post(dismiss_warning))
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
    pdf_response(&auth, account_id, &body.trip).await
}

/// A trip's PDF for an account already proven, by cookie or by file link.
/// The throttle is in here rather than in either caller, so a link cannot
/// be the way round it.
pub(crate) async fn pdf_response(auth: &AuthState, account_id: i64, trip: &str) -> Response {
    if !auth.pdf_by_account.allow(&account_id.to_string()) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let plan = match scout_core::trips::find(&auth.core, account_id, trip).await {
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

/// `deny_unknown_fields` is a guard, not tidiness. `/chat/trips` is one
/// path segment short of `/chat/trips/segment`, and both take a DELETE with
/// a JSON body naming a trip — so a client that lost the suffix would send
/// a leg removal here, and serde would happily read the `trip` out of it
/// and destroy the whole itinerary instead of one flight. A body carrying
/// `position` is refused rather than obeyed.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteTripIn {
    trip: String,
}

/// Deletes a trip, its legs, and every flight option parked on them.
///
/// Answers with the account's remaining trips rather than `204`. The client
/// has to repaint a list it can no longer derive — the trip it was showing
/// may be the one that went — and every other write on this router already
/// answers with what to draw, so a second GET here would be the one write
/// whose result could be raced by a load in flight.
async fn delete_trip(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<DeleteTripIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::delete(&auth.core, account_id, &body.trip).await {
        Ok(Some(remaining)) => axum::Json(remaining).into_response(),
        // Somebody else's trip and a name nobody used are the same answer,
        // for the reason `keep` gives: a 403 would confirm the trip exists.
        // A second press on a tab that has not repainted lands here too,
        // and "already gone" is the state that press wanted.
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not delete a trip");
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

/// Positions follow dates on every write, so a leg lands where its
/// `departure_date` puts it. An old client's `position` is an unknown field
/// and ignored — not refused, which is why this is not `deny_unknown_fields`.
#[derive(serde::Deserialize)]
struct AddLegIn {
    trip: String,
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

/// What the client drew on the item it wants gone. A flight card sends its
/// route, a stay card its title; every field given must still match, so a
/// tab that drew the trip before somebody else edited it cannot take the
/// wrong item. `departure_date` is what flight cards have always sent and
/// `date` is the field every item has; both name the same check.
#[derive(serde::Deserialize)]
struct RemoveItemIn {
    trip: String,
    position: i64,
    origin: Option<String>,
    destination: Option<String>,
    title: Option<String>,
    #[serde(alias = "departure_date")]
    date: Option<String>,
}

/// What the page sends to write a note. The item is named the way
/// `RemoveItemIn` names one — a position plus what the card showed — for
/// the same reason: positions are recomputed on every write, so a tab
/// holding an older copy of the trip could otherwise write a link onto
/// whatever has since taken that number.
///
/// `note` absent, `null`, or blank all clear the field. There is no
/// separate "clear" request because there is no difference to draw: the
/// page's box is empty in both cases, and a second endpoint would be a
/// second thing to keep in step with `note_text`.
#[derive(serde::Deserialize)]
struct NoteItemIn {
    trip: String,
    position: i64,
    origin: Option<String>,
    destination: Option<String>,
    title: Option<String>,
    #[serde(alias = "departure_date")]
    date: Option<String>,
    note: Option<String>,
}

/// What the page sends to mark an item held, or let it go. The item is
/// named the way `NoteItemIn` names one, and guarded the same way.
#[derive(serde::Deserialize)]
struct HoldItemIn {
    trip: String,
    position: i64,
    origin: Option<String>,
    destination: Option<String>,
    title: Option<String>,
    #[serde(alias = "departure_date")]
    date: Option<String>,
    held: bool,
}

async fn hold_item(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<HoldItemIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match scout_core::trips::hold_item(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        scout_core::trips::ItemExpectation {
            origin: body.origin,
            destination: body.destination,
            title: body.title,
            date: body.date,
        },
        body.held,
    )
    .await
    {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not mark a trip item held");
            sorry()
        }
    }
}

/// What the page sends to move an item to another day. Named and guarded
/// as `NoteItemIn` is. `confirm` is the answer to "it is held; move it
/// anyway?", and defaults to no.
#[derive(serde::Deserialize)]
struct MoveItemIn {
    trip: String,
    position: i64,
    title: Option<String>,
    date: Option<String>,
    to: String,
    #[serde(default)]
    confirm: bool,
}

/// The trip as it is now, with what happened to the move.
async fn move_answer(auth: &AuthState, account_id: i64, trip: &str, moved: bool, checked: bool) -> Response {
    match scout_core::trips::find(&auth.core, account_id, trip).await {
        Ok(Some(plan)) => axum::Json(serde_json::json!({ "trip": plan, "moved": moved, "checked": checked })).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not read a trip after a move");
            sorry()
        }
    }
}

/// Moves an item, then has Scout check the move.
///
/// The move does not wait for the check and does not depend on it: a plan
/// moves at once, and a held item moves once the request confirms it. The
/// check is a request like any other — logged, counted toward the day —
/// and when the account has none left the item moves unchecked and the
/// answer says so.
async fn move_item(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<MoveItemIn>,
) -> Response {
    use scout_core::trips::MoveEdit;
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let expected = scout_core::trips::ItemExpectation {
        origin: None,
        destination: None,
        title: body.title,
        date: body.date,
    };
    let moved = match scout_core::trips::move_item(&auth.core, account_id, &body.trip, body.position, expected, &body.to, body.confirm).await {
        Ok(MoveEdit::Moved(moved)) => *moved,
        Ok(MoveEdit::Same) => return move_answer(&auth, account_id, &body.trip, false, false).await,
        Ok(MoveEdit::NeedsConfirm) => return axum::Json(serde_json::json!({ "needs_confirm": true })).into_response(),
        Ok(MoveEdit::TripNotFound) => return StatusCode::NOT_FOUND.into_response(),
        Ok(MoveEdit::SegmentChanged) => return StatusCode::CONFLICT.into_response(),
        Ok(MoveEdit::Invalid(message)) => return (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not move a trip item");
            return sorry();
        }
    };
    let checked = scout_core::session::over_daily_cap(&auth.core, account_id).await.is_none()
        && auth.by_account.allow(&format!("check:{account_id}"));
    if checked {
        if let Err(e) = auth.core.log_request(account_id, "move_check").await {
            tracing::warn!(error = %e, account_id, "request logging failed");
        }
        // Before the answer is read, so the trip it carries already says
        // the item is being checked.
        if let Err(e) = scout_core::move_check::begin(&auth.core, moved.item_id).await {
            tracing::warn!(error = %e, account_id, "could not mark an item as being checked");
        }
        tokio::spawn(run_check(auth.clone(), moved.clone()));
    }
    move_answer(&auth, account_id, &moved.trip, true, checked).await
}

/// The thread a check runs in: the trip's own when the web may post into
/// it, else a new one — `composerTarget`'s rule on the page, for the same
/// reason. A Telegram group's thread is other people's room.
async fn check_thread(auth: &AuthState, account_id: i64, trip: &str) -> anyhow::Result<i64> {
    let plan = scout_core::trips::find(&auth.core, account_id, trip)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the trip is gone"))?;
    match plan.chat.as_ref().filter(|chat| chat.scope == "direct") {
        Some(chat) => Ok(chat.id),
        None => scout_core::session::reset(&auth.core, account_id, "direct").await,
    }
}

/// The check, as a background task: a run of the main agent in the trip's
/// thread, mirrored to the phone when the thread is.
async fn run_check(auth: AuthState, moved: scout_core::move_check::Move) {
    let account_id = moved.account_id;
    let thread = match check_thread(&auth, account_id, &moved.trip).await {
        Ok(thread) => thread,
        Err(e) => {
            tracing::warn!(error = %e, account_id, "no thread to check a move in");
            scout_core::move_check::abandon(&auth.core, &moved).await;
            return;
        }
    };
    let reply_to = crate::routes::chat::reply_to_for(&auth, account_id).await;
    let verdict = scout_core::move_check::check(&auth.core, &moved, |prompt: String| {
        let (auth, reply_to) = (auth.clone(), reply_to.clone());
        async move {
            let run = scout_api::RunContext {
                account_id,
                conversation_id: thread,
                reply_to,
                // The line the thread shows, which is also what names a
                // thread started for this.
                title_source: prompt.lines().next().map(str::to_string),
            };
            // Nobody is watching this run draw; the events are read so the
            // channel has a reader and dropped.
            let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
            let drain = tokio::spawn(async move { while seen.recv().await.is_some() {} });
            let outcome = scout_core::run::run_agent(&auth.core, events, &run, &prompt).await;
            let _ = drain.await;
            if matches!(&outcome, Ok(scout_core::run::RunOutcome::Answered(_))) {
                crate::routes::chat::queue_conversation(&auth, account_id, thread).await;
            }
            outcome
        }
    })
    .await;
    tracing::info!(account_id, item_id = moved.item_id, verdict = ?verdict, "a move was checked");
}

/// What the page sends to dismiss an item's warning.
#[derive(serde::Deserialize)]
struct WarningIn {
    trip: String,
    position: i64,
    title: Option<String>,
    date: Option<String>,
}

async fn dismiss_warning(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<WarningIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let expected = scout_core::trips::ItemExpectation {
        origin: None,
        destination: None,
        title: body.title,
        date: body.date,
    };
    match scout_core::trips::dismiss_warning(&auth.core, account_id, &body.trip, body.position, expected).await {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not dismiss a warning");
            sorry()
        }
    }
}

async fn note_item(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<NoteItemIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::note_item(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        scout_core::trips::ItemExpectation {
            origin: body.origin,
            destination: body.destination,
            title: body.title,
            date: body.date,
        },
        body.note,
    )
    .await
    {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not note a trip item");
            sorry()
        }
    }
}

async fn remove_leg(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<RemoveItemIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match scout_core::trips::remove_item(
        &auth.core,
        account_id,
        &body.trip,
        body.position,
        scout_core::trips::ItemExpectation {
            origin: body.origin,
            destination: body.destination,
            title: body.title,
            date: body.date,
        },
    )
    .await
    {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not remove a trip item");
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
        assert_eq!(body[0]["items"][0]["origin"], "AMS");
        assert_eq!(
            body[0]["items"][0]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(body[0]["readiness"]["state"], "not_ready");
        assert!(body[0]["readiness"]["reason"].as_str().unwrap().contains("2 options"));
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
    async fn deleting_a_trip_needs_a_session_and_a_csrf_token() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let uri = "/chat/trips";
        let body = r#"{"trip":"October"}"#;

        // The most destructive write on this router is behind the same two
        // gates as the least.
        let anonymous = delete_json(&app, uri, "not-a-session", Some(&csrf), body).await;
        assert_eq!(anonymous.status(), StatusCode::SEE_OTHER);

        let refused = delete_json(&app, uri, &cookie, None, body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        // And the trip survived both refusals.
        let trip = scout_core::trips::find(&core, account_id, "October").await.unwrap();
        assert!(trip.is_some());
    }

    #[tokio::test]
    async fn a_deleted_trip_is_gone_and_the_response_carries_the_trips_that_are_left() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        scout_core::trips::seed_trip_for_tests(&core, account_id, "Atlantic loop")
            .await
            .unwrap();

        let res = delete_json(
            &app,
            "/chat/trips",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        let names: Vec<&str> = response
            .as_array()
            .unwrap()
            .iter()
            .map(|plan| plan["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Atlantic loop"],
            "the client repaints the list from this body, selection and all",
        );

        // Asserted against the store, not just the response: the response
        // is what the route claims happened, the store is what actually did.
        let left = scout_core::trips::list(&core, account_id).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].trip.name, "Atlantic loop");
    }

    #[tokio::test]
    async fn deleting_a_trip_this_account_does_not_have_is_a_not_found() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let res = delete_json(
            &app,
            "/chat/trips",
            &cookie,
            Some(&csrf),
            r#"{"trip":"a trip nobody made"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            scout_core::trips::list(&core, account_id).await.unwrap().len(),
            1,
            "a miss deletes nothing"
        );
    }

    #[tokio::test]
    async fn one_account_cannot_delete_another_accounts_trip_of_the_same_name() {
        let (app, core, _dir, owner, _cookie, _csrf) = setup().await;
        // The owner has an "Atlantic loop" too, so the name alone cannot be
        // what the delete finds: only the account scoping decides whose trip
        // this destroys, and here it destroys every leg and option on it.
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
        scout_core::trips::seed_trip_for_tests(&core, stranger, "Atlantic loop")
            .await
            .unwrap();
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);

        let res = delete_json(
            &app,
            "/chat/trips",
            &cookie,
            Some(&csrf),
            r#"{"trip":"Atlantic loop"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(
            response.as_array().unwrap().is_empty(),
            "the stranger deleted their own trip and has none left",
        );

        let theirs = scout_core::trips::find(&core, owner, "Atlantic loop")
            .await
            .unwrap()
            .expect("the owner's trip of the same name is untouched");
        assert_eq!(theirs.trip.items.len(), 1);
        assert_eq!(
            theirs.trip.items[0].candidates.len(),
            2,
            "and so are the options parked on it",
        );
        assert_eq!(
            scout_core::trips::list(&core, owner).await.unwrap().len(),
            2,
            "the owner still has both trips",
        );
    }

    #[tokio::test]
    async fn a_leg_removal_sent_to_the_trip_delete_route_is_refused_rather_than_obeyed() {
        // `/chat/trips` is one path segment short of `/chat/trips/segment`
        // and takes the same method. Without `deny_unknown_fields` on
        // `DeleteTripIn`, a client that lost the suffix would read as "delete
        // the whole trip" — the exact body `removeLegBody` sends, silently
        // destroying an itinerary instead of one leg.
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        let res = delete_json(
            &app,
            "/chat/trips",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"origin":"AMS","destination":"LIS","departure_date":"2026-10-12"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let trip = scout_core::trips::find(&core, account_id, "October")
            .await
            .unwrap()
            .expect("the trip is still there");
        assert_eq!(trip.trip.items.len(), 1, "and so is the leg it was about");
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
        assert_eq!(body["items"][0]["candidates"][1]["chosen"], true);
        assert_eq!(
            body["readiness"],
            serde_json::json!({"state": "ready", "legs": ["segment 1 (AMS→LIS)"]}),
            "the one leg on this trip is the one the page says it would price",
        );
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
        assert_eq!(response["items"][1]["origin"], "LIS");
        assert_eq!(response["items"][1]["destination"], "FCO");

        // Asserted against the store, not just the response: the response
        // is what the route claims happened, the store is what actually did.
        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert_eq!(trip.trip.items.len(), 2);
        assert_eq!(trip.trip.items[1].origin.as_deref(), Some("LIS"));
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
            trip.trip.items.len(),
            1,
            "the guard refused before touching anything"
        );
        assert_eq!(trip.trip.items[0].destination.as_deref(), Some("LIS"));
    }

    #[test]
    fn the_box_the_page_offers_holds_what_the_store_accepts() {
        // Two copies of one number, and the cheap failure is the box that
        // takes more than the store will: the traveller types a note, is
        // told nothing, and loses it on save. `note_text` is the rule; this
        // is what keeps the page's `maxLength` standing next to it.
        let js = include_str!("../chat.js");
        let line = js
            .lines()
            .find(|line| line.contains("export const NOTE_MAX_CHARS"))
            .expect("the page must say how long a note may be");
        assert!(
            line.contains(&scout_core::trips::MAX_NOTE_CHARS.to_string()),
            "the page offers a different length than the store accepts: {line}"
        );
    }

    #[tokio::test]
    async fn an_item_is_marked_held_from_the_page_and_a_stale_card_is_a_conflict() {
        // The gap the owner hit in chat, on the page: a lunch arranged
        // over WhatsApp is held, and nothing could say so.
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        scout_core::trips::seed_item_for_tests(&core, account_id, "October", "activity", "Lunch with Stanley", "2026-10-12")
            .await
            .unwrap();

        let stale = post_json(
            &app,
            "/chat/trips/item-held",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Dinner with Stanley","date":"2026-10-12","held":true}"#,
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);

        let res = post_json(
            &app,
            "/chat/trips/item-held",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","held":true}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["items"][1]["booked"], true);

        // And off again, because plans fall through.
        let res = post_json(
            &app,
            "/chat/trips/item-held",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","held":false}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["items"][1]["booked"], false);

        let bare = post_json(
            &app,
            "/chat/trips/item-held",
            &cookie,
            None,
            r#"{"trip":"October","position":2,"title":"Lunch with Stanley","held":true}"#,
        )
        .await;
        assert_eq!(bare.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_note_is_written_from_the_page_and_a_stale_card_is_a_conflict() {
        // The page's half of the request behind this feature: a link for an
        // item already on the trip. The body is a stay card's — a title and
        // a date — and it is checked the way a removal's is, because a note
        // on the wrong item is a mistake the page never announces.
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        scout_core::trips::seed_item_for_tests(
            &core,
            account_id,
            "October",
            "activity",
            "Lunch with Stanley",
            "2026-10-12",
        )
        .await
        .unwrap();
        let link = "https://www.google.com/maps/search/?api=1&query=Queen%27s+Cafe";

        let stale = post_json(
            &app,
            "/chat/trips/item-note",
            &cookie,
            Some(&csrf),
            &format!(r#"{{"trip":"October","position":2,"title":"Dinner with Stanley","date":"2026-10-12","note":"{link}"}}"#),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);

        let res = post_json(
            &app,
            "/chat/trips/item-note",
            &cookie,
            Some(&csrf),
            &format!(r#"{{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","note":"{link}"}}"#),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["items"][1]["notes"], link);

        // No CSRF header is the same refusal every write on this router
        // gives, and it must reach the store no more than the stale one did.
        let bare = post_json(
            &app,
            "/chat/trips/item-note",
            &cookie,
            None,
            r#"{"trip":"October","position":2,"title":"Lunch with Stanley","note":"elsewhere"}"#,
        )
        .await;
        assert_eq!(bare.status(), StatusCode::BAD_REQUEST);

        // Sending nothing clears it, which is how the page's empty box is
        // meant to read.
        let res = post_json(
            &app,
            "/chat/trips/item-note",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(response["items"][1]["notes"].is_null(), "{response}");
    }

    #[tokio::test]
    async fn a_position_on_an_added_leg_is_ignored_because_its_date_decides() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        // A tab built before positions followed dates still sends one.
        // Position 5 on a one-leg trip used to be a conflict; now it is
        // simply not what decides where the leg goes — the date is, and
        // 2026-10-10 is before the seeded 2026-10-12 flight.
        let res = post_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":5,"origin":"BCN","destination":"MAD","departure_date":"2026-10-10"}"#,
        )
        .await;

        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["items"][0]["origin"], "BCN");
        assert_eq!(response["items"][0]["position"], 1);

        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert_eq!(
            trip.trip
                .items
                .iter()
                .map(|i| i.origin.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["BCN", "AMS"],
            "the leg landed where its date puts it, not at position 5",
        );
    }

    #[tokio::test]
    async fn a_stay_is_removed_by_title_and_date_and_the_flight_body_still_works() {
        let (app, core, _dir, account_id, cookie, csrf) = setup().await;
        // Same day as the seeded flight; a stay sorts behind a flight on
        // its day, so it is position 2.
        scout_core::trips::seed_item_for_tests(
            &core,
            account_id,
            "October",
            "stay",
            "Hotel Lisboa",
            "2026-10-12",
        )
        .await
        .unwrap();

        // This body is what a stay card sends: no route, a title and a
        // date. The wrong title is a stale tab and changes nothing.
        let stale = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Hostel Lisboa","date":"2026-10-12"}"#,
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);

        let res = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":2,"title":"Hotel Lisboa","date":"2026-10-12"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(response["items"].as_array().unwrap().len(), 1);
        assert_eq!(response["items"][0]["kind"], "flight");

        // The flight body a flight card has always sent is still accepted
        // on the same route, so a client need not know two endpoints.
        let res = delete_json(
            &app,
            "/chat/trips/segment",
            &cookie,
            Some(&csrf),
            r#"{"trip":"October","position":1,"origin":"AMS","destination":"LIS","departure_date":"2026-10-12"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);

        let trip = scout_core::trips::find(&core, account_id, "October")
            .await
            .unwrap()
            .unwrap();
        assert!(trip.trip.items.is_empty(), "both removals reached the store");
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
            theirs.trip.items.len(),
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
            theirs.trip.items.len(),
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
            response["items"].as_array().unwrap().is_empty(),
            "the client can repaint from this body without a second fetch",
        );

        let trip = scout_core::trips::list(&core, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.trip.name == "October")
            .unwrap();
        assert!(trip.trip.items.is_empty());
    }
    async fn with_lunch() -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir, i64, String, String) {
        let (app, core, dir, account_id, cookie, csrf) = setup().await;
        scout_core::trips::seed_item_for_tests(&core, account_id, "October", "activity", "Lunch with Stanley", "2026-10-12")
            .await
            .unwrap();
        (app, core, dir, account_id, cookie, csrf)
    }

    async fn json_of(res: Response) -> serde_json::Value {
        serde_json::from_str(&body_of(res).await).unwrap()
    }

    #[tokio::test]
    async fn a_plan_moves_at_once_and_is_marked_as_being_checked() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let body = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await;
        assert_eq!(res.status(), StatusCode::OK);
        let answer = json_of(res).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(true), Some(true)));
        let lunch = answer["trip"]["items"].as_array().unwrap().iter().find(|i| i["title"] == "Lunch with Stanley").unwrap();
        assert_eq!(lunch["date"], "2026-10-14");
        assert_eq!(lunch["checking"], true, "the card says so from this response, not five seconds later");

        // The same request again names a day the item is no longer on.
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_held_item_is_not_moved_until_the_request_says_so() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let held = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","held":true}"#;
        assert_eq!(post_json(&app, "/chat/trips/item-held", &cookie, Some(&csrf), held).await.status(), StatusCode::OK);

        let ask = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), ask).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await, serde_json::json!({ "needs_confirm": true }));
        let trips = json_of(get_with_cookie(&app, "/chat/trips", &cookie).await).await;
        assert_eq!(trips[0]["items"][1]["date"], "2026-10-12", "asking is not moving");

        let sure = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14","confirm":true}"#;
        let answer = json_of(post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), sure).await).await;
        assert_eq!(answer["moved"], true);
    }

    #[tokio::test]
    async fn what_cannot_be_moved_is_refused_and_a_move_to_the_same_day_is_nothing() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let to = "/chat/trips/item-move";
        let flight = r#"{"trip":"October","position":1,"title":"AMS → LIS","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), flight).await.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bad_date = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"tomorrow"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), bad_date).await.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let same = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-12"}"#;
        let answer = json_of(post_json(&app, to, &cookie, Some(&csrf), same).await).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(false), Some(false)));
        let lost = r#"{"trip":"Nowhere","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), lost).await.status(), StatusCode::NOT_FOUND);
        let good = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, None, good).await.status(), StatusCode::BAD_REQUEST, "no form token, no move");
    }

    #[tokio::test]
    async fn over_the_daily_cap_the_item_moves_and_nothing_is_checked() {
        let (app, core, _dir, account_id, cookie, csrf) = with_lunch().await;
        for _ in 0..20 {
            core.log_request(account_id, "text").await.unwrap();
        }
        let body = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let answer = json_of(post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(true), Some(false)));
        let lunch = answer["trip"]["items"].as_array().unwrap().iter().find(|i| i["title"] == "Lunch with Stanley").unwrap();
        assert_eq!(lunch["checking"], false);
    }

    #[tokio::test]
    async fn a_warning_is_dismissed_from_the_page_and_a_stale_card_is_a_conflict() {
        let (app, core, _dir, account_id, cookie, csrf) = with_lunch().await;
        scout_core::trips::seed_warning_for_tests(&core, account_id, "October", "Lunch with Stanley", "closed that day").await.unwrap();
        let trips = json_of(get_with_cookie(&app, "/chat/trips", &cookie).await).await;
        assert_eq!(trips[0]["items"][1]["warning"], "closed that day");

        let stale = r#"{"trip":"October","position":2,"title":"Dinner","date":"2026-10-12"}"#;
        assert_eq!(post_json(&app, "/chat/trips/item-warning", &cookie, Some(&csrf), stale).await.status(), StatusCode::CONFLICT);
        let mine = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12"}"#;
        let plan = json_of(post_json(&app, "/chat/trips/item-warning", &cookie, Some(&csrf), mine).await).await;
        assert_eq!(plan["items"][1]["warning"], serde_json::Value::Null);
    }
}
