//! Mailgun's sending API: `POST /v3/{domain}/messages` (fields, as multipart or URL-encoded form
//! data) and `POST /v3/{domain}/messages.mime` (a ready-made MIME `message` file). With
//! `recipient-variables`, each `to` recipient gets a message of their own, with `%recipient.x%`
//! filled in, as in a Mailgun batch send.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::{basic, header};
use crate::address::{Addr, parse_list};
use crate::message::{Attachment, Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const MAX_RECIPIENTS: usize = 1000;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v3/{domain}/messages", post(messages)).route("/v3/{domain}/messages.mime", post(mime))
}

/// One form field: text, or an uploaded file.
enum Field {
    Text(String),
    File { filename: String, content_type: Option<String>, content: Vec<u8> },
}

struct Form(Vec<(String, Field)>);

impl Form {
    fn texts<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> + 'a {
        let name = name.to_string();
        self.0.iter().filter(move |(n, _)| *n == name).filter_map(|(_, f)| match f {
            Field::Text(t) => Some(t.as_str()),
            Field::File { .. } => None,
        })
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.texts(name).map(str::trim).find(|t| !t.is_empty())
    }

    fn files<'a>(&'a self, name: &str) -> impl Iterator<Item = (&'a str, Option<&'a str>, &'a [u8])> + 'a {
        let name = name.to_string();
        self.0.iter().filter(move |(n, _)| *n == name).filter_map(|(_, f)| match f {
            Field::File { filename, content_type, content } => {
                Some((filename.as_str(), content_type.as_deref(), content.as_slice()))
            }
            // A plain text field holding the file's content (some clients send that).
            Field::Text(_) => None,
        })
    }

    /// The form for display: text fields (a list when repeated) and a note for each file.
    fn display(&self) -> Value {
        let mut out = Map::new();
        for (name, f) in &self.0 {
            let v = match f {
                Field::Text(t) => crate::util::elide_long_strings(&Value::String(t.clone())),
                Field::File { filename, content_type, content } => json!({
                    "filename": filename, "content_type": content_type, "size": content.len(),
                }),
            };
            match out.get_mut(name) {
                Some(Value::Array(list)) => list.push(v),
                Some(prev) => *prev = Value::Array(vec![prev.clone(), v]),
                None => {
                    out.insert(name.clone(), v);
                }
            }
        }
        Value::Object(out)
    }
}

async fn read_form(headers: &HeaderMap, body: Bytes) -> Result<Form, Failure> {
    let content_type = header(headers, "content-type").unwrap_or_default().to_string();
    if content_type.to_ascii_lowercase().starts_with("multipart/form-data") {
        let req = Request::builder()
            .header("content-type", &content_type)
            .body(Body::from(body))
            .map_err(|_| Failure::invalid("Invalid multipart body"))?;
        let mut multipart =
            Multipart::from_request(req, &()).await.map_err(|_| Failure::invalid("Invalid multipart body"))?;
        let mut fields = vec![];
        while let Some(field) = multipart.next_field().await.map_err(|_| Failure::invalid("Invalid multipart body"))? {
            let name = field.name().unwrap_or_default().to_string();
            let filename = field.file_name().map(String::from);
            let ct = field.content_type().map(String::from);
            let content = field.bytes().await.map_err(|_| Failure::invalid("Invalid multipart body"))?.to_vec();
            let value = match filename {
                Some(filename) => Field::File { filename, content_type: ct, content },
                // mailgun.js sends a MIME `message` as a plain field; keep its bytes as they are.
                None if name == "message" => Field::File { filename: "message".into(), content_type: ct, content },
                None => Field::Text(String::from_utf8_lossy(&content).into_owned()),
            };
            fields.push((name, value));
        }
        return Ok(Form(fields));
    }
    Ok(Form(url::form_urlencoded::parse(&body).into_owned().map(|(k, v)| (k, Field::Text(v))).collect()))
}

fn authorize(headers: &HeaderMap) -> Result<(), Failure> {
    basic(headers).map(|_| ()).ok_or(Failure::MissingAuth)
}

fn message(text: impl Into<String>) -> Failure {
    Failure::invalid(text)
}

