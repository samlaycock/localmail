//! Microsoft Graph's sendMail (Outlook and Microsoft 365): `POST /v1.0/me/sendMail` and
//! `POST /v1.0/users/{id}/sendMail` (and the same under `/beta`). The message is JSON, or MIME
//! sent base64-encoded as `text/plain`. Success is `202 Accepted` with no body and no message id.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{attachment, bearer, header, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, has_header, with_header};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1.0/me/sendMail", post(send_me))
        .route("/v1.0/users/{user}/sendMail", post(send_user))
        .route("/beta/me/sendMail", post(send_me_beta))
        .route("/beta/users/{user}/sendMail", post(send_user_beta))
}

async fn send_me(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    send(&state, "v1.0", None, &headers, &body).await
}

async fn send_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    send(&state, "v1.0", Some(user), &headers, &body).await
}

async fn send_me_beta(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    send(&state, "beta", None, &headers, &body).await
}

async fn send_user_beta(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    send(&state, "beta", Some(user), &headers, &body).await
}

async fn send(state: &AppState, version: &str, user: Option<String>, headers: &HeaderMap, body: &[u8]) -> Response {
    let endpoint = match &user {
        Some(u) => format!("POST /{version}/users/{u}/sendMail"),
        None => format!("POST /{version}/me/sendMail"),
    };
    // A user id that's an address is the mailbox sending; for `me`, the token may say whose it is.
    let mailbox = match &user {
        Some(u) => crate::address::is_valid_email(u).then(|| Addr::new(None, u)),
        None => bearer(headers).and_then(super::tokens::mailbox_of).map(|m| Addr::new(None, &m)),
    };
    let mime = header(headers, "content-type").is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/plain"));
    let items = authorize(headers)
        .and_then(|()| if mime { read_mime(body, mailbox.as_ref()) } else { read_json(body, mailbox.as_ref()) })
        .map(|mut d| {
            if let Some(u) = &user {
                d.details.insert("user".into(), Value::String(u.clone()));
            }
            vec![Ok(d)]
        });
    let mut outcome = submit(state, Provider::Graph, &endpoint, items, false).await;
    if headers.get(axum::http::header::AUTHORIZATION).is_none() {
        outcome.note.warnings.push(
            "no access token: Graph needs one, but the Graph JavaScript SDK leaves it out over plain http, so localmail doesn't insist"
                .into(),
        );
    }
    let client_request_id = header(headers, "client-request-id").map(String::from);
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(_)) => StatusCode::ACCEPTED.into_response(),
            Some(ItemResult::Failed(f)) => error(f, client_request_id.as_deref()),
            None => error(&Failure::ServerError, client_request_id.as_deref()),
        },
        Err(f) => error(f, client_request_id.as_deref()),
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let h = resp.headers_mut();
    h.insert("request-id", request_id.parse().unwrap());
    if let Some(v) = client_request_id.and_then(|c| c.parse().ok()) {
        h.insert("client-request-id", v);
    }
    outcome.note.attach(&mut resp);
    resp
}

/// A token is needed, but a request without an Authorization header at all is let through: the
/// Graph JavaScript SDK removes it from any request that isn't https.
fn authorize(headers: &HeaderMap) -> Result<(), Failure> {
    if headers.get(axum::http::header::AUTHORIZATION).is_none() {
        return Ok(());
    }
    bearer(headers).map(|_| ()).ok_or(Failure::MissingAuth)
}

/// Graph's JSON with every property name in camelCase: the Python SDK sends `Message` and
/// `SaveToSentItems`, and Graph matches names case-insensitively.
fn camel_case(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let mut chars = k.chars();
                    let k = match chars.next() {
                        Some(c) => c.to_lowercase().chain(chars).collect(),
                        None => k,
                    };
                    (k, camel_case(v))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(camel_case).collect()),
        other => other,
    }
}

fn coded(code: &str, message: impl Into<String>) -> Failure {
    Failure::coded(message, code)
}

fn no_recipients() -> Failure {
    coded(
        "ErrorInvalidRecipients",
        "At least one recipient isn't valid., A message can't be sent because it contains no recipients.",
    )
}

fn unresolved(addr: &str) -> Failure {
    coded(
        "ErrorInvalidRecipients",
        format!(
            "At least one recipient isn't valid., Recipient '{addr}' isn't resolved. All recipients must be resolved before a message can be submitted."
        ),
    )
}

