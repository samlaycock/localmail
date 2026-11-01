//! The email provider APIs: just the sending endpoints, answering like the real services do.
//! Each adapter reads its provider's request into [`Draft`]s (or the provider's validation error),
//! hands them to [`crate::send::submit`], and answers in its provider's format.

pub mod postmark;
pub mod resend;
pub mod sendgrid;
pub mod ses;

use axum::http::HeaderMap;
use axum::{Router, middleware};
use serde_json::{Map, Value};

use crate::message::{Attachment, Draft};
use crate::state::AppState;

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .merge(sendgrid::routes())
        .merge(postmark::routes())
        .merge(resend::routes())
        .merge(ses::routes())
        // Bodies are read (up to the limit) by the attempts recorder.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(state, crate::attempts::record))
}

/// The token in `Authorization: Bearer <token>`, if there's a non-empty one.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

pub fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

/// A string field, if present and not empty.
pub fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Decode an attachment's base64 content.
pub fn attachment(
    content_b64: &str,
    filename: &str,
    content_type: Option<&str>,
    content_id: Option<&str>,
    inline: bool,
) -> Option<Attachment> {
    let content = crate::util::b64_decode_lenient(content_b64)?;
    let content_id = content_id.map(|c| c.trim().trim_start_matches("cid:").trim_matches(['<', '>']).to_string());
    Some(Attachment {
        filename: filename.to_string(),
        content_type: content_type.map(String::from).unwrap_or_else(|| guess_type(filename).into()),
        content,
        content_id: content_id.filter(|c| !c.is_empty()),
        inline,
    })
}

/// A content type from a filename's extension, for attachments sent without one.
pub fn guess_type(filename: &str) -> &'static str {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Header name/value pairs from a JSON object (SendGrid, Resend).
pub fn header_map(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.clone(), crate::util::value_to_string(v))).collect(),
        _ => vec![],
    }
}

/// Copy the listed fields of a request, where present, into a draft's details.
pub fn keep(draft: &mut Draft, from: &Value, keys: &[&str]) {
    for k in keys {
        if let Some(v) = from.get(*k).filter(|v| !v.is_null()) {
            draft.details.insert((*k).to_string(), v.clone());
        }
    }
}

/// Merge two JSON objects (the second wins).
pub fn merged(a: Option<&Value>, b: Option<&Value>) -> Option<Value> {
    let mut out = Map::new();
    for v in [a, b].into_iter().flatten() {
        if let Value::Object(m) = v {
            out.extend(m.clone());
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}
