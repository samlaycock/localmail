//! Brevo's (formerly Sendinblue) transactional email API: `POST /v3/smtp/email`, authenticated
//! with an `api-key` header. With `messageVersions`, each version is a message of its own, with
//! its own recipients and `params`. `{{ params.x }}` is filled in, as Brevo's templating does.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{attachment, header, header_map, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const ENDPOINT: &str = "POST /v3/smtp/email";
const MAX_RECIPIENTS: usize = 99;
const MAX_VERSIONS: usize = 1000;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v3/smtp/email", post(send))
}

fn missing(message: impl Into<String>) -> Failure {
    Failure::coded(message, "missing_parameter")
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::coded(message, "invalid_parameter")
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let versions = serde_json::from_slice::<Value>(&body)
        .ok()
        .is_some_and(|v| v.get("messageVersions").is_some_and(|m| m.is_array()));
    let items = header(&headers, "api-key").ok_or(Failure::MissingAuth).and_then(|_| read(&body));
    let outcome = submit(&state, Provider::Brevo, ENDPOINT, items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let ids: Vec<&str> = results
                .iter()
                .filter_map(|r| match r {
                    ItemResult::Sent(s) => Some(s.message_id.as_str()),
                    _ => None,
                })
                .collect();
            let body = if versions { json!({ "messageIds": ids }) } else { json!({ "messageId": ids.first() }) };
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn error(f: &Failure) -> Response {
    let body = |code: &str, message: &str| Json(json!({ "code": code, "message": message }));
    match f {
        Failure::MissingAuth => {
            (StatusCode::UNAUTHORIZED, body("unauthorized", "authentication not found in headers")).into_response()
        }
        Failure::BadAuth => (StatusCode::UNAUTHORIZED, body("unauthorized", "Key not found")).into_response(),
        Failure::Invalid(Invalid { message, code, status, .. }) => (
            status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::BAD_REQUEST),
            body(code.as_deref().unwrap_or("bad_request"), message),
        )
            .into_response(),
        Failure::Rejected(addr) => {
            (StatusCode::BAD_REQUEST, body("invalid_parameter", &format!("{addr} is blocked"))).into_response()
        }
        Failure::RateLimited => {
            let reset = "60";
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("x-sib-ratelimit-limit", "1000"),
                    ("x-sib-ratelimit-remaining", "0"),
                    ("x-sib-ratelimit-reset", reset),
                ],
                body("too_many_requests", "The expected rate limit is exceeded."),
            )
                .into_response()
        }
        Failure::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, body("internal_error", "Internal server error")).into_response()
        }
    }
}

fn contact(v: &Value, field: &str) -> Result<Addr, Failure> {
    let email = str_field(v, "email").ok_or_else(|| missing(format!("email is missing in {field}")))?;
    let a = Addr::new(str_field(v, "name"), email);
    if !a.is_valid() {
        return Err(invalid(format!("email is not valid in {field}")));
    }
    Ok(a)
}

fn contacts(v: &Value, key: &str) -> Result<Vec<Addr>, Failure> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(list)) => list.iter().map(|c| contact(c, key)).collect(),
        Some(_) => Err(invalid(format!("{key} should be an array"))),
    }
}

