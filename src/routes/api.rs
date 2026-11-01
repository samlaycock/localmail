//! The dev API: list, wait for, read and delete messages, pull codes and links out of them, read
//! the attempts log and reset scenarios. The web UI uses it too.

// Helpers return the error response itself, ready to send.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde_json::{Value, json};

use crate::extract;
use crate::message::{Provider, Stored};
use crate::state::AppState;
use crate::store::Filter;

/// Longest `wait` allowed.
pub const MAX_WAIT_SECS: i64 = 300;
const DEFAULT_WAIT_SECS: i64 = 30;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// A JSON error: `{"error": "...", "error_description": "..."}`.
fn api_error(status: StatusCode, error: &str, description: impl Into<String>) -> Response {
    (status, no_store(), Json(json!({ "error": error, "error_description": description.into() }))).into_response()
}

fn invalid(description: impl Into<String>) -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_request", description)
}

fn not_found(id: &str) -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", format!("no message '{id}'"))
}

/// Query parameters, checked against what each endpoint accepts so a typo (`subjet=`) is an error
/// rather than a filter that silently matches everything.
struct Params(HashMap<String, String>);

const FILTER_KEYS: [&str; 7] = ["to", "from", "subject", "subject_contains", "provider", "q", "after"];
const EXTRACT_KEYS: [&str; 4] = ["extract", "otp_pattern", "regex", "link_contains"];

impl Params {
    fn check(map: HashMap<String, String>, allowed: &[&[&str]]) -> Result<Self, Response> {
        for k in map.keys() {
            if !allowed.iter().any(|set| set.contains(&k.as_str())) {
                let all: Vec<&str> = allowed.iter().flat_map(|s| s.iter().copied()).collect();
                return Err(invalid(format!("unknown parameter '{k}' (accepted: {})", all.join(", "))));
            }
        }
        Ok(Self(map))
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str).filter(|v| !v.trim().is_empty())
    }

    fn filter(&self) -> Result<Filter, Response> {
        let provider = match self.get("provider") {
            Some(p) => Some(Provider::parse(p).ok_or_else(|| {
                invalid(format!(
                    "unknown provider '{p}' (use {})",
                    Provider::ALL.iter().map(|p| p.id()).collect::<Vec<_>>().join(", ")
                ))
            })?),
            None => None,
        };
        let after = match self.get("after") {
            Some(a) => Some(a.parse::<i64>().map_err(|_| invalid("after must be a message id (a cursor)"))?),
            None => None,
        };
        Ok(Filter {
            to: self.get("to").map(String::from),
            from: self.get("from").map(String::from),
            subject: self.get("subject").map(String::from),
            subject_contains: self.get("subject_contains").map(String::from),
            provider,
            q: self.get("q").map(String::from),
            after,
        })
    }

    fn extract(&self) -> Result<extract::Request, Response> {
        extract::Request::parse(
            self.get("extract"),
            self.get("otp_pattern"),
            self.get("regex"),
            self.get("link_contains"),
        )
        .map_err(invalid)
    }

    fn usize(&self, k: &str, default: usize, max: usize) -> Result<usize, Response> {
        match self.get(k) {
            Some(v) => v.parse::<usize>().map(|n| n.min(max)).map_err(|_| invalid(format!("{k} must be a number"))),
            None => Ok(default),
        }
    }
}

