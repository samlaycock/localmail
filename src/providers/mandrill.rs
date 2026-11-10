//! Mailchimp Transactional (Mandrill): `POST /api/1.3/messages/send`, `/send-template` and
//! `/send-raw` (each also with `.json`, and under the older `/api/1.0`). The API key is the
//! `key` field of the body; required parameters are checked before it, as Mandrill does. Unless
//! `preserve_recipients` is set, each recipient gets a message of their own with their merge
//! vars filled in. The answer has one entry per recipient, with a status such as `sent`,
//! `queued`, `rejected` or `invalid`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::{attachment, header_map, str_field};
use crate::address::{Addr, parse_list};
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    let mut r = Router::new();
    for (path, handler) in [("send", Kind::Send), ("send-template", Kind::Template), ("send-raw", Kind::Raw)] {
        let h = move |state: State<AppState>, body: Bytes| send(state, handler, body);
        for version in ["1.0", "1.3"] {
            r = r
                .route(&format!("/api/{version}/messages/{path}"), post(h))
                .route(&format!("/api/{version}/messages/{path}.json"), post(h));
        }
    }
    r
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Send,
    Template,
    Raw,
}

impl Kind {
    /// The parameters a call can't do without.
    fn required(self) -> &'static [&'static str] {
        match self {
            Kind::Send => &["message"],
            Kind::Template => &["template_name", "template_content", "message"],
            Kind::Raw => &["raw_message"],
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Kind::Send => "POST /api/1.3/messages/send",
            Kind::Template => "POST /api/1.3/messages/send-template",
            Kind::Raw => "POST /api/1.3/messages/send-raw",
        }
    }
}

/// Which recipients each message is for, to answer per recipient.
struct Plan {
    /// For each draft, its recipients.
    recipients: Vec<Vec<Addr>>,
    /// Recipients whose address is invalid.
    invalid: Vec<String>,
    /// `queued` (async) or `scheduled` (send_at) instead of `sent`.
    status: &'static str,
}

fn validation(detail: Value) -> Failure {
    Failure::Invalid(Invalid {
        message: format!("Validation error: {detail}"),
        code: Some("ValidationError".into()),
        ..Default::default()
    })
}

fn blank(path: &[&str]) -> Failure {
    let mut v = json!("Sorry, this field can't be left blank.");
    for key in path.iter().rev() {
        v = json!({ *key: v });
    }
    validation(v)
}

async fn send(State(state): State<AppState>, kind: Kind, body: Bytes) -> Response {
    let read = serde_json::from_slice::<Value>(&body).map_err(|_| Failure::MissingAuth).and_then(|req| {
        if let Some(p) = kind.required().iter().find(|p| req.get(**p).is_none_or(Value::is_null)) {
            return Err(specify(p));
        }
        if str_field(&req, "key").is_none() {
            return Err(Failure::MissingAuth);
        }
        match kind {
            Kind::Raw => read_raw(&req),
            Kind::Send | Kind::Template => read_send(&req, kind),
        }
    });
    let (items, plan) = match read {
        Ok((items, plan)) => (Ok(items), Some(plan)),
        Err(f) => (Err(f), None),
    };
    let outcome = submit(&state, Provider::Mandrill, kind.endpoint(), items, true).await;
    let mut resp = match (&outcome.result, plan) {
        (Ok(results), Some(plan)) => {
            let mut entries = vec![];
            for (r, rcpts) in results.iter().zip(&plan.recipients) {
                for (i, a) in rcpts.iter().enumerate() {
                    entries.push(match r {
                        ItemResult::Sent(s) => json!({
                            "email": a.email, "status": plan.status, "reject_reason": null, "queued_reason": null,
                            "_id": if i == 0 { s.message_id.clone() } else { Provider::Mandrill.new_message_id() },
                        }),
                        ItemResult::Failed(Failure::Rejected(_)) => json!({
                            "email": a.email, "status": "rejected", "reject_reason": "hard-bounce", "queued_reason": null,
                            "_id": Provider::Mandrill.new_message_id(),
                        }),
                        ItemResult::Failed(_) => json!({
                            "email": a.email, "status": "invalid", "reject_reason": null, "queued_reason": null,
                            "_id": Provider::Mandrill.new_message_id(),
                        }),
                    });
                }
            }
            for email in plan.invalid {
                entries.push(json!({
                    "email": email, "status": "invalid", "reject_reason": null, "queued_reason": null,
                    "_id": Provider::Mandrill.new_message_id(),
                }));
            }
            Json(Value::Array(entries)).into_response()
        }
        (Ok(_), None) => error(&Failure::ServerError),
        (Err(f), _) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn error(f: &Failure) -> Response {
    let body = |code: i64, name: &str, message: &str| {
        Json(json!({ "status": "error", "code": code, "name": name, "message": message }))
    };
    let bad = StatusCode::BAD_REQUEST;
    match f {
        Failure::MissingAuth => (bad, body(-1, "ValidationError", "You must specify a key value")).into_response(),
        Failure::BadAuth => (StatusCode::UNAUTHORIZED, body(401, "Invalid_Key", "Invalid API key")).into_response(),
        Failure::Invalid(i) => {
            let code = if i.message.starts_with("Validation error:") { -2 } else { -1 };
            (bad, body(code, i.code.as_deref().unwrap_or("ValidationError"), &i.message)).into_response()
        }
        Failure::Rejected(addr) => (bad, body(-1, "ValidationError", &format!("{addr} was rejected"))).into_response(),
        Failure::RateLimited => {
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], body(-1, "GeneralError", "Too many requests"))
                .into_response()
        }
        Failure::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, body(-1, "GeneralError", "An unexpected error occurred"))
                .into_response()
        }
    }
}

