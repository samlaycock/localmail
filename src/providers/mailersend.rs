//! MailerSend's email API: `POST /v1/email` (one email, answered `202` with an `X-Message-Id`
//! header) and `POST /v1/bulk-email` (up to 500, answered with a `bulk_email_id`; as with
//! MailerSend, invalid emails of a bulk request don't fail the request). `personalization` data
//! fills in `{{ var }}`; with several `to` recipients, each gets a copy with their own data.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{attachment, bearer, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const MAX_TO: usize = 50;
const MAX_CC: usize = 10;
const MAX_BULK: usize = 500;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/email", post(send)).route("/v1/bulk-email", post(bulk))
}

/// A Laravel-style validation error for one field.
fn field(key: &str, message: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid { message: message.into(), field: Some(key.into()), code: None, status: Some(422) })
}

fn parse(body: &[u8]) -> Result<Value, Failure> {
    serde_json::from_slice(body).map_err(|_| {
        Failure::Invalid(Invalid {
            message: "The request body must be valid JSON.".into(),
            status: Some(400),
            ..Default::default()
        })
    })
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let items = bearer(&headers).ok_or(Failure::MissingAuth).and_then(|_| parse(&body)).and_then(|v| read(&v, ""));
    let outcome = submit(&state, Provider::Mailersend, "POST /v1/email", items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let mut resp = StatusCode::ACCEPTED.into_response();
            let id = results.iter().find_map(|r| match r {
                ItemResult::Sent(s) => Some(s.message_id.clone()),
                _ => None,
            });
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

async fn bulk(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let items = bearer(&headers).ok_or(Failure::MissingAuth).and_then(|_| parse(&body)).and_then(|v| {
        let Value::Array(list) = v else {
            return Err(field("0", "The request body must be an array of emails."));
        };
        if list.is_empty() || list.len() > MAX_BULK {
            return Err(field("0", format!("A bulk request must contain between 1 and {MAX_BULK} emails.")));
        }
        let mut out = vec![];
        for (i, email) in list.iter().enumerate() {
            match read(email, &format!("{i}.")) {
                Ok(drafts) => out.extend(drafts),
                Err(f) => out.push(Err(f)),
            }
        }
        Ok(out)
    });
    let outcome = submit(&state, Provider::Mailersend, "POST /v1/bulk-email", items, true).await;
    let mut resp = match &outcome.result {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(json!({ "message": "The bulk email is being processed.", "bulk_email_id": Provider::Mailersend.new_message_id() })),
        )
            .into_response(),
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn error(f: &Failure) -> Response {
    let message = |status: StatusCode, m: &str| (status, Json(json!({ "message": m }))).into_response();
    match f {
        Failure::MissingAuth | Failure::BadAuth => message(StatusCode::UNAUTHORIZED, "Unauthenticated."),
        Failure::Invalid(i) => {
            let status =
                i.status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
            match &i.field {
                Some(key) => {
                    (status, Json(json!({ "message": "The given data was invalid.", "errors": { key: [i.message] } })))
                        .into_response()
                }
                None => message(status, &i.message),
            }
        }
        // Suppressed recipients aren't an error: a 202 with a warning, and no X-Message-Id when
        // none was sent.
        Failure::Rejected(addr) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "message": "There are some warnings for your request.",
                "warnings": [{
                    "type": "ALL_SUPPRESSED",
                    "warning": "None of the recipients were sent this email because they are all suppressed.",
                    "recipients": [{ "email": addr, "name": null, "reasons": ["hard_bounced"] }],
                }],
            })),
        )
            .into_response(),
        Failure::RateLimited => {
            let reset = (crate::util::wall_clock() + 59).to_string();
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("x-ratelimit-limit", "60"),
                    ("x-ratelimit-remaining", "0"),
                    ("retry-after", "59"),
                    ("x-ratelimit-reset", reset.as_str()),
                ],
                Json(json!({ "message": "Your account reached its rate limit of 120 requests/min. Please wait before trying again. #MS42903" })),
            )
                .into_response()
        }
        Failure::ServerError => message(StatusCode::INTERNAL_SERVER_ERROR, "Server Error"),
    }
}

fn contact(v: &Value, key: &str) -> Result<Addr, Failure> {
    let email = str_field(v, "email")
        .ok_or_else(|| field(&format!("{key}.email"), format!("The {key}.email field is required.")))?;
    let a = Addr::new(str_field(v, "name"), email);
    if !a.is_valid() {
        return Err(field(&format!("{key}.email"), format!("The {key}.email must be a valid email address.")));
    }
    Ok(a)
}

