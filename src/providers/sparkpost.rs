//! SparkPost's Transmissions API: `POST /api/v1/transmissions`, authenticated with the API key
//! as the whole `Authorization` header. Each recipient gets a message of their own, with
//! `{{ substitution_data }}` filled in; all of a transmission's messages share its id. Invalid
//! recipients make the answer a 422 "transmission created, but with validation errors" listing
//! them in `rcpt_to_errors`; suppressed (rejected) ones are only counted.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{attachment, header, header_map, merged, str_field};
use crate::address::{Addr, parse_list, parse_one};
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const ENDPOINT: &str = "POST /api/v1/transmissions";

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/transmissions", post(send))
}

fn required(description: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid {
        message: "required field is missing".into(),
        field: Some(description.into()),
        code: Some("1400".into()),
        status: Some(422),
    })
}

fn bad(description: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid {
        message: "invalid data format/type".into(),
        field: Some(description.into()),
        code: Some("1300".into()),
        status: Some(422),
    })
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let items = header(&headers, "authorization").ok_or(Failure::MissingAuth).and_then(|_| read(&body));
    let outcome = submit(&state, Provider::Sparkpost, ENDPOINT, items, true).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let sent: Vec<&str> = results
                .iter()
                .filter_map(|r| match r {
                    ItemResult::Sent(s) => Some(s.message_id.as_str()),
                    _ => None,
                })
                .collect();
            let rejected = results.len() - sent.len();
            // Recipients that failed validation (suppressed ones are only counted).
            let rcpt_errors: Vec<Value> = results
                .iter()
                .filter_map(|r| match r {
                    ItemResult::Failed(Failure::Invalid(i)) => Some(json!({
                        "message": i.message, "description": i.field, "code": i.code,
                    })),
                    _ => None,
                })
                .collect();
            let counts = |id: Option<&&str>| {
                let mut r = json!({ "total_rejected_recipients": rejected, "total_accepted_recipients": sent.len() });
                if let Some(id) = id {
                    r["id"] = json!(id);
                }
                r
            };
            match sent.first() {
                Some(id) if rcpt_errors.is_empty() => Json(json!({ "results": counts(Some(id)) })).into_response(),
                Some(id) => {
                    let mut results = counts(Some(id));
                    results["rcpt_to_errors"] = Value::Array(rcpt_errors);
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({
                            "errors": [{ "message": "transmission created, but with validation errors", "code": "2000" }],
                            "results": results,
                        })),
                    )
                        .into_response()
                }
                None => {
                    let reason = results.iter().find_map(|r| match r {
                        ItemResult::Failed(f) => Some(f),
                        _ => None,
                    });
                    match reason {
                        Some(f @ (Failure::ServerError | Failure::Invalid(_))) => error(f),
                        _ => error(&Failure::Invalid(Invalid {
                            message: "Invalid recipient".into(),
                            field: Some("All provided recipients are suppressed".into()),
                            code: Some("5002".into()),
                            status: Some(422),
                        })),
                    }
                }
            }
        }
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn errors(status: StatusCode, message: &str, description: Option<&str>, code: Option<&str>) -> Response {
    let mut e = json!({ "message": message });
    if let Some(d) = description {
        e["description"] = json!(d);
    }
    if let Some(c) = code {
        e["code"] = json!(c);
    }
    (status, Json(json!({ "errors": [e] }))).into_response()
}

fn error(f: &Failure) -> Response {
    match f {
        Failure::MissingAuth | Failure::BadAuth => errors(StatusCode::UNAUTHORIZED, "Unauthorized.", None, None),
        Failure::Invalid(i) => errors(
            i.status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY),
            &i.message,
            i.field.as_deref(),
            i.code.as_deref(),
        ),
        Failure::Rejected(addr) => errors(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid recipient",
            Some(&format!("{addr} is on the suppression list")),
            Some("5002"),
        ),
        Failure::RateLimited => {
            let mut resp = errors(StatusCode::TOO_MANY_REQUESTS, "Too many requests", None, Some("1500"));
            resp.headers_mut().insert("retry-after", "60".parse().unwrap());
            resp
        }
        Failure::ServerError => errors(StatusCode::INTERNAL_SERVER_ERROR, "internal server error", None, Some("1500")),
    }
}

/// An address: a string, or `{email, name, header_to}`.
fn address(v: &Value) -> (Addr, Option<String>) {
    match v {
        Value::String(s) => (parse_one(s), None),
        Value::Object(_) => (
            Addr::new(str_field(v, "name"), str_field(v, "email").unwrap_or_default()),
            str_field(v, "header_to").map(String::from),
        ),
        _ => (Addr::new(None, ""), None),
    }
}

