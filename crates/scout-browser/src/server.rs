//! Sessions over HTTP: open one on a page, act, close.
//!
//! ```text
//! POST   /sessions            {"url": "https://www.shop.example/p/widget"}
//! POST   /sessions/{id}/act   {"do": "click", "n": 3}
//! DELETE /sessions/{id}
//! ```
//!
//! Opening and acting answer with the page as an outline and what is left
//! of the session. A rule that stopped an action is not an error of the
//! call: the answer is the page as it still is, with what was refused and
//! why. The statuses are for calls that have no page to answer with.

use crate::drive::{Action, Browser};
use crate::session::{Failure, Sessions, Step};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;

/// The routes, over `sessions`.
pub fn router<B: Browser>(sessions: Arc<Sessions<B>>) -> Router {
    Router::new()
        .route("/sessions", post(open::<B>))
        .route("/sessions/{id}/act", post(act::<B>))
        .route("/sessions/{id}", delete(close::<B>))
        .with_state(sessions)
}

/// What a session is opened with.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    url: String,
}

/// A new session's name, and its first page.
#[derive(serde::Serialize)]
struct Opened {
    session: String,
    #[serde(flatten)]
    step: Step,
}

async fn open<B: Browser>(State(sessions): State<Arc<Sessions<B>>>, Json(asked): Json<Open>) -> Response {
    match sessions.open(&asked.url).await {
        Ok((session, step)) => Json(Opened { session, step }).into_response(),
        Err(failure) => failed(failure),
    }
}

async fn act<B: Browser>(State(sessions): State<Arc<Sessions<B>>>, Path(id): Path<String>, Json(action): Json<Action>) -> Response {
    match sessions.act(&id, action).await {
        Ok(step) => Json(step).into_response(),
        Err(failure) => failed(failure),
    }
}

async fn close<B: Browser>(State(sessions): State<Arc<Sessions<B>>>, Path(id): Path<String>) -> Response {
    match sessions.close(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => failed(failure),
    }
}

