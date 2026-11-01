pub mod api;
pub mod pages;

use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::inbox))
        .route("/attempts", get(pages::attempts))
        .route("/setup", get(pages::setup))
        .route("/assets/app.css", get(pages::css))
        .route("/assets/app.js", get(pages::js))
        .route("/health", get(|| async { "ok" }))
        .route("/dev/api/cursor", get(api::cursor))
        .route("/dev/api/mailboxes", get(api::mailboxes))
        .route("/dev/api/messages", get(api::list).delete(api::delete_matching))
        .route("/dev/api/messages/wait", get(api::wait))
        .route("/dev/api/messages/read", post(api::mark_all_read))
        .route("/dev/api/messages/{id}", get(api::get).delete(api::delete))
        .route("/dev/api/messages/{id}/read", post(api::mark_read))
        .route("/dev/api/messages/{id}/raw", get(api::raw))
        .route("/dev/api/messages/{id}/html", get(api::html))
        .route("/dev/api/messages/{id}/extract", get(api::extract_handler))
        .route("/dev/api/messages/{id}/attachments/{index}", get(api::attachment))
        .route("/dev/api/messages/{id}/cid/{cid}", get(api::cid))
        .route("/dev/api/attempts", get(api::attempts).delete(api::attempts_clear))
        .route("/dev/api/scenarios", get(api::scenarios))
        .route("/dev/api/scenarios/reset", post(api::scenarios_reset))
        .merge(crate::providers::routes(state.clone()))
        .layer(CorsLayer::very_permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