fn read(body: &[u8]) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    let req: Value = serde_json::from_slice(body).map_err(|_| {
        Failure::Invalid(Invalid {
            message: "Invalid JSON".into(),
            code: Some("1300".into()),
            status: Some(400),
            field: None,
        })
    })?;
    if !req.is_object() {
        return Err(bad("request body must be an object"));
    }
    let recipients = match req.get("recipients") {
        Some(Value::Array(r)) if !r.is_empty() => r,
        Some(Value::Object(r)) if r.contains_key("list_id") => {
            return Err(bad(
                "recipients.list_id: localmail can't send to a stored recipient list; list the recipients inline",
            ));
        }
        _ => return Err(required("recipients or list_id required")),
    };
    let content = match req.get("content") {
        Some(c) if c.is_object() => c,
        _ => return Err(required("content object or template_id required")),
    };
    let template_id = str_field(content, "template_id");
    let rfc822 = str_field(content, "email_rfc822");
    let from = match content.get("from") {
        Some(f) => {
            let (a, _) = address(f);
            if !a.is_valid() {
                return Err(bad("content.from.email must be a valid email address"));
            }
            Some(a)
        }
        None if template_id.is_some() || rfc822.is_some() => None,
        None => return Err(required("content.from is a required field")),
    };
    let (text, html, amp) = (str_field(content, "text"), str_field(content, "html"), str_field(content, "amp_html"));
    if template_id.is_none() && rfc822.is_none() {
        if str_field(content, "subject").is_none() {
            return Err(required("content.subject is a required field"));
        }
        if text.is_none() && html.is_none() && amp.is_none() {
            return Err(required("At least one of text or html needs to exist in content"));
        }
    }

    let mut base = Draft {
        from,
        reply_to: str_field(content, "reply_to").map(parse_list).unwrap_or_default(),
        subject: str_field(content, "subject").unwrap_or_default().to_string(),
        text: text.map(String::from),
        html: html.map(String::from),
        headers: header_map(content.get("headers"))
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("cc"))
            .collect(),
        request: Some(crate::util::elide_long_strings(&req)),
        provider_message_id: Some(Provider::Sparkpost.new_message_id()),
        ..Default::default()
    };
    if let Some(amp) = amp {
        base.details.insert("amp_html".into(), Value::String(amp.into()));
    }
    for (key, inline) in [("attachments", false), ("inline_images", true)] {
        let Some(Value::Array(list)) = content.get(key) else { continue };
        for a in list {
            let name = str_field(a, "name").unwrap_or("attachment");
            let data =
                str_field(a, "data").ok_or_else(|| required(format!("content.{key}.data is a required field")))?;
            // Inline images are referenced as `cid:<name>`.
            let att = attachment(data, name, str_field(a, "type"), inline.then_some(name), inline)
                .ok_or_else(|| bad(format!("content.{key}.data must be base64 encoded")))?;
            base.attachments.push(att);
        }
    }
    super::keep(&mut base, &req, &["options", "campaign_id", "description", "metadata", "return_path"]);
    let cc_header: Vec<String> = content
        .pointer("/headers/CC")
        .or_else(|| content.pointer("/headers/cc"))
        .and_then(Value::as_str)
        .map(|c| parse_list(c).into_iter().map(|a| a.key()).collect())
        .unwrap_or_default();

    Ok(recipients
        .iter()
        .map(|r| {
            let (addr, header_to) = address(r.get("address").unwrap_or(&Value::Null));
            if !addr.is_valid() {
                return Err(bad(format!("Invalid email address: {}", addr.email)));
            }
            let data = merged(req.get("substitution_data"), r.get("substitution_data")).unwrap_or(Value::Null);
            let mut d = base.clone();
            // `header_to` naming someone else makes this recipient a Cc (if the CC header lists
            // them) or a Bcc.
            match header_to.filter(|h| !parse_list(h).iter().any(|a| a.key() == addr.key())) {
                Some(h) if cc_header.contains(&addr.key()) => {
                    d.cc = vec![addr];
                    d.details.insert("header_to".into(), Value::String(h));
                }
                Some(h) => {
                    d.bcc = vec![addr];
                    d.details.insert("header_to".into(), Value::String(h));
                }
                None => d.to = vec![addr],
            }
            if let Some(raw) = rfc822 {
                let mut m = Draft::from_raw(raw.as_bytes().to_vec(), Some(d.recipients().cloned().collect()));
                m.details = d.details.clone();
                m.request = d.request.clone();
                m.provider_message_id = d.provider_message_id.clone();
                d = m;
            }
            if let Some(id) = template_id {
                d.template = Some(TemplateInfo {
                    field: "template_id".into(),
                    id: id.into(),
                    data: data.clone(),
                    rendered: false,
                });
            } else if !data.is_null() {
                super::fill(&mut d, |s| crate::util::render_placeholders(s, &data));
            }
            if !data.is_null() {
                d.details.insert("substitution_data".into(), data);
            }
            for key in ["metadata", "tags"] {
                if let Some(v) = r.get(key).filter(|v| !v.is_null()) {
                    if key == "tags" {
                        d.tags =
                            v.as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
                    } else {
                        let m = merged(req.get("metadata"), Some(v)).unwrap_or(Value::Null);
                        d.details.insert("metadata".into(), m);
                    }
                }
            }
            Ok(d)
        })
        .collect())
}
