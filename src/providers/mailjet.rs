//! Mailjet's Send API v3.1: `POST /v3.1/send` with up to 50 `Messages`, each its own message, and
//! `Globals` merged into every one. Valid messages are sent even when others in the request
//! aren't; the answer lists each message's status, with errors for the ones that failed.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::{attachment, basic, header_map, str_field};
use crate::address::Addr;
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const ENDPOINT: &str = "POST /v3.1/send";
const MAX_MESSAGES: usize = 50;
const MAX_RECIPIENTS: usize = 50;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v3.1/send", post(send))
}

fn mj(code: &str, message: impl Into<String>, related: &[&str]) -> Failure {
    Failure::Invalid(Invalid {
        message: message.into(),
        code: Some(code.into()),
        field: (!related.is_empty()).then(|| related.join(",")),
        status: None,
    })
}

fn malformed() -> Failure {
    mj("mj-0002", "Malformed JSON, please review the syntax and properties types.", &[])
}

async fn send(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = basic(&headers).ok_or(Failure::MissingAuth).and_then(|_| read(&body));
    // Each valid message's recipients and CustomID, in order, for the answer.
    let shown: Vec<Option<(Recipients, Value)>> = match &request {
        Ok(items) => items
            .iter()
            .map(|i| {
                i.as_ref().ok().map(|d| (Recipients::of(d), d.details.get("CustomID").cloned().unwrap_or(json!(""))))
            })
            .collect(),
        Err(_) => vec![],
    };
    // Valid messages are sent even when others in the request aren't.
    let outcome = submit(&state, Provider::Mailjet, ENDPOINT, request, true).await;
    let mut resp = match &outcome.result {
        Ok(results) => {
            let mut failed = false;
            let messages: Vec<Value> = results
                .iter()
                .zip(&shown)
                .map(|(r, shown)| match (r, shown) {
                    (ItemResult::Sent(_), Some((rcpts, custom_id))) => rcpts.success(custom_id),
                    (ItemResult::Failed(f), _) => {
                        failed = true;
                        message_error(f)
                    }
                    (ItemResult::Sent(_), None) => message_error(&Failure::ServerError),
                })
                .collect();
            let status = if failed { StatusCode::BAD_REQUEST } else { StatusCode::OK };
            (status, Json(json!({ "Messages": messages }))).into_response()
        }
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

/// A message's recipients, to list in the answer.
struct Recipients {
    to: Vec<Addr>,
    cc: Vec<Addr>,
    bcc: Vec<Addr>,
    /// A MessageUUID for each recipient (To, Cc, Bcc); the first is the stored message's id.
    uuids: Vec<String>,
    /// Sandbox mode validates but sends nothing, so it answers without ids.
    sandbox: bool,
}

impl Recipients {
    fn of(d: &Draft) -> Recipients {
        let n = d.recipients().count();
        let uuids = d.provider_message_id.clone().into_iter().chain((1..n).map(|_| uuid::Uuid::new_v4().to_string()));
        Recipients {
            to: d.to.clone(),
            cc: d.cc.clone(),
            bcc: d.bcc.clone(),
            uuids: uuids.collect(),
            sandbox: d.details.contains_key("SandboxMode"),
        }
    }

    fn success(&self, custom_id: &Value) -> Value {
        let mut uuids = self.uuids.iter();
        let mut list = |addrs: &[Addr]| -> Vec<Value> {
            addrs
                .iter()
                .map(|a| {
                    if self.sandbox {
                        return json!({ "Email": a.email });
                    }
                    // A 17-digit MessageID.
                    let id = format!("2{}", crate::util::random_digits(16));
                    json!({
                        "Email": a.email,
                        "MessageUUID": uuids.next().cloned().unwrap_or_default(),
                        "MessageID": id.parse::<u64>().unwrap_or_default(),
                        "MessageHref": format!("https://api.mailjet.com/v3/message/{id}"),
                    })
                })
                .collect()
        };
        json!({
            "Status": "success",
            "CustomID": custom_id,
            "To": list(&self.to),
            "Cc": list(&self.cc),
            "Bcc": list(&self.bcc),
        })
    }
}

fn error_item(f: &Failure) -> Value {
    let (code, message, related, status) = match f {
        Failure::Invalid(i) => (
            i.code.clone().unwrap_or_else(|| "mj-0003".into()),
            i.message.clone(),
            i.field.as_deref().map(|f| f.split(',').map(String::from).collect()).unwrap_or_else(Vec::new),
            i.status.unwrap_or(400),
        ),
        Failure::Rejected(addr) => {
            ("mj-0013".into(), format!("\"{addr}\" is an invalid email address."), vec!["To[0].Email".into()], 400)
        }
        _ => ("mj-0000".into(), "Internal Server Error".into(), vec![], 500),
    };
    json!({
        "ErrorIdentifier": uuid::Uuid::new_v4().to_string(),
        "ErrorCode": code,
        "StatusCode": status,
        "ErrorMessage": message,
        "ErrorRelatedTo": related,
    })
}

fn message_error(f: &Failure) -> Value {
    json!({ "Status": "error", "Errors": [error_item(f)] })
}

/// An error for the whole request: `{ErrorIdentifier, StatusCode, ErrorMessage}`.
fn top_level(status: StatusCode, message: &str, code: Option<&str>) -> Response {
    let mut body = json!({
        "ErrorIdentifier": uuid::Uuid::new_v4().to_string(),
        "StatusCode": status.as_u16(),
        "ErrorMessage": message,
    });
    if let Some(c) = code {
        body["ErrorCode"] = json!(c);
    }
    (status, Json(body)).into_response()
}

fn error(f: &Failure) -> Response {
    const AUTH: &str = "API key authentication/authorization failure. You may be unauthorized to access the API or your API key may be expired. Visit API keys management section to check your keys.";
    match f {
        // Without credentials, Mailjet answers a bare `Not authorized`.
        Failure::MissingAuth => {
            (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], "Not authorized").into_response()
        }
        Failure::BadAuth => top_level(StatusCode::UNAUTHORIZED, AUTH, None),
        Failure::Invalid(i) if i.code.as_deref() == Some("mj-0002") => {
            top_level(StatusCode::BAD_REQUEST, &i.message, Some("mj-0002"))
        }
        Failure::Invalid(_) | Failure::Rejected(_) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "Messages": [message_error(f)] }))).into_response()
        }
        Failure::RateLimited => {
            let mut resp = top_level(StatusCode::TOO_MANY_REQUESTS, "Too many requests", None);
            resp.headers_mut().insert("retry-after", "60".parse().unwrap());
            resp
        }
        Failure::ServerError => top_level(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error", None),
    }
}

