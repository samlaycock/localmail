//! Postmark's Email API: `POST /email`, `/email/batch`, `/email/withTemplate` and
//! `/email/batchWithTemplates`. Batches answer 200 with a result per message, so invalid and
//! rejected messages fail on their own.

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use super::{attachment, header, keep, str_field};
use crate::address::{Addr, parse_list};
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, Sent, submit};
use crate::state::AppState;

/// Recipients (To, Cc and Bcc) per message.
const MAX_RECIPIENTS: usize = 50;
const MAX_BATCH: usize = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/email", post(email))
        .route("/email/withTemplate", post(with_template))
        .route("/email/batch", post(batch))
        .route("/email/batchWithTemplates", post(batch_with_templates))
}

async fn email(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    single(state, headers, body, "POST /email", false).await
}

async fn with_template(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    single(state, headers, body, "POST /email/withTemplate", true).await
}

async fn batch(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    many(state, headers, body, "POST /email/batch", false).await
}

async fn batch_with_templates(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    many(state, headers, body, "POST /email/batchWithTemplates", true).await
}

async fn single(state: AppState, headers: HeaderMap, body: Bytes, endpoint: &str, template: bool) -> Response {
    let items = authorize(&headers).and_then(|()| parse(&body)).map(|v| vec![read(&v, template)]);
    let outcome = submit(&state, Provider::Postmark, endpoint, items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(sent)) => Json(success(sent)).into_response(),
            Some(ItemResult::Failed(f)) => error(f),
            None => error(&Failure::ServerError),
        },
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

