//! The Gmail API's send: `POST /gmail/v1/users/{userId}/messages/send` with the message as
//! base64url in `raw`, or uploaded to `/upload/gmail/v1/users/{userId}/messages/send` as
//! `uploadType=media` (the bare RFC 822 message) or `uploadType=multipart` (JSON metadata, then
//! the message). Gmail sends the MIME as it is, except that it removes the Bcc header.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{bearer, header, str_field};
use crate::message::{Draft, Provider, has_header, with_header};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gmail/v1/users/{user}/messages/send", post(send))
        .route("/upload/gmail/v1/users/{user}/messages/send", post(upload))
}

const MISSING_RAW: &str = "'raw' RFC822 payload message string or uploading message via /upload/* URL required";

async fn send(State(state): State<AppState>, Path(user): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let endpoint = format!("POST /gmail/v1/users/{user}/messages/send");
    let request = authorize(&headers).and_then(|()| {
        let v: Value = serde_json::from_slice(&body).map_err(|_| parse_error())?;
        if !v.is_object() {
            return Err(parse_error());
        }
        let raw = str_field(&v, "raw").ok_or_else(|| invalid(MISSING_RAW))?;
        let mime = crate::util::b64_decode_lenient(raw).ok_or_else(|| {
            invalid("Invalid value at 'message.raw' (TYPE_BYTES), Base64 decoding failed for \"raw\"")
        })?;
        let mut request = crate::util::elide_long_strings(&v);
        request["raw"] = Value::String(format!("({} bytes of base64url RFC 822, decoded below)", raw.len()));
        Ok((mime, str_field(&v, "threadId").map(String::from), request))
    });
    finish(&state, &endpoint, &mailbox(&user, &headers), &user, request).await
}

async fn upload(
    State(state): State<AppState>,
    Path(user): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upload_type = query.get("uploadType").map(String::as_str).unwrap_or_default();
    let endpoint = format!("POST /upload/gmail/v1/users/{user}/messages/send?uploadType={upload_type}");
    let content_type = header(&headers, "content-type").unwrap_or_default().to_string();
    let request = authorize(&headers).and_then(|()| match upload_type {
        "media" => {
            media_type(&content_type)?;
            Ok((body.to_vec(), None, json!({ "uploadType": "media" })))
        }
        "multipart" => {
            let (metadata, mime) = multipart_related(&content_type, &body)?;
            let thread_id = metadata.as_ref().and_then(|m| str_field(m, "threadId")).map(String::from);
            Ok((mime, thread_id, json!({ "uploadType": "multipart", "metadata": metadata })))
        }
        "" => Err(invalid("Upload requests must include an uploadType URL parameter and a URL path beginning with /upload/")),
        other => Err(invalid(format!(
            "Invalid uploadType \"{other}\". localmail accepts uploadType=media and uploadType=multipart (resumable uploads aren't emulated)."
        ))),
    });
    finish(&state, &endpoint, &mailbox(&user, &headers), &user, request).await
}

/// The sending mailbox's address, if known: the userId when it's one, or the user an access
/// token was issued for.
fn mailbox(user: &str, headers: &HeaderMap) -> Option<String> {
    if crate::address::is_valid_email(user) {
        return Some(user.to_string());
    }
    bearer(headers).and_then(super::tokens::mailbox_of)
}