fn read(body: &[u8]) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    let req: Value =
        serde_json::from_slice(body).map_err(|_| Failure::coded("Input must be a valid JSON object", "bad_request"))?;
    if !req.is_object() {
        return Err(Failure::coded("Input must be a valid JSON object", "bad_request"));
    }
    let template_id = req.get("templateId").filter(|v| !v.is_null());
    let from = match req.get("sender").filter(|v| !v.is_null()) {
        Some(s) if str_field(s, "email").is_some() => Some(contact(s, "sender")?),
        // A sender by id is one stored in the account.
        Some(s) if s.get("id").is_some() => None,
        Some(_) => return Err(missing("sender email is missing")),
        None if template_id.is_some() => None,
        None => return Err(missing("sender is missing")),
    };
    let html = str_field(&req, "htmlContent");
    let text = str_field(&req, "textContent");
    let versions = match req.get("messageVersions") {
        Some(Value::Array(v)) if !v.is_empty() => Some(v),
        Some(Value::Array(_)) => return Err(invalid("messageVersions should not be empty")),
        _ => None,
    };
    if versions.is_none() && contacts(&req, "to")?.is_empty() {
        return Err(missing("to is missing"));
    }
    if html.is_none() && text.is_none() && template_id.is_none() {
        return Err(missing("htmlContent or textContent or templateId is missing"));
    }
    if str_field(&req, "subject").is_none()
        && template_id.is_none()
        && versions.is_none_or(|v| v.iter().any(|m| str_field(m, "subject").is_none()))
    {
        return Err(missing("subject is missing"));
    }

    let mut base = Draft {
        from,
        to: contacts(&req, "to")?,
        cc: contacts(&req, "cc")?,
        bcc: contacts(&req, "bcc")?,
        reply_to: match req.get("replyTo").filter(|v| !v.is_null()) {
            Some(r) => vec![contact(r, "replyTo")?],
            None => vec![],
        },
        subject: str_field(&req, "subject").unwrap_or_default().to_string(),
        text: text.map(String::from),
        html: html.map(String::from),
        headers: header_map(req.get("headers")),
        request: Some(crate::util::elide_long_strings(&req)),
        ..Default::default()
    };
    if let Some(Value::Array(list)) = req.get("attachment") {
        for a in list {
            let name = str_field(a, "name").unwrap_or("attachment");
            let att = match (str_field(a, "content"), str_field(a, "url")) {
                (Some(content), _) => attachment(content, name, None, None, false)
                    .ok_or_else(|| invalid("attachment content should be base64 encoded"))?,
                (None, Some(url)) => crate::message::Attachment {
                    filename: name.into(),
                    content_type: "text/plain".into(),
                    content: format!("localmail didn't fetch this attachment. Its url was: {url}\n").into_bytes(),
                    content_id: None,
                    inline: false,
                },
                (None, None) => return Err(missing("attachment content or url is missing")),
            };
            base.attachments.push(att);
        }
    }
    if let Some(Value::Array(tags)) = req.get("tags") {
        base.tags = tags.iter().filter_map(Value::as_str).map(String::from).collect();
    }
    if let Some(id) = template_id {
        base.template = Some(TemplateInfo {
            field: "templateId".into(),
            id: crate::util::value_to_string(id),
            data: req.get("params").cloned().unwrap_or(Value::Null),
            rendered: false,
        });
    }
    super::keep(&mut base, &req, &["params", "scheduledAt", "batchId"]);

    let Some(versions) = versions else {
        check_count(&base)?;
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        return Ok(vec![Ok(fill(base, &params))]);
    };
    if versions.len() > MAX_VERSIONS {
        return Err(invalid(format!("messageVersions can't have more than {MAX_VERSIONS} versions")));
    }
    let mut out = vec![];
    for v in versions {
        let mut d = base.clone();
        d.to = contacts(v, "to")?;
        if d.to.is_empty() {
            return Err(missing("to is missing in messageVersions"));
        }
        if v.get("cc").is_some() {
            d.cc = contacts(v, "cc")?;
        }
        if v.get("bcc").is_some() {
            d.bcc = contacts(v, "bcc")?;
        }
        if let Some(r) = v.get("replyTo").filter(|r| !r.is_null()) {
            d.reply_to = vec![contact(r, "replyTo")?];
        }
        if let Some(s) = str_field(v, "subject") {
            d.subject = s.into();
        }
        if let Some(h) = str_field(v, "htmlContent") {
            d.html = Some(h.into());
        }
        if let Some(t) = str_field(v, "textContent") {
            d.text = Some(t.into());
        }
        check_count(&d)?;
        let params = super::merged(req.get("params"), v.get("params")).unwrap_or(Value::Null);
        if let Some(t) = &mut d.template {
            t.data = params.clone();
        }
        if !params.is_null() {
            d.details.insert("params".into(), params.clone());
        }
        d.request = Some(crate::util::elide_long_strings(v));
        out.push(Ok(fill(d, &params)));
    }
    Ok(out)
}

fn check_count(d: &Draft) -> Result<(), Failure> {
    if d.recipients().count() > MAX_RECIPIENTS {
        return Err(invalid(format!("The total number of recipients can't exceed {MAX_RECIPIENTS}")));
    }
    Ok(())
}

/// Fill in `{{ params.x }}` in the subject and bodies.
fn fill(mut d: Draft, params: &Value) -> Draft {
    if params.is_null() {
        return d;
    }
    let data = json!({ "params": params });
    super::fill(&mut d, |s| crate::util::render_placeholders(s, &data));
    d
}