/// A Mailgun message id, e.g. `<20261110120000.12345.ab12cd34ef56ab78@mg.example.com>`.
fn message_id(domain: &str) -> String {
    let stamp: String = crate::util::iso8601(crate::util::wall_clock()).chars().filter(char::is_ascii_digit).collect();
    format!("<{stamp}.{}.{}@{domain}>", crate::util::random_digits(5), crate::util::random_hex(8).to_ascii_uppercase())
}

fn addresses(form: &Form, name: &str) -> Result<Vec<Addr>, Failure> {
    let list: Vec<Addr> = form.texts(name).flat_map(parse_list).collect();
    if list.iter().any(|a| !a.is_valid()) {
        return Err(message(format!("{name} parameter is not a valid address. please check documentation")));
    }
    Ok(list)
}

/// What every message of a request shares, besides its content: tags, options, headers,
/// variables.
fn common(form: &Form, draft: &mut Draft) {
    draft.tags = form.texts("o:tag").map(String::from).collect();
    let mut options = Map::new();
    let mut variables = Map::new();
    for (name, f) in &form.0 {
        let Field::Text(value) = f else { continue };
        if let Some(h) = name.strip_prefix("h:") {
            if h.eq_ignore_ascii_case("reply-to") {
                draft.reply_to.extend(parse_list(value));
            } else {
                draft.headers.push((h.to_string(), value.clone()));
            }
        } else if let Some(o) = name.strip_prefix("o:").filter(|o| *o != "tag") {
            options.insert(o.to_string(), Value::String(value.clone()));
        } else if let Some(v) = name.strip_prefix("v:") {
            variables.insert(v.to_string(), Value::String(value.clone()));
        }
    }
    if !variables.is_empty() {
        draft.headers.push(("X-Mailgun-Variables".into(), Value::Object(variables.clone()).to_string()));
        draft.details.insert("variables".into(), Value::Object(variables));
    }
    if !options.is_empty() {
        draft.details.insert("options".into(), Value::Object(options));
    }
}

async fn messages(
    State(state): State<AppState>,
    Path(domain): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let endpoint = format!("POST /v3/{domain}/messages");
    let items = match authorize(&headers) {
        Ok(()) => read_form(&headers, body).await.and_then(|form| read(&form, &domain)),
        Err(f) => Err(f),
    };
    respond(&state, &endpoint, items).await
}

async fn mime(State(state): State<AppState>, Path(domain): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let endpoint = format!("POST /v3/{domain}/messages.mime");
    let items = match authorize(&headers) {
        Ok(()) => read_form(&headers, body).await.and_then(|form| read_mime(&form, &domain)),
        Err(f) => Err(f),
    };
    respond(&state, &endpoint, items).await
}