/// Read the message, store it, and answer like Gmail.
async fn finish(
    state: &AppState,
    endpoint: &str,
    mailbox: &Option<String>,
    user: &str,
    request: Result<(Vec<u8>, Option<String>, Value), Failure>,
) -> Response {
    let thread_id = request.as_ref().ok().and_then(|(_, t, _)| t.clone());
    let items = request
        .and_then(|(mime, thread_id, request)| read(mime, thread_id, request, user, mailbox.as_deref()))
        .map(|d| vec![Ok(d)]);
    let outcome = submit(state, Provider::Gmail, endpoint, items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(sent)) => Json(json!({
                "id": sent.message_id,
                "threadId": thread_id.unwrap_or_else(|| sent.message_id.clone()),
                "labelIds": ["SENT"],
            }))
            .into_response(),
            Some(ItemResult::Failed(f)) => error(f),
            None => error(&Failure::ServerError),
        },
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn read(
    mime: Vec<u8>,
    thread_id: Option<String>,
    request: Value,
    user: &str,
    mailbox: Option<&str>,
) -> Result<Draft, Failure> {
    // Gmail fills in a missing From with the account's address.
    let mime = match mailbox {
        Some(m) if !has_header(&mime, "from") => with_header(mime, "From", m),
        _ => mime,
    };
    let mut draft = Draft::from_sent_mime(mime);
    if draft.recipients().next().is_none() {
        return Err(invalid("Recipient address required"));
    }
    if let Some(bad) = draft.recipients().find(|a| !a.is_valid()) {
        let field = if draft.to.iter().any(|a| a.key() == bad.key()) { "To" } else { "Cc" };
        return Err(invalid(format!("Invalid {field} header")));
    }
    draft.details.insert("userId".into(), Value::String(user.into()));
    if let Some(t) = thread_id {
        draft.details.insert("threadId".into(), Value::String(t));
    }
    draft.request = Some(request);
    Ok(draft)
}

fn authorize(headers: &HeaderMap) -> Result<(), Failure> {
    bearer(headers).map(|_| ()).ok_or(Failure::MissingAuth)
}

fn media_type(content_type: &str) -> Result<(), Failure> {
    let essence = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if essence.starts_with("message/") {
        return Ok(());
    }
    let shown = if essence.is_empty() { "application/octet-stream" } else { &essence };
    Err(invalid(format!("Media type '{shown}' is not supported. Valid media types: [message/*]")))
}

/// A `multipart/related` upload: optional JSON metadata, then the message.
fn multipart_related(content_type: &str, body: &[u8]) -> Result<(Option<Value>, Vec<u8>), Failure> {
    let boundary = content_type
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("boundary"))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .ok_or_else(|| invalid("Multipart upload requires a boundary in the Content-Type header."))?;
    let parts = split_multipart(body, &boundary);
    let mut metadata = None;
    let mut mime = None;
    for (part_headers, content) in parts {
        let ct = part_headers
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-type")))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if ct.starts_with("application/json") && metadata.is_none() && mime.is_none() {
            metadata = Some(serde_json::from_slice::<Value>(&content).map_err(|_| parse_error())?);
        } else if mime.is_none() {
            media_type(&ct)?;
            let base64 = part_headers.lines().any(|l| {
                l.split_once(':').is_some_and(|(k, v)| {
                    k.trim().eq_ignore_ascii_case("content-transfer-encoding")
                        && v.trim().eq_ignore_ascii_case("base64")
                })
            });
            mime = Some(if base64 {
                crate::util::b64_decode_lenient(&String::from_utf8_lossy(&content))
                    .ok_or_else(|| invalid("Invalid base64 in the uploaded message."))?
            } else {
                content
            });
        }
    }
    let mime = mime.ok_or_else(|| invalid(MISSING_RAW))?;
    Ok((metadata, mime))
}