async fn many(state: AppState, headers: HeaderMap, body: Bytes, endpoint: &str, template: bool) -> Response {
    let items = authorize(&headers).and_then(|()| parse(&body)).and_then(|v| {
        let list = if template { v.get("Messages").cloned().unwrap_or(Value::Null) } else { v };
        let Value::Array(list) = list else {
            return Err(coded(if template { 403 } else { 402 }, invalid_request_message(template)));
        };
        if list.len() > MAX_BATCH {
            return Err(coded(410, format!("You may only send up to {MAX_BATCH} messages in a batch.")));
        }
        Ok(list.iter().map(|m| read(m, template)).collect())
    });
    let outcome = submit(&state, Provider::Postmark, endpoint, items, true).await;
    let mut resp = match &outcome.result {
        Ok(results) => Json(
            results
                .iter()
                .map(|r| match r {
                    ItemResult::Sent(sent) => success(sent),
                    ItemResult::Failed(f) => {
                        let (_, code, message) = parts(f);
                        json!({ "ErrorCode": code, "Message": message })
                    }
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn invalid_request_message(template: bool) -> String {
    if template { "Invalid request field(s): 'Messages'.".into() } else { "Received invalid JSON input.".into() }
}

fn success(sent: &Sent) -> Value {
    json!({
        "To": sent.to.iter().map(Addr::display).collect::<Vec<_>>().join(", "),
        "SubmittedAt": submitted_at(),
        "MessageID": sent.message_id,
        "ErrorCode": 0,
        "Message": "OK",
    })
}

/// Like Postmark's `2014-02-17T07:25:01.4178645-05:00`, in UTC.
fn submitted_at() -> String {
    let ms = crate::util::now_ms();
    let base = crate::util::iso8601(ms / 1000);
    format!("{}.{:03}0000+00:00", base.trim_end_matches('Z'), ms % 1000)
}

fn coded(code: u32, message: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid { message: message.into(), code: Some(code.to_string()), ..Default::default() })
}

fn authorize(headers: &HeaderMap) -> Result<(), Failure> {
    header(headers, "x-postmark-server-token").map(|_| ()).ok_or(Failure::MissingAuth)
}

fn parse(body: &[u8]) -> Result<Value, Failure> {
    serde_json::from_slice(body).map_err(|_| coded(402, "Received invalid JSON input."))
}

/// HTTP status, Postmark ErrorCode and message.
fn parts(f: &Failure) -> (StatusCode, u32, String) {
    match f {
        Failure::MissingAuth => (
            StatusCode::UNAUTHORIZED,
            10,
            "No Account or Server API tokens were supplied in the HTTP headers. Please add a header for either X-Postmark-Server-Token or X-Postmark-Account-Token.".into(),
        ),
        Failure::BadAuth => (
            StatusCode::UNAUTHORIZED,
            10,
            "The Server Token you provided in the X-Postmark-Server-Token request header was invalid. Please verify that you are using a valid token.".into(),
        ),
        Failure::Invalid(i) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            i.code.as_deref().and_then(|c| c.parse().ok()).unwrap_or(300),
            i.message.clone(),
        ),
        Failure::Rejected(addr) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            406,
            format!(
                "You tried to send to a recipient that has been marked as inactive. Found inactive addresses: {addr}. Inactive recipients are ones that have generated a hard bounce, a spam complaint, or a manual suppression."
            ),
        ),
        // Postmark doesn't document these bodies; SDKs go by the HTTP status.
        Failure::RateLimited => (StatusCode::TOO_MANY_REQUESTS, 429, "Rate limit exceeded. Please slow down your requests.".into()),
        Failure::ServerError => (StatusCode::INTERNAL_SERVER_ERROR, 500, "Internal server error.".into()),
    }
}

fn error(f: &Failure) -> Response {
    let (status, code, message) = parts(f);
    (status, Json(json!({ "ErrorCode": code, "Message": message }))).into_response()
}

fn addresses(v: &Value, key: &str) -> Result<Vec<Addr>, Failure> {
    let list = str_field(v, key).map(parse_list).unwrap_or_default();
    match list.iter().find(|a| !a.is_valid()) {
        Some(bad) => Err(coded(300, format!("Invalid '{key}' address: '{}'.", bad.display()))),
        None => Ok(list),
    }
}

/// One message (or a batch item).
fn read(v: &Value, template: bool) -> Result<Draft, Failure> {
    if !v.is_object() {
        return Err(coded(402, "Received invalid JSON input."));
    }
    let from = match str_field(v, "From") {
        Some(f) => {
            let a = crate::address::parse_one(f);
            if !a.is_valid() {
                return Err(coded(300, format!("Invalid 'From' address: '{f}'.")));
            }
            a
        }
        None => return Err(coded(300, "Invalid 'From' address: ''.")),
    };
    let (to, cc, bcc) = (addresses(v, "To")?, addresses(v, "Cc")?, addresses(v, "Bcc")?);
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(coded(300, "Zero recipients specified"));
    }
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(coded(300, format!("Maximum of {MAX_RECIPIENTS} recipients allowed per message.")));
    }

    let mut draft = Draft {
        from: Some(from),
        to,
        cc,
        bcc,
        reply_to: str_field(v, "ReplyTo").map(parse_list).unwrap_or_default(),
        subject: str_field(v, "Subject").unwrap_or_default().to_string(),
        text: str_field(v, "TextBody").map(String::from),
        html: str_field(v, "HtmlBody").map(String::from),
        request: Some(crate::util::elide_long_strings(v)),
        ..Default::default()
    };
    if template {
        let (field, id) = match (v.get("TemplateId").filter(|t| !t.is_null()), str_field(v, "TemplateAlias")) {
            (Some(id), _) => ("TemplateId", crate::util::value_to_string(id)),
            (None, Some(alias)) => ("TemplateAlias", alias.to_string()),
            (None, None) => {
                return Err(coded(
                    1101,
                    "The 'TemplateId' or 'TemplateAlias' must be associated with an existing template.",
                ));
            }
        };
        let Some(model) = v.get("TemplateModel").filter(|m| m.is_object()) else {
            return Err(coded(1109, "The 'TemplateModel' is required and must be an object."));
        };
        draft.template = Some(TemplateInfo { field: field.into(), id, data: model.clone(), rendered: false });
        draft.subject = String::new();
        draft.text = None;
        draft.html = None;
        keep(&mut draft, v, &["InlineCss"]);
    } else if draft.text.is_none() && draft.html.is_none() {
        return Err(coded(300, "Provide either email TextBody or HtmlBody or both."));
    }

    if let Some(Value::Array(headers)) = v.get("Headers") {
        draft.headers = headers
            .iter()
            .filter_map(|h| Some((str_field(h, "Name")?.to_string(), crate::util::value_to_string(h.get("Value")?))))
            .collect();
    }
    if let Some(Value::Array(list)) = v.get("Attachments") {
        for a in list {
            let name = str_field(a, "Name").unwrap_or("attachment");
            let cid = str_field(a, "ContentID");
            let att = str_field(a, "Content")
                .and_then(|c| attachment(c, name, str_field(a, "ContentType"), cid, cid.is_some()))
                .ok_or_else(|| coded(300, format!("Attachment '{name}' has invalid or missing content.")))?;
            draft.attachments.push(att);
        }
    }
    if let Some(tag) = str_field(v, "Tag") {
        draft.tags.push(tag.to_string());
    }
    draft.details.insert("MessageStream".into(), str_field(v, "MessageStream").unwrap_or("outbound").into());
    keep(&mut draft, v, &["Metadata", "TrackOpens", "TrackLinks"]);
    Ok(draft)
}