fn read(body: &[u8]) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    let req: Value = serde_json::from_slice(body).map_err(|_| malformed())?;
    if !req.is_object() {
        return Err(malformed());
    }
    let messages = match req.get("Messages") {
        Some(Value::Array(m)) if !m.is_empty() => m,
        Some(Value::Array(_)) => return Err(mj("mj-0007", "Array must contain at least one item.", &["Messages"])),
        None => return Err(mj("mj-0003", "Missing mandatory property.", &["Messages"])),
        Some(_) => return Err(mj("mj-0004", "Type mismatch. Expected type \"array of messages\".", &["Messages"])),
    };
    if messages.len() > MAX_MESSAGES {
        return Err(mj("mj-0008", format!("Array has more than {MAX_MESSAGES} items."), &["Messages"]));
    }
    let sandbox = req.get("SandboxMode").and_then(Value::as_bool).unwrap_or(false);
    let globals = req.get("Globals").filter(|g| g.is_object());
    Ok(messages
        .iter()
        .map(|m| {
            let merged = match (globals, m) {
                (Some(Value::Object(g)), Value::Object(m)) => {
                    let mut out: Map<String, Value> = g.clone();
                    out.extend(m.clone());
                    Value::Object(out)
                }
                _ => m.clone(),
            };
            let mut d = read_message(&merged)?;
            d.request = Some(crate::util::elide_long_strings(m));
            if sandbox {
                d.details.insert("SandboxMode".into(), Value::Bool(true));
            }
            d.provider_message_id = Some(uuid::Uuid::new_v4().to_string());
            Ok(d)
        })
        .collect())
}

fn contact(v: &Value, field: &str) -> Result<Addr, Failure> {
    let email = str_field(v, "Email")
        .ok_or_else(|| mj("mj-0003", "Missing mandatory property.", &[&format!("{field}.Email")]))?;
    let a = Addr::new(str_field(v, "Name"), email);
    if !a.is_valid() {
        return Err(mj("mj-0013", format!("\"{email}\" is an invalid email address."), &[&format!("{field}.Email")]));
    }
    Ok(a)
}