/// A required parameter is missing: `You must specify a message value`.
fn specify(param: &str) -> Failure {
    Failure::Invalid(Invalid {
        message: format!("You must specify a {param} value"),
        code: Some("ValidationError".into()),
        ..Default::default()
    })
}

/// `[{name, content}]` as an object.
fn name_content(v: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for item in v.and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = str_field(item, "name") {
            out.insert(name.to_string(), item.get("content").cloned().unwrap_or(Value::Null));
        }
    }
    out
}

/// Fill in merge tags: `*|NAME|*` (mailchimp, names case-insensitive) or `{{name}}` (handlebars).
fn merge(s: &str, vars: &Map<String, Value>, handlebars: bool) -> String {
    if handlebars {
        return crate::util::render_placeholders(s, &Value::Object(vars.clone()));
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\*\|([A-Za-z0-9_:]+)\|\*").unwrap());
    re.replace_all(s, |c: &regex::Captures| match vars.iter().find(|(k, _)| k.eq_ignore_ascii_case(&c[1])) {
        Some((_, v)) => crate::util::value_to_string(v),
        None => c[0].to_string(),
    })
    .into_owned()
}

fn read_send(req: &Value, kind: Kind) -> Result<(Vec<Result<Draft, Failure>>, Plan), Failure> {
    let template_name = match kind {
        Kind::Template => Some(str_field(req, "template_name").ok_or_else(|| specify("template_name"))?),
        _ => None,
    };
    let m = match req.get("message") {
        Some(m) if m.is_object() => m,
        _ => return Err(specify("message")),
    };
    let to = match m.get("to") {
        Some(Value::Array(list)) if !list.is_empty() => list,
        _ => {
            return Err(validation(
                json!({ "message": { "to": [{ "email": "Sorry, this field can't be left blank." }] } }),
            ));
        }
    };
    let from = match str_field(m, "from_email") {
        Some(e) => {
            let a = Addr::new(str_field(m, "from_name"), e);
            if !a.is_valid() {
                return Err(validation(
                    json!({ "message": { "from_email": "An email address must contain a single @" } }),
                ));
            }
            Some(a)
        }
        None if template_name.is_some() => None,
        None => return Err(blank(&["message", "from_email"])),
    };
    let mut headers = header_map(m.get("headers"));
    let reply_to: Vec<Addr> = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("reply-to"))
        .map(|(_, v)| parse_list(v))
        .unwrap_or_default();
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("reply-to"));

    // (address, type), and the addresses that aren't valid.
    let mut rcpts: Vec<(Addr, String)> = vec![];
    let mut invalid = vec![];
    for r in to {
        let email = str_field(r, "email").unwrap_or_default();
        let a = Addr::new(str_field(r, "name"), email);
        if a.is_valid() {
            rcpts.push((a, str_field(r, "type").unwrap_or("to").to_ascii_lowercase()));
        } else {
            invalid.push(email.to_string());
        }
    }
    if let Some(bcc) = str_field(m, "bcc_address") {
        rcpts.push((Addr::new(None, bcc), "bcc".into()));
    }

    let mut base = Draft {
        from,
        reply_to,
        subject: str_field(m, "subject").unwrap_or_default().to_string(),
        text: str_field(m, "text").map(String::from),
        html: str_field(m, "html").map(String::from),
        headers,
        request: Some(crate::util::elide_long_strings(req)),
        ..Default::default()
    };
    if base.html.is_none() && base.text.is_none() && template_name.is_none() {
        // Mandrill sends an empty message; so does localmail.
        base.text = Some(String::new());
    }
    for (key, inline) in [("attachments", false), ("images", true)] {
        for a in m.get(key).and_then(Value::as_array).into_iter().flatten() {
            let name = str_field(a, "name").unwrap_or("attachment");
            let content = str_field(a, "content").unwrap_or_default();
            // Images are referenced as `cid:<name>`.
            let att =
                attachment(content, name, str_field(a, "type"), inline.then_some(name), inline).ok_or_else(|| {
                    validation(json!({ "message": { key: "Attachment content must be base64 encoded" } }))
                })?;
            base.attachments.push(att);
        }
    }
    base.tags = m
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    super::keep(
        &mut base,
        m,
        &[
            "metadata",
            "subaccount",
            "important",
            "track_opens",
            "track_clicks",
            "merge_language",
            "preserve_recipients",
            "google_analytics_campaign",
        ],
    );
    super::keep(&mut base, req, &["async", "ip_pool", "send_at"]);

    let merging = m.get("merge").and_then(Value::as_bool).unwrap_or(true);
    let handlebars = str_field(m, "merge_language").is_some_and(|l| l.eq_ignore_ascii_case("handlebars"));
    let global = name_content(m.get("global_merge_vars"));
    let per_rcpt = |email: &str| -> Map<String, Value> {
        let mut vars = global.clone();
        for mv in m.get("merge_vars").and_then(Value::as_array).into_iter().flatten() {
            if str_field(mv, "rcpt").is_some_and(|r| r.eq_ignore_ascii_case(email)) {
                vars.extend(name_content(mv.get("vars")));
            }
        }
        vars
    };
    let template_data = |vars: &Map<String, Value>| json!({ "template_content": name_content(req.get("template_content")), "merge_vars": vars });
    let build = |mut d: Draft, vars: &Map<String, Value>| -> Draft {
        if let Some(name) = template_name {
            d.template = Some(TemplateInfo {
                field: "template_name".into(),
                id: name.into(),
                data: template_data(vars),
                rendered: false,
            });
        } else if merging && !vars.is_empty() {
            super::fill(&mut d, |s| merge(s, vars, handlebars));
        }
        if !vars.is_empty() {
            d.details.insert("merge_vars".into(), Value::Object(vars.clone()));
        }
        d
    };

    let status = if req.get("async").and_then(Value::as_bool).unwrap_or(false) {
        "queued"
    } else if str_field(req, "send_at").is_some() {
        "scheduled"
    } else {
        "sent"
    };
    let preserve = m.get("preserve_recipients").and_then(Value::as_bool).unwrap_or(false);
    let (mut items, mut recipients) = (vec![], vec![]);
    if preserve {
        // One message, everyone in its headers (except Bcc), merged with global vars only.
        let mut d = base;
        for (a, t) in &rcpts {
            match t.as_str() {
                "cc" => d.cc.push(a.clone()),
                "bcc" => d.bcc.push(a.clone()),
                _ => d.to.push(a.clone()),
            }
        }
        recipients.push(d.recipients().cloned().collect());
        items.push(Ok(build(d, &global)));
    } else {
        for (a, t) in rcpts {
            let mut d = base.clone();
            // Each copy shows only its own recipient; a Bcc copy shows none.
            match t.as_str() {
                "bcc" => d.bcc = vec![a.clone()],
                "cc" => d.cc = vec![a.clone()],
                _ => d.to = vec![a.clone()],
            }
            let vars = per_rcpt(&a.email);
            recipients.push(vec![a]);
            items.push(Ok(build(d, &vars)));
        }
    }
    Ok((items, Plan { recipients, invalid, status }))
}