fn check_recipients(d: &Draft) -> Result<(), Failure> {
    if d.recipients().next().is_none() {
        return Err(no_recipients());
    }
    match d.recipients().find(|a| !a.is_valid()) {
        Some(bad) => Err(unresolved(&bad.email)),
        None => Ok(()),
    }
}

/// MIME, base64-encoded, as the body of a `text/plain` request.
fn read_mime(body: &[u8], mailbox: Option<&Addr>) -> Result<Draft, Failure> {
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Err(coded("ErrorMimeContentInvalid", "The MIME content is empty."));
    }
    let mut mime = crate::util::b64_decode_lenient(&text)
        .ok_or_else(|| coded("ErrorMimeContentInvalidBase64String", "Invalid base64 string for MIME content."))?;
    if let Some(m) = mailbox.filter(|_| !has_header(&mime, "from")) {
        mime = with_header(mime, "From", &m.email);
    }
    let mut d = Draft::from_sent_mime(mime);
    check_recipients(&d)?;
    d.request = Some(json!({ "mime": format!("({} bytes of base64 MIME, decoded below)", text.trim().len()) }));
    Ok(d)
}

/// `{"emailAddress": {"address", "name"}}`.
fn recipient(v: &Value) -> Addr {
    let e = v.get("emailAddress").unwrap_or(&Value::Null);
    Addr::new(str_field(e, "name"), str_field(e, "address").unwrap_or_default())
}

fn recipients(message: &Value, key: &str) -> Result<Vec<Addr>, Failure> {
    match message.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(list)) => {
            let list: Vec<Addr> = list.iter().map(recipient).collect();
            match list.iter().find(|a| !a.is_valid()) {
                Some(bad) => Err(unresolved(&bad.email)),
                None => Ok(list),
            }
        }
        Some(_) => Err(coded(
            "BadRequest",
            format!("Invalid value for property '{key}'. A collection of recipients is expected."),
        )),
    }
}

fn read_json(body: &[u8], mailbox: Option<&Addr>) -> Result<Draft, Failure> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(coded("BadRequest", "Empty Payload. JSON content expected."));
    }
    let req: Value = serde_json::from_slice(body).map(camel_case).map_err(|_| {
        coded(
            "BadRequest",
            "Unable to read JSON request payload. Please ensure Content-Type header is set and payload is of valid JSON format.",
        )
    })?;
    let message = match req.get("message") {
        Some(m) if m.is_object() => m,
        _ => return Err(coded("ErrorInvalidParameter", "The value of the parameter 'Message' is empty.")),
    };
    let from = match message.get("from").filter(|v| !v.is_null()) {
        Some(f) => {
            let a = recipient(f);
            if !a.is_valid() {
                return Err(coded("ErrorInvalidFromAddress", "The from address isn't valid."));
            }
            Some(a)
        }
        None => mailbox.cloned(),
    };
    let body = message.get("body").unwrap_or(&Value::Null);
    let content = str_field(body, "content").map(String::from);
    let html = str_field(body, "contentType").is_some_and(|t| t.eq_ignore_ascii_case("html"));
    let mut headers = vec![];
    if let Some(Value::Array(list)) = message.get("internetMessageHeaders") {
        if list.len() > 5 {
            return Err(coded(
                "InvalidInternetMessageHeaderCollection",
                "Maximum number of headers in one message should be less than or equal to 5.",
            ));
        }
        for h in list {
            let name = str_field(h, "name").unwrap_or_default();
            if !name.to_ascii_lowercase().starts_with("x-") {
                return Err(coded(
                    "InvalidInternetMessageHeader",
                    format!("The internet message header name '{name}' should start with 'x-' or 'X-'."),
                ));
            }
            headers.push((name.to_string(), str_field(h, "value").unwrap_or_default().to_string()));
        }
    }
    if let Some(importance) = str_field(message, "importance") {
        match importance.to_ascii_lowercase().as_str() {
            "high" => headers.push(("Importance".into(), "High".into())),
            "low" => headers.push(("Importance".into(), "Low".into())),
            _ => {}
        }
    }
    let mut draft = Draft {
        from,
        to: recipients(message, "toRecipients")?,
        cc: recipients(message, "ccRecipients")?,
        bcc: recipients(message, "bccRecipients")?,
        reply_to: recipients(message, "replyTo")?,
        subject: str_field(message, "subject").unwrap_or_default().to_string(),
        text: if html { None } else { Some(content.clone().unwrap_or_default()) },
        html: if html { content } else { None },
        headers,
        request: Some(crate::util::elide_long_strings(&req)),
        ..Default::default()
    };
    check_recipients(&draft)?;
    if let Some(Value::Array(list)) = message.get("attachments") {
        for a in list {
            let kind = str_field(a, "@odata.type").unwrap_or("#microsoft.graph.fileAttachment");
            let name = str_field(a, "name").unwrap_or("attachment");
            if !kind.eq_ignore_ascii_case("#microsoft.graph.fileAttachment") {
                // Item and reference attachments point into the mailbox, which localmail doesn't have.
                draft.attachments.push(crate::message::Attachment {
                    filename: format!("{name}.txt"),
                    content_type: "text/plain".into(),
                    content: format!("localmail can't show this {kind}: it refers to something in the mailbox.\n")
                        .into_bytes(),
                    content_id: None,
                    inline: false,
                });
                continue;
            }
            let bytes = str_field(a, "contentBytes").ok_or_else(|| {
                coded("ErrorInvalidAttachment", format!("The attachment '{name}' has no contentBytes."))
            })?;
            let inline = a.get("isInline").and_then(Value::as_bool).unwrap_or(false);
            let att = attachment(bytes, name, str_field(a, "contentType"), str_field(a, "contentId"), inline)
                .ok_or_else(|| coded("ErrorInvalidAttachment", "Invalid base64 string for attachment content."))?;
            draft.attachments.push(att);
        }
    }
    if let Some(Value::Array(cats)) = message.get("categories") {
        draft.tags = cats.iter().filter_map(Value::as_str).map(String::from).collect();
    }
    super::keep(&mut draft, &req, &["saveToSentItems"]);
    super::keep(&mut draft, message, &["importance", "isDeliveryReceiptRequested", "isReadReceiptRequested", "flag"]);
    Ok(draft)
}

