//! Resend's API: `POST /emails` and `POST /emails/batch`. Batches are all-or-nothing unless
//! sent with `x-batch-validation: permissive`.

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use super::{attachment, bearer, header, header_map, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, TemplateInfo, addrs_from_value};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const MAX_TO: usize = 50;
const MAX_BATCH: usize = 100;
const ADDRESS_FORMAT: &str =
    "The email address needs to follow the `email@example.com` or `Name <email@example.com>` format.";

pub fn routes() -> Router<AppState> {
    Router::new().route("/emails", post(send)).route("/emails/batch", post(batch))
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let items = authorize(&headers).and_then(|()| parse(&body)).map(|v| vec![read(&v, false)]);
    let outcome = submit(&state, Provider::Resend, "POST /emails", items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(sent)) => Json(json!({ "id": sent.message_id })).into_response(),
            Some(ItemResult::Failed(f)) => error(f),
            None => error(&Failure::ServerError),
        },
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

async fn batch(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let permissive = header(&headers, "x-batch-validation").is_some_and(|v| v.eq_ignore_ascii_case("permissive"));
    let items = authorize(&headers).and_then(|()| parse(&body)).and_then(|v| {
        let Value::Array(list) = v else {
            return Err(invalid(422, "validation_error", "The request body must be an array of emails."));
        };
        if list.is_empty() || list.len() > MAX_BATCH {
            return Err(invalid(
                422,
                "validation_error",
                format!("A batch must contain between 1 and {MAX_BATCH} emails."),
            ));
        }
        Ok(list.iter().map(|m| read(m, true)).collect())
    });
    let outcome = submit(&state, Provider::Resend, "POST /emails/batch", items, permissive).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let data: Vec<Value> = results
                .iter()
                .filter_map(|r| match r {
                    ItemResult::Sent(s) => Some(json!({ "id": s.message_id })),
                    _ => None,
                })
                .collect();
            let mut body = json!({ "data": data });
            if permissive {
                body["errors"] = results
                    .iter()
                    .enumerate()
                    .filter_map(|(index, r)| match r {
                        ItemResult::Failed(f) => Some(json!({ "index": index, "message": parts(f).2 })),
                        _ => None,
                    })
                    .collect();
            }
            Json(body).into_response()
        }
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn invalid(status: u16, name: &str, message: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid { message: message.into(), code: Some(name.into()), status: Some(status), field: None })
}

fn authorize(headers: &HeaderMap) -> Result<(), Failure> {
    bearer(headers).map(|_| ()).ok_or(Failure::MissingAuth)
}

fn parse(body: &[u8]) -> Result<Value, Failure> {
    serde_json::from_slice(body).map_err(|_| invalid(400, "validation_error", "Invalid JSON in the request body."))
}

/// HTTP status, error name and message.
fn parts(f: &Failure) -> (StatusCode, String, String) {
    match f {
        Failure::MissingAuth => (StatusCode::UNAUTHORIZED, "missing_api_key".into(), "Missing API key in the authorization header. Include the following header 'Authorization: Bearer YOUR_API_KEY' in the request.".into()),
        Failure::BadAuth => (StatusCode::UNAUTHORIZED, "validation_error".into(), "API key is invalid".into()),
        Failure::Invalid(i) => (
            i.status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY),
            i.code.clone().unwrap_or_else(|| "validation_error".into()),
            i.message.clone(),
        ),
        Failure::Rejected(addr) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error".into(),
            format!("The recipient `{addr}` was rejected."),
        ),
        Failure::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded".into(),
            "Too many requests. Please limit the number of requests per second. Or contact support to increase rate limit.".into(),
        ),
        Failure::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, "application_error".into(), "An unexpected error occurred.".into())
        }
    }
}

fn error(f: &Failure) -> Response {
    let (status, name, message) = parts(f);
    let body = Json(json!({ "statusCode": status.as_u16(), "message": message, "name": name }));
    if matches!(f, Failure::RateLimited) {
        let headers =
            [("ratelimit-limit", "10"), ("ratelimit-remaining", "0"), ("ratelimit-reset", "1"), ("retry-after", "1")];
        return (status, headers, body).into_response();
    }
    (status, body).into_response()
}

