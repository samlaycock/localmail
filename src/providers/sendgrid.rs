//! SendGrid's v3 Mail Send API: `POST /v3/mail/send`. Each personalization is its own message.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{attachment, bearer, header_map, keep, merged, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, ItemResult, submit};
use crate::state::AppState;

const ENDPOINT: &str = "POST /v3/mail/send";
/// Recipients (to, cc and bcc) per request.
const MAX_RECIPIENTS: usize = 1000;
const MAX_PERSONALIZATIONS: usize = 1000;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v3/mail/send", post(send))
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = read(&headers, &body);
    let sandbox =
        matches!(&request, Ok(items) if items.iter().flatten().any(|d| d.details.contains_key("sandbox_mode")));
    let outcome = submit(&state, Provider::Sendgrid, ENDPOINT, request, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let id = results.iter().find_map(|r| match r {
                ItemResult::Sent(s) => Some(s.message_id.clone()),
                _ => None,
            });
            // Sandbox mode answers 200 instead of 202.
            let status = if sandbox { StatusCode::OK } else { StatusCode::ACCEPTED };
            let mut resp = (status, [("content-type", "text/plain; charset=utf-8")]).into_response();
            if let Some(v) = id.and_then(|id| HeaderValue::from_str(&id).ok()) {
                resp.headers_mut().insert("x-message-id", v);
            }
            resp
        }
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn error(f: &Failure) -> Response {
    let body = |message: &str, field: Option<&str>| json!({ "errors": [{ "message": message, "field": field, "help": null }] });
    match f {
        Failure::MissingAuth => (StatusCode::UNAUTHORIZED, Json(body("authorization required", None))).into_response(),
        Failure::BadAuth => (
            StatusCode::UNAUTHORIZED,
            Json(body("The provided authorization grant is invalid, expired, or revoked", None)),
        )
            .into_response(),
        Failure::Invalid(i) => (
            i.status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(body(&i.message, i.field.as_deref())),
        )
            .into_response(),
        Failure::Rejected(addr) => (
            StatusCode::BAD_REQUEST,
            Json(body(&format!("The recipient {addr} was rejected."), Some("personalizations.0.to"))),
        )
            .into_response(),
        Failure::RateLimited => {
            let reset = (crate::util::wall_clock() + 60).to_string();
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("x-ratelimit-limit", "600"), ("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())],
                Json(json!({ "errors": [{ "field": null, "message": "too many requests" }] })),
            )
                .into_response()
        }
        Failure::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "errors": [{ "message": "internal server error" }] })))
                .into_response()
        }
    }
}

const MISSING_FROM: &str = "The from object must be provided for every email send. It is an object that requires the email parameter, but may also contain a name parameter.  e.g. {\"email\" : \"example@example.com\"}  or {\"email\" : \"example@example.com\", \"name\" : \"Example Recipient\"}.";
const INVALID_FROM: &str = "The from object must at least have an email parameter with a valid email address and may also contain a name parameter. e.g. {\"email\" : \"example@example.com\"}  or {\"email\" : \"example@example.com\", \"name\" : \"Example Recipient\"}.";
const MISSING_SUBJECT: &str = "The subject is required. You can get around this requirement if you use a template with a subject defined or if every personalization has a subject defined.";
const MISSING_CONTENT: &str =
    "The content param is required unless you are using a transactional template and have defined a template_ID.";

/// An `{email, name}` object.
fn email_object(v: &Value, field: &str) -> Result<Addr, Failure> {
    let addr = Addr::new(str_field(v, "name"), str_field(v, "email").unwrap_or_default());
    if addr.is_valid() {
        Ok(addr)
    } else {
        Err(Failure::field("Does not contain a valid address.", &format!("{field}.email")))
    }
}