fn contacts(v: &Value, key: &str, prefix: &str, max: usize) -> Result<Vec<Addr>, Failure> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(list)) => {
            if list.len() > max {
                return Err(field(
                    &format!("{prefix}{key}"),
                    format!("The {prefix}{key} may not have more than {max} items."),
                ));
            }
            list.iter().enumerate().map(|(i, c)| contact(c, &format!("{prefix}{key}.{i}"))).collect()
        }
        Some(_) => Err(field(&format!("{prefix}{key}"), format!("The {prefix}{key} must be an array."))),
    }
}

/// One email object, as one draft, or one per `to` recipient when they have personalization.
fn read(v: &Value, prefix: &str) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    if !v.is_object() {
        return Err(field(prefix.trim_end_matches('.'), "Each email must be an object."));
    }
    let from = match v.get("from").filter(|f| !f.is_null()) {
        Some(f) => contact(f, &format!("{prefix}from"))?,
        None => {
            return Err(field(&format!("{prefix}from.email"), format!("The {prefix}from.email field is required.")));
        }
    };
    let to = contacts(v, "to", prefix, MAX_TO)?;
    if to.is_empty() {
        return Err(field(&format!("{prefix}to"), format!("The {prefix}to field is required.")));
    }
    let template_id = str_field(v, "template_id");
    let subject = str_field(v, "subject");
    if subject.is_none() && template_id.is_none() {
        return Err(field(
            &format!("{prefix}subject"),
            format!("The {prefix}subject field is required when template id is not present."),
        ));
    }
    let (text, html) = (str_field(v, "text"), str_field(v, "html"));
    if text.is_none() && html.is_none() && template_id.is_none() {
        return Err(field(
            &format!("{prefix}html"),
            format!("The {prefix}html field is required when none of text / template id are present."),
        ));
    }
    let mut base = Draft {
        from: Some(from),
        cc: contacts(v, "cc", prefix, MAX_CC)?,
        bcc: contacts(v, "bcc", prefix, MAX_CC)?,
        reply_to: match v.get("reply_to").filter(|r| !r.is_null()) {
            Some(r) => vec![contact(r, &format!("{prefix}reply_to"))?],
            None => vec![],
        },
        subject: subject.unwrap_or_default().to_string(),
        text: text.map(String::from),
        html: html.map(String::from),
        request: Some(crate::util::elide_long_strings(v)),
        ..Default::default()
    };
    for h in v.get("headers").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(name), Some(value)) = (str_field(h, "name"), str_field(h, "value")) {
            base.headers.push((name.into(), value.into()));
        }
    }
    if let Some(r) = str_field(v, "in_reply_to") {
        base.headers.push(("In-Reply-To".into(), r.into()));
    }
    for (i, a) in v.get("attachments").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let key = format!("{prefix}attachments.{i}");
        let filename = str_field(a, "filename")
            .ok_or_else(|| field(&format!("{key}.filename"), format!("The {key}.filename field is required.")))?;
        let content = str_field(a, "content")
            .ok_or_else(|| field(&format!("{key}.content"), format!("The {key}.content field is required.")))?;
        let inline = str_field(a, "disposition").is_some_and(|d| d.eq_ignore_ascii_case("inline"));
        let att = attachment(content, filename, None, str_field(a, "id"), inline)
            .ok_or_else(|| field(&format!("{key}.content"), format!("The {key}.content must be base64 encoded.")))?;
        base.attachments.push(att);
    }
    base.tags = v
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    super::keep(&mut base, v, &["send_at", "precedence_bulk", "settings", "list_unsubscribe"]);

    let personalization = |email: &str| -> Value {
        v.get("personalization")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|p| str_field(p, "email").is_some_and(|e| e.eq_ignore_ascii_case(email)))
            .and_then(|p| p.get("data").cloned())
            .unwrap_or(Value::Null)
    };
    let build = |mut d: Draft, data: Value| -> Draft {
        if let Some(id) = template_id {
            d.template =
                Some(TemplateInfo { field: "template_id".into(), id: id.into(), data: data.clone(), rendered: false });
        } else if !data.is_null() {
            super::fill(&mut d, |s| crate::util::render_placeholders(s, &data));
        }
        if !data.is_null() {
            d.details.insert("personalization".into(), data);
        }
        d
    };
    let personalized = to.iter().any(|a| !personalization(&a.email).is_null());
    if to.len() == 1 || !personalized {
        let data = to.first().map(|a| personalization(&a.email)).unwrap_or(Value::Null);
        return Ok(vec![Ok(build(Draft { to, ..base }, data))]);
    }
    Ok(to
        .into_iter()
        .map(|a| {
            let data = personalization(&a.email);
            Ok(build(Draft { to: vec![a], ..base.clone() }, data))
        })
        .collect())
}