/// A call with no page to answer with: the status says which kind, the
/// body why, in the sentence the session layer wrote.
fn failed(failure: Failure) -> Response {
    let status = match &failure {
        // The address is one no session opens. Asking again will not help.
        Failure::Refused(_) => StatusCode::UNPROCESSABLE_ENTITY,
        // Every place stayed taken. Later may be different.
        Failure::Full(_) => StatusCode::SERVICE_UNAVAILABLE,
        Failure::Unknown => StatusCode::NOT_FOUND,
        // There was such a session, and it is over.
        Failure::Ended(_) => StatusCode::GONE,
        // The browser behind this, or the page it went for, failed.
        Failure::Broken(_) => StatusCode::BAD_GATEWAY,
    };
    let mut body = json!({ "error": failure.to_string() });
    if let Failure::Ended(why) = &failure {
        body["ended"] = json!(why);
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use crate::testing::{refusing, Scripted, WIDGET};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tower::ServiceExt;

    /// One call, and its status with its body as JSON (null when empty).
    async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(path);
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let response = app.clone().oneshot(request.unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn open(app: &Router, url: &str) -> (StatusCode, Value) {
        call(app, "POST", "/sessions", Some(json!({ "url": url }))).await
    }

    async fn act(app: &Router, id: &str, action: Value) -> (StatusCode, Value) {
        call(app, "POST", &format!("/sessions/{id}/act"), Some(action)).await
    }

    fn scripted(limits: Limits) -> (Scripted, Router) {
        let browser = Scripted::default();
        let app = router(browser.sessions(limits));
        (browser, app)
    }

    #[tokio::test]
    async fn a_session_is_opened_acted_in_and_closed_over_http() {
        let (browser, app) = scripted(Limits::default());
        let (status, opened) = open(&app, WIDGET).await;
        assert_eq!(status, StatusCode::OK);
        let id = opened["session"].as_str().expect("a session's name").to_string();
        assert_eq!(opened["page"], json!({ "title": "Widget", "url": WIDGET, "text": "# The page\nWidget", "more": false }));
        assert_eq!((&opened["refused"], &opened["notes"], &opened["ended"]), (&json!([]), &json!([]), &Value::Null));
        assert_eq!(opened["actions_left"], 25);
        assert!(opened["seconds_left"].as_u64().is_some_and(|left| (179..=180).contains(&left)), "{opened}");

        let (status, step) = act(&app, &id, json!({ "do": "click", "n": 3 })).await;
        assert_eq!((status, &step["page"]["title"], &step["actions_left"]), (StatusCode::OK, &json!("Next"), &json!(24)));
        // An action's answer does not name the session again.
        assert_eq!(step.get("session"), None);
        let (status, _) = act(&app, &id, json!({ "do": "type", "n": 1, "text": "kattenvoer", "enter": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(*browser.asked.lock().unwrap(), [Action::Click { n: 3 }, Action::Type { n: 1, text: "kattenvoer".into(), enter: true }]);

        let (status, body) = call(&app, "DELETE", &format!("/sessions/{id}"), None).await;
        assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
        assert_eq!(browser.closed.load(Ordering::SeqCst), 1);
        // Gone is gone, for an action and for a second close.
        let (status, body) = act(&app, &id, json!({ "do": "read" })).await;
        assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "there is no such session" })));
        assert_eq!(call(&app, "DELETE", &format!("/sessions/{id}"), None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn what_a_rule_stopped_is_told_with_the_page_and_not_as_an_error() {
        let (browser, app) = scripted(Limits::default());
        let (_, opened) = open(&app, WIDGET).await;
        let id = opened["session"].as_str().unwrap();
        let mut stopped = crate::testing::shown("Widget");
        stopped.refused.push("that field is for something personal".to_string());
        stopped.notes.push("Chosen: xl".to_string());
        browser.then([Ok(stopped)]);
        let (status, step) = act(&app, id, json!({ "do": "type", "n": 6, "text": "x" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!((&step["refused"], &step["notes"]), (&json!(["that field is for something personal"]), &json!(["Chosen: xl"])));
        assert_eq!(step["page"]["title"], "Widget");
    }

    #[tokio::test]
    async fn an_address_that_may_not_be_opened_is_refused_with_the_reason() {
        let (browser, app) = scripted(Limits::default());
        for (address, why) in [
            ("https://www.shop.example/checkout", "that address is a checkout, and a session goes no further than the basket"),
            ("https://www.shop.example/account/login", "that address is for signing in, and a session never signs in"),
            ("file:///etc/passwd", "that is not a web address"),
        ] {
            let (status, body) = open(&app, address).await;
            assert_eq!((status, body), (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": why })), "{address}");
        }
        assert_eq!(browser.opened.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fourth_session_is_told_there_is_no_place_when_its_time_is_up() {
        let (_browser, app) = scripted(Limits::default());
        for _ in 0..3 {
            assert_eq!(open(&app, WIDGET).await.0, StatusCode::OK);
        }
        let started = tokio::time::Instant::now();
        let (status, body) = open(&app, WIDGET).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({ "error": "3 sessions are open, and no place came free within this one's 180 seconds" }));
        assert_eq!(started.elapsed(), Duration::from_secs(180));
    }

    #[tokio::test]
    async fn a_session_that_is_over_answers_gone_with_why() {
        let (browser, app) = scripted(Limits { sessions: 3, actions: 2, time: Duration::from_secs(180) });
        let (_, opened) = open(&app, WIDGET).await;
        let id = opened["session"].as_str().unwrap();
        assert_eq!(act(&app, id, json!({ "do": "read" })).await.1["ended"], Value::Null);
        // The last action answers with its page, and says it was the last.
        let (status, last) = act(&app, id, json!({ "do": "read" })).await;
        assert_eq!((status, &last["ended"], &last["actions_left"]), (StatusCode::OK, &json!("its 2 actions are spent"), &json!(0)));
        let (status, body) = act(&app, id, json!({ "do": "read" })).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body, json!({ "error": "the session has ended: its 2 actions are spent", "ended": "its 2 actions are spent" }));
        // Closing what is over is still allowed, and forgets it.
        assert_eq!(call(&app, "DELETE", &format!("/sessions/{id}"), None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(browser.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_session_opened_on_a_refusal_is_shown_it_and_is_over() {
        let (browser, app) = scripted(Limits::default());
        browser.opening_with(Ok(refusing()));
        let (status, opened) = open(&app, WIDGET).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(opened["ended"], "the page the session was opened on was refused");
        assert_eq!(opened["notes"], json!(["This page is a refusal (HTTP 403). Nothing is done to get past it."]));
        let id = opened["session"].as_str().unwrap();
        let (status, body) = act(&app, id, json!({ "do": "click", "n": 1 })).await;
        assert_eq!((status, &body["ended"]), (StatusCode::GONE, &json!("the page the session was opened on was refused")));
        assert_eq!(browser.asked.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_browser_that_fails_is_a_bad_gateway() {
        let (browser, app) = scripted(Limits::default());
        browser.opening_with(Err(crate::drive::Broken::Failed("Chrome opened no page".to_string())));
        let (status, body) = open(&app, WIDGET).await;
        assert_eq!((status, body), (StatusCode::BAD_GATEWAY, json!({ "error": "the browser failed: Chrome opened no page" })));
    }

    #[tokio::test]
    async fn a_call_that_is_not_one_of_the_three_is_turned_away() {
        let (browser, app) = scripted(Limits::default());
        let (_, opened) = open(&app, WIDGET).await;
        let id = opened["session"].as_str().unwrap();
        // An action that is none of the six, or lacks a part.
        for body in [json!({ "do": "buy" }), json!({ "do": "click" }), json!({ "do": "evaluate", "script": "1" }), json!({ "n": 1 }), json!("read")] {
            let (status, _) = act(&app, id, body.clone()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        }
        assert_eq!(call(&app, "POST", "/sessions", Some(json!({ "address": WIDGET }))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(call(&app, "POST", "/sessions", None).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        // No way in that is not one of the three: no listing, no reading a session back, no render.
        assert_eq!(call(&app, "GET", "/sessions", None).await.0, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(call(&app, "GET", &format!("/sessions/{id}"), None).await.0, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(call(&app, "POST", "/render", Some(json!({ "url": WIDGET }))).await.0, StatusCode::NOT_FOUND);
        assert_eq!(browser.asked.lock().unwrap().len(), 0, "none of it reached the page");
    }

    // ------------------------------------------- with a Chrome behind it

    use crate::tab::Chrome;
    use crate::testing::{self, html, Pages};

    /// A shop of four pages, served by the test.
    async fn shop() -> Pages {
        Pages::serve(&[
            (
                "/p/widget",
                html(
                    r#"<!doctype html><title>Widget kopen</title>
                    <div id="cookie-banner"><p>Wij gebruiken cookies.</p><button onclick="this.parentNode.remove()">Alleen noodzakelijk</button></div>
                    <main>
                      <h1>Widget</h1>
                      <p>Prijs <span id="price">12,99</span></p>
                      <select aria-label="Maat" onchange="document.getElementById('price').textContent = this.value === 'XL' ? '14,99' : '12,99'">
                        <option value="M" selected>M</option><option value="XL">XL</option>
                      </select>
                      <form action="/search"><input type="search" name="q" placeholder="Zoeken"></form>
                      <a href="/cart">In winkelwagen</a> <a href="/delivery">Bezorgen</a> <a href="/checkout">Afrekenen</a>
                    </main>"#,
                ),
            ),
            ("/cart", html(r#"<!doctype html><title>Winkelwagen</title><main><h1>Winkelwagen</h1><p>Widget 12,99</p><p>Verzendkosten 4,95</p><label>Postcode <input name="postcode"></label></main>"#)),
            ("/delivery", html("<!doctype html><title>Bezorgen</title><main><h1>Bezorgen</h1><p>Bezorgen kost 4,95.</p></main>")),
            ("/search", html("<!doctype html><title>Zoekresultaten</title><main><h1>Zoekresultaten</h1></main>")),
            ("/checkout", html("<!doctype html><title>Afrekenen</title><main><h1>Afrekenen</h1></main>")),
        ])
        .await
    }

    /// The routes over a real Chrome let through to `pages`, or `None` on
    /// a machine with no Chrome, after saying so.
    fn with_chrome(pages: &Pages, limits: Limits) -> Option<(Arc<Sessions<Chrome>>, Router)> {
        let sessions = Sessions::new(Chrome::reaching(testing::chrome_or_skip()?, pages.addr), limits);
        Some((sessions.clone(), router(sessions)))
    }

    #[tokio::test]
    async fn a_shop_is_browsed_from_its_product_page_to_its_basket_over_http() {
        let _one_of_few = testing::gate().await;
        let pages = shop().await;
        let Some((sessions, app)) = with_chrome(&pages, Limits::default()) else { return };

        let (status, opened) = open(&app, &pages.url("/p/widget")).await;
        assert_eq!(status, StatusCode::OK, "{opened}");
        let id = opened["session"].as_str().unwrap().to_string();
        let text = |step: &Value| step["page"]["text"].as_str().unwrap_or_default().to_string();
        assert_eq!(opened["page"]["title"], "Widget kopen");
        assert!(text(&opened).starts_with("# In front of the page\nWij gebruiken cookies.\n[1] button \"Alleen noodzakelijk\""), "{}", text(&opened));
        assert!(text(&opened).contains("Prijs 12,99") && text(&opened).contains(r#"[6] link "Afrekenen""#), "{}", text(&opened));

        // The notice in front of the page is put away, and the page is what is left.
        let (_, step) = act(&app, &id, json!({ "do": "click", "n": 1 })).await;
        assert!(text(&step).starts_with("# The page\nWidget"), "{}", text(&step));
        // Another size, and the price the page then shows.
        let (_, step) = act(&app, &id, json!({ "do": "select", "n": 1, "option": "XL" })).await;
        assert_eq!(step["notes"], json!(["Chosen: xl"]));
        assert!(text(&step).contains("Prijs 14,99"), "{}", text(&step));
        // A search, and back from it.
        let (_, step) = act(&app, &id, json!({ "do": "type", "n": 2, "text": "kattenvoer", "enter": true })).await;
        assert_eq!((&step["page"]["title"], step["page"]["url"].as_str()), (&json!("Zoekresultaten"), Some(pages.url("/search?q=kattenvoer").as_str())));
        let (_, step) = act(&app, &id, json!({ "do": "back" })).await;
        assert_eq!(step["page"]["title"], "Widget kopen");
        // The basket, with what delivery costs, and a postcode typed there.
        let basket = text(&step).lines().find(|line| line.contains("In winkelwagen")).and_then(|line| line.trim().strip_prefix('[')?.split(']').next()?.parse::<u32>().ok());
        let (_, step) = act(&app, &id, json!({ "do": "click", "n": basket.expect("the basket link is numbered") })).await;
        assert!(text(&step).contains("Verzendkosten 4,95"), "{}", text(&step));
        let (_, step) = act(&app, &id, json!({ "do": "type", "n": 1, "text": "1011 PN" })).await;
        assert!(text(&step).contains(r#"[1] field "Postcode" = "1011 PN""#), "{}", text(&step));
        // The shop's own page about delivery, by its address.
        let (_, step) = act(&app, &id, json!({ "do": "open", "url": pages.url("/delivery") })).await;
        assert!(text(&step).contains("Bezorgen kost 4,95."), "{}", text(&step));
        // And no further: the checkout is refused, with the page still the one it was.
        let (status, step) = act(&app, &id, json!({ "do": "open", "url": pages.url("/checkout") })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(step["refused"], json!(["that address is a checkout, and a session goes no further than the basket"]));
        assert_eq!((&step["page"]["title"], &step["actions_left"], &step["ended"]), (&json!("Bezorgen"), &json!(17), &Value::Null));
        assert!(!pages.was_asked("/checkout"));

        assert_eq!(call(&app, "DELETE", &format!("/sessions/{id}"), None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(sessions.free(), 3);
        assert_eq!(act(&app, &id, json!({ "do": "read" })).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_limits_hold_for_two_sessions_at_once_each_in_a_chrome_of_its_own() {
        let _one_of_few = testing::gate().await;
        let _two_of_few = testing::gate().await;
        let pages = shop().await;
        let limits = Limits { sessions: 2, actions: 3, time: Duration::from_secs(60) };
        let Some((sessions, app)) = with_chrome(&pages, limits) else { return };

        // Two at once, each on a page of its own.
        let (widget, cart) = (pages.url("/p/widget"), pages.url("/cart"));
        let ((status_a, a), (status_b, b)) = tokio::join!(open(&app, &widget), open(&app, &cart));
        assert_eq!((status_a, status_b), (StatusCode::OK, StatusCode::OK), "{a} {b}");
        let (id_a, id_b) = (a["session"].as_str().unwrap().to_string(), b["session"].as_str().unwrap().to_string());
        assert_ne!(id_a, id_b);
        assert_eq!((&a["page"]["title"], &b["page"]["title"]), (&json!("Widget kopen"), &json!("Winkelwagen")));
        assert_eq!(sessions.free(), 0);

        // A third finds no place and waits for one.
        let third = tokio::spawn({
            let (app, url) = (app.clone(), pages.url("/delivery"));
            async move { open(&app, &url).await }
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!third.is_finished(), "a third session was let in beside two");

        // Both are driven at the same moment. What one does is not counted against the other.
        let read = json!({ "do": "read" });
        let ((_, a1), (_, b1)) = tokio::join!(act(&app, &id_a, read.clone()), act(&app, &id_b, json!({ "do": "type", "n": 1, "text": "1011 PN" })));
        assert_eq!((&a1["actions_left"], &b1["actions_left"]), (&json!(2), &json!(2)));
        assert_eq!((&a1["page"]["title"], &b1["page"]["title"]), (&json!("Widget kopen"), &json!("Winkelwagen")));
        // What was typed in one is not in the other: each has a browser to itself.
        assert!(b1["page"]["text"].as_str().unwrap().contains("1011 PN") && !a1["page"]["text"].as_str().unwrap().contains("1011 PN"));
        let (_, a2) = act(&app, &id_a, read.clone()).await;
        let (status, a3) = act(&app, &id_a, read.clone()).await;
        assert_eq!((status, &a2["ended"], &a3["ended"]), (StatusCode::OK, &Value::Null, &json!("its 3 actions are spent")));
        let (status, a4) = act(&app, &id_a, read.clone()).await;
        assert_eq!((status, &a4["ended"]), (StatusCode::GONE, &json!("its 3 actions are spent")));

        // The place the first gave back goes to the one that was waiting.
        let (status, c) = third.await.unwrap();
        assert_eq!((status, &c["page"]["title"]), (StatusCode::OK, &json!("Bezorgen")), "{c}");
        // And the second still has what it had.
        let (status, b2) = act(&app, &id_b, read).await;
        assert_eq!((status, &b2["actions_left"], &b2["ended"]), (StatusCode::OK, &json!(1), &Value::Null));
        assert_eq!(sessions.free(), 0);
    }

    #[tokio::test]
    async fn a_session_with_a_chrome_behind_it_ends_itself_when_its_time_is_up() {
        let _one_of_few = testing::gate().await;
        let pages = shop().await;
        let limits = Limits { sessions: 1, actions: 25, time: Duration::from_secs(5) };
        let Some((sessions, app)) = with_chrome(&pages, limits) else { return };
        let (status, opened) = open(&app, &pages.url("/p/widget")).await;
        assert_eq!(status, StatusCode::OK, "{opened}");
        let id = opened["session"].as_str().unwrap();
        assert_eq!(sessions.free(), 0);
        // Nobody calls. The Chrome is stopped and the place given back.
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(sessions.free(), 1);
        let (status, body) = act(&app, id, json!({ "do": "read" })).await;
        assert_eq!((status, &body["ended"]), (StatusCode::GONE, &json!("its 5 seconds are spent")));
    }
}