fn read_raw(req: &Value) -> Result<(Vec<Result<Draft, Failure>>, Plan), Failure> {
    let raw = str_field(req, "raw_message").ok_or_else(|| specify("raw_message"))?;
    let to: Vec<Addr> = req
        .get("to")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|e| Addr::new(None, e))
        .collect();
    let (valid, invalid): (Vec<Addr>, Vec<Addr>) = to.into_iter().partition(Addr::is_valid);
    let mut d = Draft::from_raw(raw.as_bytes().to_vec(), (!valid.is_empty()).then_some(valid));
    if let Some(e) = str_field(req, "from_email") {
        d.from = Some(Addr::new(str_field(req, "from_name"), e));
    }
    if d.recipients().next().is_none() && invalid.is_empty() {
        return Err(validation(json!({ "to": "No recipients were found in the message or the to parameter" })));
    }
    let mut shown = req.clone();
    shown["raw_message"] = json!(format!("({} characters of MIME, shown below)", raw.len()));
    d.request = Some(crate::util::elide_long_strings(&shown));
    super::keep(&mut d, req, &["async", "ip_pool", "send_at", "return_path_domain"]);
    let status = if req.get("async").and_then(Value::as_bool).unwrap_or(false) {
        "queued"
    } else if str_field(req, "send_at").is_some() {
        "scheduled"
    } else {
        "sent"
    };
    let recipients = vec![d.recipients().cloned().collect()];
    let items = if d.recipients().next().is_some() { vec![Ok(d)] } else { vec![] };
    let recipients = if items.is_empty() { vec![] } else { recipients };
    Ok((items, Plan { recipients, invalid: invalid.into_iter().map(|a| a.email).collect(), status }))
}
