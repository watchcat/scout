//! Signing in: the form that asks for an address.
//!
//! Mounted only when `AuthConfig::from_env` found every key, so every
//! handler here can assume it has somewhere to mail a link to.

use crate::{pages, session, AuthState};
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

/// The signed-in half of the site, over its own state.
pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/sign-in", get(sign_in_page))
        .with_state(auth)
}

async fn sign_in_page(State(auth): State<AuthState>) -> Html<String> {
    Html(pages::sign_in(&session::csrf(&auth.cfg.session_key)))
}