fn contacts(m: &Value, key: &str) -> Result<Vec<Addr>, Failure> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(list)) => list.iter().enumerate().map(|(i, c)| contact(c, &format!("{key}[{i}]"))).collect(),
        Some(_) => Err(mj("mj-0004", "Type mismatch. Expected type \"array of emails\".", &[key])),
    }
}

/// Fill in `{{var:name}}` and `{{var:name:"default"}}` (Mailjet's template language).
fn render(s: &str, vars: &Value) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"\{\{\s*var:([\w.-]+)(?::"([^"]*)")?\s*\}\}"#).unwrap());
    re.replace_all(s, |c: &regex::Captures| match vars.get(&c[1]) {
        Some(v) => crate::util::value_to_string(v),
        None => c.get(2).map(|d| d.as_str().to_string()).unwrap_or_else(|| c[0].to_string()),
    })
    .into_owned()
}

fn read_message(m: &Value) -> Result<Draft, Failure> {
    if !m.is_object() {
        return Err(mj("mj-0004", "Type mismatch. Expected type \"message\".", &[]));
    }
    let from = match m.get("From").filter(|v| !v.is_null()) {
        Some(f) => contact(f, "From")?,
        None => return Err(mj("mj-0003", "Missing mandatory property.", &["From"])),
    };
    let to = contacts(m, "To")?;
    if to.is_empty() {
        return Err(mj("mj-0003", "Missing mandatory property.", &["To"]));
    }
    let (cc, bcc) = (contacts(m, "Cc")?, contacts(m, "Bcc")?);
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(mj("mj-0008", format!("Array has more than {MAX_RECIPIENTS} items."), &["To"]));
    }
    let reply_to = match m.get("ReplyTo").filter(|v| !v.is_null()) {
        Some(r) => vec![contact(r, "ReplyTo")?],
        None => vec![],
    };
    let template_id = m.get("TemplateID").filter(|v| !v.is_null() && *v != &json!(0));
    let (text, html) = (str_field(m, "TextPart"), str_field(m, "HTMLPart"));
    if text.is_none() && html.is_none() && template_id.is_none() {
        return Err(mj(
            "send-0003",
            "At least \"HTMLPart\", \"TextPart\" or \"TemplateID\" must be provided.",
            &["HTMLPart", "TextPart"],
        ));
    }
    let vars = m.get("Variables").cloned().unwrap_or(Value::Null);
    let templating = m.get("TemplateLanguage").and_then(Value::as_bool).unwrap_or(false);
    let fill = |s: &str| if templating { render(s, &vars) } else { s.to_string() };

    let mut d = Draft {
        from: Some(from),
        to,
        cc,
        bcc,
        reply_to,
        subject: fill(str_field(m, "Subject").unwrap_or_default()),
        text: text.map(fill),
        html: html.map(fill),
        headers: header_map(m.get("Headers")),
        ..Default::default()
    };
    if let Some(id) = template_id {
        d.template = Some(TemplateInfo {
            field: "TemplateID".into(),
            id: crate::util::value_to_string(id),
            data: vars.clone(),
            rendered: false,
        });
    }
    for (key, inline) in [("Attachments", false), ("InlinedAttachments", true)] {
        let Some(Value::Array(list)) = m.get(key) else { continue };
        for (i, a) in list.iter().enumerate() {
            let filename = str_field(a, "Filename").unwrap_or("attachment");
            let content = str_field(a, "Base64Content")
                .ok_or_else(|| mj("mj-0003", "Missing mandatory property.", &[&format!("{key}[{i}].Base64Content")]))?;
            let cid = inline.then(|| str_field(a, "ContentID")).flatten();
            let att = attachment(content, filename, str_field(a, "ContentType"), cid, inline).ok_or_else(|| {
                mj("mj-0004", "Type mismatch. Expected type \"base64\".", &[&format!("{key}[{i}].Base64Content")])
            })?;
            d.attachments.push(att);
        }
    }
    if let Some(c) = str_field(m, "MonitoringCategory") {
        d.tags.push(c.to_string());
    }
    super::keep(
        &mut d,
        m,
        &[
            "CustomID",
            "EventPayload",
            "CustomCampaign",
            "DeduplicateCampaign",
            "URLTags",
            "Priority",
            "TrackOpens",
            "TrackClicks",
            "TemplateLanguage",
            "Variables",
            "MonitoringCategory",
        ],
    );
    Ok(d)
}