fn extracted(m: &Stored, req: &extract::Request) -> Value {
    req.run(&m.searchable_text(), m.html().as_deref())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

pub async fn cursor(State(state): State<AppState>) -> Response {
    (no_store(), Json(json!({ "cursor": state.store.cursor() }))).into_response()
}

pub async fn list(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let params = match Params::check(q, &[&FILTER_KEYS, &["limit", "offset"]]) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (filter, limit, offset) =
        match (params.filter(), params.usize("limit", DEFAULT_LIMIT, MAX_LIMIT), params.usize("offset", 0, usize::MAX))
        {
            (Ok(f), Ok(l), Ok(o)) => (f, l, o),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
    let body = json!({
        "messages": state.store.list(&filter, limit, offset),
        "total": state.store.count(&filter),
        "unread": state.store.unread(),
        "cursor": state.store.cursor(),
    });
    (no_store(), Json(body)).into_response()
}

pub async fn delete_matching(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let filter = match Params::check(q, &[&FILTER_KEYS]).and_then(|p| p.filter()) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let deleted = state.store.delete_matching(&filter);
    (no_store(), Json(json!({ "deleted": deleted }))).into_response()
}

/// Long-poll for the first message matching the filters (after the cursor, if given).
pub async fn wait(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let params = match Params::check(q, &[&FILTER_KEYS, &EXTRACT_KEYS, &["timeout"]]) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (filter, req) = match (params.filter(), params.extract()) {
        (Ok(f), Ok(r)) => (f, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let timeout = match params.get("timeout").map(crate::util::parse_duration).unwrap_or(Ok(DEFAULT_WAIT_SECS)) {
        Ok(t) if (0..=MAX_WAIT_SECS).contains(&t) => t,
        Ok(_) => return invalid(format!("timeout must be at most {MAX_WAIT_SECS}s")),
        Err(e) => return invalid(e),
    };

    // Subscribe before looking, so a message stored in between still wakes us up.
    let mut changes = state.store.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
    loop {
        if let Some(m) = state.store.first(&filter) {
            let mut body = json!({ "message": m.detail() });
            if !req.is_empty() {
                body["extracted"] = extracted(&m, &req);
            }
            return (no_store(), Json(body)).into_response();
        }
        tokio::select! {
            changed = changes.changed() => if changed.is_err() { break },
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    // Say what did arrive, since a wrong filter is the usual reason.
    let after = filter.after.unwrap_or(0);
    let since = Filter { after: Some(after), ..Default::default() };
    let recent = state.store.list(&since, 5, 0);
    let description = format!(
        "no message matched within {timeout}s; {} message(s) arrived after cursor {after}{}",
        state.store.count(&since),
        if recent.is_empty() { String::new() } else { " (the newest are in \"recent\")".into() }
    );
    let body = json!({
        "error": "timeout",
        "error_description": description,
        "cursor": state.store.cursor(),
        "recent": recent,
    });
    (StatusCode::REQUEST_TIMEOUT, no_store(), Json(body)).into_response()
}

/// A message by id, or `latest` (the newest, optionally matching filters).
fn find(state: &AppState, id: &str, params: Option<&Params>) -> Result<Stored, Response> {
    if id == "latest" {
        let filter = match params {
            Some(p) => p.filter()?,
            None => Filter::default(),
        };
        let newest = state.store.list(&filter, 1, 0).into_iter().next().ok_or_else(|| not_found(id))?;
        return state.store.get(newest.id).ok_or_else(|| not_found(id));
    }
    let n: i64 = id.parse().map_err(|_| invalid("message ids are numbers (or 'latest')"))?;
    state.store.get(n).ok_or_else(|| not_found(id))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let found = Params::check(q, &[&FILTER_KEYS, &EXTRACT_KEYS]).and_then(|p| {
        let m = find(&state, &id, Some(&p))?;
        let req = p.extract()?;
        Ok((m, req))
    });
    match found {
        Ok((m, req)) => {
            let mut body = serde_json::to_value(m.detail()).unwrap_or_default();
            if !req.is_empty() {
                body["extracted"] = extracted(&m, &req);
            }
            (no_store(), Json(body)).into_response()
        }
        Err(e) => e,
    }
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match id.parse::<i64>() {
        Ok(n) if state.store.delete(n) => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(&id),
        Err(_) => invalid("message ids are numbers"),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {
    pub read: Option<bool>,
}

pub async fn mark_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ReadRequest>>,
) -> Response {
    let read = body.and_then(|b| b.0.read).unwrap_or(true);
    match id.parse::<i64>() {
        Ok(n) if state.store.set_read(Some(n), read) > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(&id),
        Err(_) => invalid("message ids are numbers"),
    }
}

pub async fn mark_all_read(State(state): State<AppState>) -> Response {
    let n = state.store.set_read(None, true);
    (no_store(), Json(json!({ "updated": n }))).into_response()
}

pub async fn extract_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let result = Params::check(q, &[&EXTRACT_KEYS]).and_then(|p| {
        let m = find(&state, &id, None)?;
        let mut req = p.extract()?;
        if req.is_empty() {
            req.otp = true;
            req.links = true;
        }
        Ok((m, req))
    });
    match result {
        Ok((m, req)) => {
            let mut body = extracted(&m, &req);
            body["message_id"] = m.summary.id.into();
            (no_store(), Json(body)).into_response()
        }
        Err(e) => e,
    }
}

pub async fn raw(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let m = match find(&state, &id, None) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let disposition = if q.contains_key("download") {
        format!("attachment; filename=\"message-{}.eml\"", m.summary.id)
    } else {
        "inline".into()
    };
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        m.raw,
    )
        .into_response()
}

/// Rendered email HTML runs in a sandbox: no scripts, no access to this origin. Links open in a
/// new tab, and inline images (`cid:`) point at the message's parts.
const HTML_CSP: &str = "default-src 'none'; img-src * data: blob:; style-src * 'unsafe-inline'; font-src * data:; media-src * data:; sandbox allow-popups allow-popups-to-escape-sandbox";

fn cid_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)\bcid:([^"'\s)>]+)"#).unwrap())
}

pub async fn html(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let m = match find(&state, &id, None) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let Some(html) = m.html() else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "this message has no HTML part");
    };
    let msg_id = m.summary.id;
    let html = cid_regex().replace_all(&html, |c: &regex::Captures| {
        let cid: String = url::form_urlencoded::byte_serialize(c[1].as_bytes()).collect();
        format!("/dev/api/messages/{msg_id}/cid/{cid}")
    });
    let html = with_base(&html);
    let mut resp = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(HTML_CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

/// Add `<base target="_blank">` (links open in a new tab) inside `<head>`, or after `<html>`, so a
/// doctype still comes first and the page isn't rendered in quirks mode.
fn with_base(html: &str) -> String {
    const BASE: &str = r#"<base target="_blank">"#;
    let lower = html.get(..html.len().min(4096)).unwrap_or(html).to_ascii_lowercase();
    let after = |tag: &str| lower.find(tag).and_then(|i| lower[i..].find('>').map(|j| i + j + 1));
    match after("<head").or_else(|| after("<html")) {
        Some(i) => format!("{}{BASE}{}", &html[..i], &html[i..]),
        None => format!("{BASE}{html}"),
    }
}

fn serve_part(m: &Stored, index: usize, download: bool) -> Response {
    let Some((content_type, filename, bytes)) = m.attachment(index) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", format!("no attachment {index}"));
    };
    let name = filename.replace(['"', '\\', '\r', '\n'], "_");
    let disposition = format!("{}; filename=\"{name}\"", if download { "attachment" } else { "inline" });
    let mut resp =
        ([(header::CONTENT_TYPE, content_type), (header::CONTENT_DISPOSITION, disposition)], bytes).into_response();
    // Attachments are untrusted: never let one run as this origin.
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'"),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

pub async fn attachment(
    State(state): State<AppState>,
    Path((id, index)): Path<(String, usize)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match find(&state, &id, None) {
        Ok(m) => serve_part(&m, index, q.contains_key("download")),
        Err(e) => e,
    }
}

pub async fn cid(State(state): State<AppState>, Path((id, cid)): Path<(String, String)>) -> Response {
    let m = match find(&state, &id, None) {
        Ok(m) => m,
        Err(e) => return e,
    };
    match m.attachment_by_cid(&cid) {
        Some(index) => serve_part(&m, index, false),
        None => api_error(StatusCode::NOT_FOUND, "not_found", format!("no part with Content-ID '{cid}'")),
    }
}

// ---------------------------------------------------------------------------
// Attempts and scenarios
// ---------------------------------------------------------------------------

pub async fn attempts(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let params = match Params::check(q, &[&["provider", "to", "errors", "after", "limit"]]) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let provider = match params.get("provider").map(|p| Provider::parse(p).ok_or(p)) {
        Some(Err(p)) => return invalid(format!("unknown provider '{p}'")),
        Some(Ok(p)) => Some(p),
        None => None,
    };
    let after = match params.get("after").map(str::parse::<u64>) {
        Some(Err(_)) => return invalid("after must be an attempt id"),
        Some(Ok(a)) => Some(a),
        None => None,
    };
    let limit = match params.usize("limit", crate::attempts::CAPACITY, crate::attempts::CAPACITY) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let filter = crate::attempts::Filter {
        provider,
        to: params.get("to").map(String::from),
        errors_only: params.get("errors").is_some_and(|v| v == "true" || v == "1"),
        after,
        limit: Some(limit),
    };
    let body = json!({
        "attempts": state.attempts.list(&filter),
        // Lets a poller notice a restart: ids start again from 1.
        "latest_id": state.attempts.latest_id(),
        "capacity": crate::attempts::CAPACITY,
    });
    (no_store(), Json(body)).into_response()
}

pub async fn attempts_clear(State(state): State<AppState>) -> StatusCode {
    state.attempts.clear();
    StatusCode::NO_CONTENT
}

pub async fn scenarios(State(state): State<AppState>) -> Response {
    let domain = &state.config.scenario_domain;
    let body = json!({
        "domain": domain,
        "scenarios": crate::scenarios::docs(domain),
        "flaky_counters": state.flaky.snapshot(),
    });
    (no_store(), Json(body)).into_response()
}

pub async fn scenarios_reset(State(state): State<AppState>) -> Response {
    let n = state.flaky.reset();
    (no_store(), Json(json!({ "reset": n }))).into_response()
}