fn error(f: &Failure, client_request_id: Option<&str>) -> Response {
    let (status, code, message) = match f {
        // An Authorization header that isn't a usable bearer token (none at all is let through).
        Failure::MissingAuth => {
            (StatusCode::UNAUTHORIZED, "InvalidAuthenticationToken".into(), "UnableToParseTokens".into())
        }
        Failure::BadAuth => (
            StatusCode::UNAUTHORIZED,
            "InvalidAuthenticationToken".into(),
            "Protocol 'Bearer' failed to validate because The token could not be read.".into(),
        ),
        Failure::Invalid(Invalid { message, code, .. }) => {
            (StatusCode::BAD_REQUEST, code.clone().unwrap_or_else(|| "BadRequest".into()), message.clone())
        }
        Failure::Rejected(addr) => (
            StatusCode::BAD_REQUEST,
            "ErrorInvalidRecipients".into(),
            format!(
                "At least one recipient isn't valid., Recipient '{addr}' isn't resolved. All recipients must be resolved before a message can be submitted."
            ),
        ),
        Failure::RateLimited => {
            (StatusCode::TOO_MANY_REQUESTS, "TooManyRequests".into(), "Please retry again later.".into())
        }
        Failure::ServerError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "ErrorInternalServerError".into(),
            "An internal server error occurred. The operation failed.".into(),
        ),
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut inner = json!({
        "date": crate::util::iso8601(crate::util::wall_clock()),
        "request-id": request_id,
        "client-request-id": client_request_id.unwrap_or(&request_id),
    });
    if matches!(f, Failure::RateLimited) {
        inner["code"] = json!("429");
        inner["message"] = json!("Please retry after");
        inner["status"] = json!("429");
    }
    let body = json!({ "error": { "code": code, "message": message, "innerError": inner } });
    let mut resp = (status, Json(body)).into_response();
    let h = resp.headers_mut();
    if matches!(f, Failure::RateLimited) {
        h.insert("retry-after", "10".parse().unwrap());
    }
    if status == StatusCode::UNAUTHORIZED {
        h.insert(
            "www-authenticate",
            r#"Bearer realm="", authorization_uri="https://login.microsoftonline.com/common/oauth2/authorize", client_id="00000003-0000-0000-c000-000000000000""#
                .parse()
                .unwrap(),
        );
    }
    resp
}