async fn respond(state: &AppState, endpoint: &str, items: Result<Vec<Result<Draft, Failure>>, Failure>) -> Response {
    let outcome = submit(state, Provider::Mailgun, endpoint, items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.iter().find_map(|r| match r {
            ItemResult::Sent(s) => Some(s),
            _ => None,
        }) {
            Some(sent) => Json(json!({ "id": sent.message_id, "message": "Queued. Thank you." })).into_response(),
            None => error(&Failure::ServerError),
        },
        Err(f) => error(f),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn read(form: &Form, domain: &str) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    let from = match form.text("from") {
        Some(f) => {
            let a = crate::address::parse_one(f);
            if !a.is_valid() {
                return Err(message("from parameter is not a valid address. please check documentation"));
            }
            a
        }
        None => return Err(message("from parameter is missing")),
    };
    let to = addresses(form, "to")?;
    if to.is_empty() {
        return Err(message("to parameter is missing"));
    }
    let (cc, bcc) = (addresses(form, "cc")?, addresses(form, "bcc")?);
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(message(format!("Too many recipients. The maximum is {MAX_RECIPIENTS}.")));
    }
    let template = form.text("template");
    let subject = form.text("subject");
    if subject.is_none() && template.is_none() {
        return Err(message("subject parameter is missing"));
    }
    let (text, html, amp) = (form.text("text"), form.text("html"), form.text("amp-html"));
    if text.is_none() && html.is_none() && amp.is_none() && template.is_none() {
        return Err(message("Need at least one of 'text', 'html', 'amp-html' or 'template' parameters specified"));
    }
    let recipient_vars = match form.text("recipient-variables") {
        Some(rv) => match serde_json::from_str::<Value>(rv) {
            Ok(Value::Object(m)) => Some(m),
            _ => return Err(message("recipient-variables parameter is not a valid JSON")),
        },
        None => None,
    };

    let mut base = Draft {
        from: Some(from),
        cc,
        bcc,
        subject: subject.unwrap_or_default().to_string(),
        text: text.map(String::from),
        html: html.map(String::from),
        request: Some(form.display()),
        provider_message_id: Some(message_id(domain)),
        ..Default::default()
    };
    common(form, &mut base);
    if let Some(amp) = amp {
        base.details.insert("amp-html".into(), Value::String(amp.into()));
    }
    if let Some(id) = template {
        let data = form
            .text("t:variables")
            .or_else(|| form.text("h:X-Mailgun-Variables"))
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or(Value::Null);
        base.template = Some(TemplateInfo { field: "template".into(), id: id.into(), data, rendered: false });
        if let Some(version) = form.text("t:version") {
            base.details.insert("t:version".into(), Value::String(version.into()));
        }
    }
    for (kind, inline) in [("attachment", false), ("inline", true)] {
        for (filename, ct, content) in form.files(kind) {
            base.attachments.push(Attachment {
                filename: filename.to_string(),
                content_type: ct
                    .filter(|c| *c != "application/octet-stream")
                    .map(String::from)
                    .unwrap_or_else(|| super::guess_type(filename).into()),
                content: content.to_vec(),
                // Mailgun inline images are referenced as `cid:<filename>`.
                content_id: inline.then(|| filename.to_string()),
                inline,
            });
        }
    }

    let Some(vars) = recipient_vars else {
        return Ok(vec![Ok(Draft { to, ..base })]);
    };
    base.details.insert("recipient-variables".into(), Value::Object(vars.clone()));
    // A batch send: a message to each `to` recipient, with their variables filled in.
    Ok(to
        .into_iter()
        .map(|addr| {
            let data = vars.get(&addr.email).cloned().unwrap_or(Value::Null);
            let mut d = base.clone();
            super::fill(&mut d, |s| substitute(s, &data));
            d.to = vec![addr];
            Ok(d)
        })
        .collect())
}

/// Fill in `%recipient.name%` from a recipient's variables.
fn substitute(s: &str, data: &Value) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"%recipient\.([A-Za-z0-9_\-]+)%").unwrap());
    re.replace_all(s, |c: &regex::Captures| match data.get(&c[1]) {
        Some(v) => crate::util::value_to_string(v),
        None => String::new(),
    })
    .into_owned()
}

fn read_mime(form: &Form, domain: &str) -> Result<Vec<Result<Draft, Failure>>, Failure> {
    let to = addresses(form, "to")?;
    if to.is_empty() {
        return Err(message("to parameter is missing"));
    }
    let raw = match form.files("message").next() {
        Some((_, _, content)) => content.to_vec(),
        // Some clients send the message as a plain field.
        None => match form.text("message") {
            Some(t) => t.as_bytes().to_vec(),
            None => return Err(message("message parameter is missing")),
        },
    };
    let mut d = Draft::from_raw(raw, Some(to));
    common(form, &mut d);
    // The MIME is kept as it was sent; the custom headers are only recorded.
    let extra = std::mem::take(&mut d.headers);
    if !extra.is_empty() {
        d.details
            .insert("headers".into(), Value::Object(extra.into_iter().map(|(k, v)| (k, Value::String(v))).collect()));
    }
    d.request = Some(form.display());
    d.provider_message_id = Some(message_id(domain));
    Ok(vec![Ok(d)])
}

fn error(f: &Failure) -> Response {
    let json_error = |status: StatusCode, message: &str| (status, Json(json!({ "message": message }))).into_response();
    match f {
        // Mailgun answers bad credentials with a bare `Forbidden`.
        Failure::MissingAuth | Failure::BadAuth => {
            let headers =
                [("content-type", "text/plain; charset=utf-8"), ("www-authenticate", "Basic realm=\"MG API\"")];
            (StatusCode::UNAUTHORIZED, headers, "Forbidden").into_response()
        }
        Failure::Invalid(Invalid { message, status, .. }) => {
            json_error(status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::BAD_REQUEST), message)
        }
        Failure::Rejected(addr) => json_error(StatusCode::BAD_REQUEST, &format!("{addr} was rejected")),
        Failure::RateLimited => {
            let mut resp = json_error(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
            resp.headers_mut().insert("retry-after", "60".parse().unwrap());
            resp
        }
        Failure::ServerError => json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    }
}