fn addresses(v: &Value, key: &str) -> Result<Vec<Addr>, Failure> {
    let list = addrs_from_value(v.get(key));
    if list.iter().any(|a| !a.is_valid()) {
        return Err(invalid(422, "validation_error", format!("Invalid `{key}` field. {ADDRESS_FORMAT}")));
    }
    Ok(list)
}

fn read(v: &Value, in_batch: bool) -> Result<Draft, Failure> {
    if !v.is_object() {
        return Err(invalid(422, "validation_error", "Each email must be an object."));
    }
    let missing = |field: &str| invalid(422, "missing_required_field", format!("Missing `{field}` field."));
    let from = match str_field(v, "from") {
        Some(f) => {
            let a = crate::address::parse_one(f);
            if !a.is_valid() {
                return Err(invalid(422, "validation_error", format!("Invalid `from` field. {ADDRESS_FORMAT}")));
            }
            a
        }
        None => return Err(missing("from")),
    };
    let to = addresses(v, "to")?;
    if to.is_empty() {
        return Err(missing("to"));
    }
    if to.len() > MAX_TO {
        return Err(invalid(
            422,
            "validation_error",
            format!("Too many recipients in the `to` field. The maximum is {MAX_TO}."),
        ));
    }
    let template = v.get("template").filter(|t| !t.is_null());
    let subject = str_field(v, "subject");
    if subject.is_none() && template.is_none() {
        return Err(missing("subject"));
    }
    let (text, html) = (str_field(v, "text"), str_field(v, "html"));

    let mut draft = Draft {
        from: Some(from),
        to,
        cc: addresses(v, "cc")?,
        bcc: addresses(v, "bcc")?,
        reply_to: addresses(v, "reply_to")?,
        subject: subject.unwrap_or_default().to_string(),
        text: text.map(String::from),
        html: html.map(String::from),
        headers: header_map(v.get("headers")),
        request: Some(crate::util::elide_long_strings(v)),
        ..Default::default()
    };

    if let Some(t) = template {
        if text.is_some() || html.is_some() {
            return Err(invalid(
                422,
                "validation_error",
                "The `template` field can't be used together with `html` or `text`.",
            ));
        }
        let Some(id) = str_field(t, "id") else { return Err(missing("template.id")) };
        draft.template = Some(TemplateInfo {
            field: "template.id".into(),
            id: id.into(),
            data: t.get("variables").cloned().unwrap_or(Value::Null),
            rendered: false,
        });
    } else if text.is_none() && html.is_none() {
        return Err(missing("html` or `text"));
    }

    if let Some(Value::Array(list)) = v.get("attachments") {
        if in_batch && !list.is_empty() {
            return Err(invalid(422, "validation_error", "Attachments are not supported in batch emails."));
        }
        for a in list {
            let filename = str_field(a, "filename").unwrap_or("attachment");
            let (content_type, cid) = (str_field(a, "content_type"), str_field(a, "content_id"));
            let att = match (str_field(a, "content"), str_field(a, "path")) {
                (Some(content), _) => attachment(content, filename, content_type, cid, cid.is_some())
                    .ok_or_else(|| invalid(422, "invalid_attachment", "Attachment content must be base64 encoded."))?,
                // A remote file: localmail is offline, so it keeps a note instead.
                (None, Some(path)) => crate::message::Attachment {
                    filename: filename.into(),
                    content_type: "text/plain".into(),
                    content: format!("localmail didn't fetch this attachment. Its path was: {path}\n").into_bytes(),
                    content_id: None,
                    inline: false,
                },
                (None, None) => {
                    return Err(invalid(
                        422,
                        "invalid_attachment",
                        "Attachment must have either a `content` or `path`.",
                    ));
                }
            };
            draft.attachments.push(att);
        }
    }
    if let Some(Value::Array(tags)) = v.get("tags") {
        for t in tags {
            let (Some(name), value) = (str_field(t, "name"), str_field(t, "value")) else { continue };
            let ok = |s: &str| s.len() <= 256 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !ok(name) || !value.is_none_or(ok) {
                return Err(invalid(
                    422,
                    "validation_error",
                    "Tags should only contain ASCII letters (a–z, A–Z), numbers (0–9), underscores (_), or dashes (-).",
                ));
            }
            draft.tags.push(match value {
                Some(value) => format!("{name}={value}"),
                None => name.to_string(),
            });
        }
    }
    super::keep(&mut draft, v, &["scheduled_at", "topic_id"]);
    Ok(draft)
}
