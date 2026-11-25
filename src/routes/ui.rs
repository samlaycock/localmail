//! The web UI: a React app in `ui/`, built into `ui/dist` and compiled into the binary. Each page
//! route serves its `index.html`, the app routes in the browser, and it reads everything through
//! the dev API. Debug builds read `ui/dist` from disk, so `npm run build` shows without a rebuild.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Dist;

pub async fn index() -> Response {
    // Revalidated every time, so an upgrade's new asset names are picked up.
    file("index.html", "no-cache")
}

pub async fn asset(Path(path): Path<String>) -> Response {
    if path.split('/').any(|part| part == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Vite puts a content hash in every asset's name, so an asset never changes.
    file(&format!("assets/{path}"), "public, max-age=31536000, immutable")
}

pub async fn favicon() -> Response {
    file("favicon.svg", "no-cache")
}

fn file(path: &str, cache_control: &'static str) -> Response {
    match Dist::get(path) {
        Some(f) => ([(header::CONTENT_TYPE, content_type(path)), (header::CACHE_CONTROL, cache_control)], f.data)
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}