fn email_list(v: Option<&Value>, field: &str) -> Result<Vec<Addr>, Failure> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(items)) => {
            items.iter().enumerate().map(|(i, item)| email_object(item, &format!("{field}.{i}"))).collect()
        }
        Some(_) => Err(Failure::field("Invalid type. Expected: array, given: object.", field)),
    }
}

fn read(headers: &HeaderMap, body: &[u8]) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    if bearer(headers).is_none() {
        return Err(Failure::MissingAuth);
    }
    let req: Value = serde_json::from_slice(body).map_err(|_| Failure::invalid("Bad Request"))?;
    if !req.is_object() {
        return Err(Failure::invalid("Bad Request"));
    }

    let from = match req.get("from") {
        Some(f) if f.is_object() => {
            let addr = Addr::new(str_field(f, "name"), str_field(f, "email").unwrap_or_default());
            if !addr.is_valid() {
                return Err(Failure::field(INVALID_FROM, "from"));
            }
            addr
        }
        _ => return Err(Failure::field(MISSING_FROM, "from")),
    };

    let personalizations = match req.get("personalizations") {
        Some(Value::Array(p)) if !p.is_empty() => p,
        _ => return Err(Failure::field("You must have at least one personalization.", "personalizations")),
    };
    if personalizations.len() > MAX_PERSONALIZATIONS {
        return Err(Failure::field(
            format!(
                "The personalization block is limited to {MAX_PERSONALIZATIONS} personalizations per API request. You have provided {} personalizations. Please consider splitting this into multiple requests and resending your request.",
                personalizations.len()
            ),
            "personalizations",
        ));
    }

    let template_id = str_field(&req, "template_id");
    let dynamic = template_id.is_some_and(|t| t.starts_with("d-"));
    let subject = str_field(&req, "subject");
    if subject.is_none() && template_id.is_none() && personalizations.iter().any(|p| str_field(p, "subject").is_none())
    {
        return Err(Failure::field(MISSING_SUBJECT, "subject"));
    }

    let (mut text, mut html, mut other_types) = (None, None, vec![]);
    match req.get("content") {
        Some(Value::Array(content)) if !content.is_empty() => {
            for (i, c) in content.iter().enumerate() {
                let value = str_field(c, "value").ok_or_else(|| {
                    Failure::field(
                        "The content value must be a string at least one character in length.",
                        &format!("content.{i}.value"),
                    )
                })?;
                match str_field(c, "type").unwrap_or_default().to_ascii_lowercase().as_str() {
                    "text/plain" => text = Some(value.to_string()),
                    "text/html" => html = Some(value.to_string()),
                    "" => {
                        return Err(Failure::field(
                            "The content type cannot contain ';', or CRLF characters.",
                            &format!("content.{i}.type"),
                        ));
                    }
                    other => other_types.push(other.to_string()),
                }
            }
        }
        _ if template_id.is_none() => return Err(Failure::field(MISSING_CONTENT, "content")),
        _ => {}
    }

    let mut reply_to = vec![];
    match (req.get("reply_to"), req.get("reply_to_list")) {
        (Some(_), Some(_)) => {
            return Err(Failure::field(
                "The reply_to and reply_to_list parameters cannot be used at the same time.",
                "reply_to_list",
            ));
        }
        (Some(r), None) => reply_to.push(email_object(r, "reply_to")?),
        (None, Some(list)) => reply_to = email_list(Some(list), "reply_to_list")?,
        (None, None) => {}
    }

    let mut attachments = vec![];
    if let Some(Value::Array(list)) = req.get("attachments") {
        for (i, a) in list.iter().enumerate() {
            let field = format!("attachments.{i}");
            let content = str_field(a, "content")
                .ok_or_else(|| Failure::field("The attachment content is required.", &format!("{field}.content")))?;
            let filename = str_field(a, "filename").ok_or_else(|| {
                Failure::field("The attachment filename parameter is required.", &format!("{field}.filename"))
            })?;
            let inline = str_field(a, "disposition") == Some("inline");
            let att = attachment(content, filename, str_field(a, "type"), str_field(a, "content_id"), inline)
                .ok_or_else(|| {
                    Failure::field("The attachment content must be base64 encoded.", &format!("{field}.content"))
                })?;
            attachments.push(att);
        }
    }

    let sandbox = match req.pointer("/mail_settings/sandbox_mode/enable") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(Failure::field(
                "The sandbox mode enable param should be a boolean value.",
                "mail_settings.sandbox_mode.enable",
            ));
        }
    };
    let categories: Vec<String> = req
        .get("categories")
        .and_then(Value::as_array)
        .map(|c| c.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    if categories.len() > 10 {
        return Err(Failure::field("Too many categories. You may only have 10 categories per request.", "categories"));
    }

    let mut drafts = vec![];
    let mut total = 0;
    let shown = crate::util::elide_long_strings(&req);
    for (i, p) in personalizations.iter().enumerate() {
        let field = format!("personalizations.{i}");
        let to = email_list(p.get("to"), &format!("{field}.to"))?;
        if to.is_empty() {
            return Err(Failure::field(
                "The to parameter is required for all personalization objects.",
                &format!("{field}.to"),
            ));
        }
        let cc = email_list(p.get("cc"), &format!("{field}.cc"))?;
        let bcc = email_list(p.get("bcc"), &format!("{field}.bcc"))?;
        total += to.len() + cc.len() + bcc.len();
        if total > MAX_RECIPIENTS {
            return Err(Failure::field(
                format!("There is a limit of {MAX_RECIPIENTS} total recipients (to + cc + bcc) per request."),
                "personalizations",
            ));
        }

        // Legacy substitutions replace their keys verbatim in the subject and content.
        let subs: Vec<(String, String)> = p
            .get("substitutions")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::util::value_to_string(v))).collect())
            .unwrap_or_default();
        let substitute = |s: &str| subs.iter().fold(s.to_string(), |acc, (k, v)| acc.replace(k.as_str(), v));

        let mut draft = Draft {
            to,
            cc,
            bcc,
            reply_to: reply_to.clone(),
            subject: substitute(str_field(p, "subject").or(subject).unwrap_or_default()),
            text: text.as_deref().map(substitute),
            html: html.as_deref().map(substitute),
            headers: [header_map(req.get("headers")), header_map(p.get("headers"))].concat(),
            attachments: attachments.clone(),
            tags: categories.clone(),
            request: Some(shown.clone()),
            ..Default::default()
        };
        draft.from = Some(match p.get("from").filter(|f| f.is_object()) {
            Some(f) => email_object(f, &format!("{field}.from"))?,
            None => from.clone(),
        });
        if let Some(id) = template_id {
            let data = if dynamic { p.get("dynamic_template_data").cloned() } else { p.get("substitutions").cloned() };
            if dynamic {
                // Dynamic templates replace the content.
                draft.text = None;
                draft.html = None;
            }
            if draft.text.is_none() && draft.html.is_none() {
                draft.template = Some(TemplateInfo {
                    field: "template_id".into(),
                    id: id.into(),
                    data: data.unwrap_or(Value::Null),
                    rendered: false,
                });
            } else {
                draft.details.insert("template_id".into(), id.into());
            }
        }
        if let Some(args) = merged(req.get("custom_args"), p.get("custom_args")) {
            draft.details.insert("custom_args".into(), args);
        }
        if let Some(send_at) = p.get("send_at").or(req.get("send_at")).filter(|v| !v.is_null()) {
            draft.details.insert("send_at".into(), send_at.clone());
        }
        keep(&mut draft, &req, &["batch_id", "asm", "ip_pool_name", "tracking_settings"]);
        if sandbox {
            draft.details.insert("sandbox_mode".into(), true.into());
        }
        if !other_types.is_empty() {
            draft.details.insert("other_content_types".into(), other_types.clone().into());
        }
        drafts.push(Ok(draft));
    }
    Ok(drafts)
}