/// The parts of a multipart body: each part's headers (as text) and content.
fn split_multipart(body: &[u8], boundary: &str) -> Vec<(String, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let d = delimiter.as_bytes();
    let mut starts = vec![];
    let mut i = 0;
    while i + d.len() <= body.len() {
        if &body[i..i + d.len()] == d && (i == 0 || body[i - 1] == b'\n') {
            starts.push(i);
            i += d.len();
        } else {
            i += 1;
        }
    }
    let mut parts = vec![];
    for w in starts.windows(2) {
        let mut seg = &body[w[0] + d.len()..w[1]];
        // The rest of the delimiter line, and the line break before the next delimiter.
        seg = match seg.iter().position(|&b| b == b'\n') {
            Some(p) => &seg[p + 1..],
            None => continue,
        };
        seg = seg.strip_suffix(b"\n").unwrap_or(seg);
        seg = seg.strip_suffix(b"\r").unwrap_or(seg);
        let (head, content) = match seg.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(p) => (&seg[..p], &seg[p + 4..]),
            None => match seg.windows(2).position(|w| w == b"\n\n") {
                Some(p) => (&seg[..p], &seg[p + 2..]),
                None => (&seg[..0], seg),
            },
        };
        parts.push((String::from_utf8_lossy(head).into_owned(), content.to_vec()));
    }
    parts
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid { message: message.into(), code: Some("invalidArgument".into()), ..Default::default() })
}

fn parse_error() -> Failure {
    Failure::Invalid(Invalid {
        message: "Invalid JSON payload received. Unexpected token.".into(),
        code: Some("parseError".into()),
        ..Default::default()
    })
}

/// A Google API error: `{"error": {code, message, errors: [...], status}}`.
fn google(status: StatusCode, message: &str, detail: Value, rpc_status: &str) -> Response {
    let mut item = json!({ "message": message, "domain": "global" });
    if let (Value::Object(item), Value::Object(detail)) = (&mut item, detail) {
        item.extend(detail);
    }
    let body =
        json!({ "error": { "code": status.as_u16(), "message": message, "errors": [item], "status": rpc_status } });
    (status, Json(body)).into_response()
}

fn error(f: &Failure) -> Response {
    const SEE: &str = "Expected OAuth 2 access token, login cookie or other valid authentication credential. See https://developers.google.com/identity/sign-in/web/devconsole-project.";
    let auth = |message: String, item: &str, reason: &str| {
        let mut resp = google(
            StatusCode::UNAUTHORIZED,
            &message,
            json!({ "message": item, "reason": reason, "location": "Authorization", "locationType": "header" }),
            "UNAUTHENTICATED",
        );
        let www = match reason {
            "authError" => r#"Bearer realm="https://accounts.google.com/", error="invalid_token""#,
            _ => r#"Bearer realm="https://accounts.google.com/""#,
        };
        resp.headers_mut().insert("www-authenticate", www.parse().unwrap());
        resp
    };
    match f {
        Failure::MissingAuth => {
            auth(format!("Request is missing required authentication credential. {SEE}"), "Login Required.", "required")
        }
        Failure::BadAuth => {
            auth(format!("Request had invalid authentication credentials. {SEE}"), "Invalid Credentials", "authError")
        }
        Failure::Invalid(i) => {
            let reason = i.code.clone().unwrap_or_else(|| "invalidArgument".into());
            let detail = match reason.as_str() {
                "parseError" => json!({ "message": "Parse Error", "reason": reason }),
                _ => json!({ "reason": reason }),
            };
            google(StatusCode::BAD_REQUEST, &i.message, detail, "INVALID_ARGUMENT")
        }
        Failure::Rejected(addr) => google(
            StatusCode::BAD_REQUEST,
            &format!("Invalid To header: {addr} was rejected"),
            json!({ "reason": "invalidArgument" }),
            "INVALID_ARGUMENT",
        ),
        Failure::RateLimited => {
            let retry = crate::util::iso8601(crate::util::wall_clock() + 60).replace('Z', ".000Z");
            let message = format!("User-rate limit exceeded.  Retry after {retry}");
            let mut resp = google(
                StatusCode::TOO_MANY_REQUESTS,
                &message,
                json!({ "domain": "usageLimits", "reason": "rateLimitExceeded" }),
                "RESOURCE_EXHAUSTED",
            );
            resp.headers_mut().insert("retry-after", "60".parse().unwrap());
            resp
        }
        Failure::ServerError => {
            google(StatusCode::INTERNAL_SERVER_ERROR, "Backend Error", json!({ "reason": "backendError" }), "INTERNAL")
        }
    }
}
