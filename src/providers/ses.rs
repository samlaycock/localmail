//! Amazon SES: v2's `SendEmail` (`POST /v2/email/outbound-emails`, JSON) and v1's `SendEmail`,
//! `SendRawEmail` and `SendTemplatedEmail` (`POST /` with `Action=…`, form-encoded, XML answers).
//! Requests must be SigV4-signed, but any credentials are accepted: the signature isn't checked.

use std::collections::BTreeMap;
use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use super::{attachment, str_field};
use crate::address::{Addr, parse_list, parse_one};
use crate::html::esc;
use crate::message::{Draft, Provider, TemplateInfo};
use crate::send::{Failure, Invalid, ItemResult, submit};
use crate::state::AppState;

const MAX_RECIPIENTS: usize = 50;
const XMLNS: &str = "http://ses.amazonaws.com/doc/2010-12-01/";

pub fn routes() -> Router<AppState> {
    Router::new().route("/v2/email/outbound-emails", post(send_v2)).route("/", post(query_v1))
}

/// The SigV4 credential scope: (access key id, region).
fn credential(headers: &HeaderMap) -> Option<(String, String)> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = auth.trim().strip_prefix("AWS4-HMAC-SHA256")?;
    let scope = rest.split(',').find_map(|p| p.trim().strip_prefix("Credential="))?;
    let mut parts = scope.split('/');
    let key = parts.next().filter(|k| !k.is_empty())?.to_string();
    let region = parts.nth(1).filter(|r| !r.is_empty()).unwrap_or("us-east-1").to_string();
    Some((key, region))
}

fn invalid(code: &str, message: impl Into<String>) -> Failure {
    Failure::Invalid(Invalid { message: message.into(), code: Some(code.into()), ..Default::default() })
}

fn check(list: Vec<Addr>) -> Result<Vec<Addr>, Failure> {
    if list.iter().any(|a| !a.is_valid()) {
        return Err(invalid("InvalidParameterValue", "Missing final '@domain'"));
    }
    Ok(list)
}

fn too_many(d: &Draft) -> Result<(), Failure> {
    if d.recipients().count() > MAX_RECIPIENTS {
        return Err(invalid("MessageRejected", format!("Recipient count exceeds {MAX_RECIPIENTS}.")));
    }
    Ok(())
}

fn rejected_message(addr: &str, region: &str) -> String {
    format!(
        "Email address is not verified. The following identities failed the check in region {}: {addr}",
        region.to_ascii_uppercase()
    )
}

fn decode_raw(data: &str) -> Result<Vec<u8>, Failure> {
    crate::util::b64_decode_lenient(data)
        .ok_or_else(|| invalid("InvalidParameterValue", "Invalid base64 in the raw message data."))
}

// ---------------------------------------------------------------------------
// v2 (restJson1)
// ---------------------------------------------------------------------------

async fn send_v2(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let cred = credential(&headers);
    let region = cred.as_ref().map(|c| c.1.clone()).unwrap_or_else(|| "us-east-1".into());
    let items = cred.ok_or(Failure::MissingAuth).and_then(|(key, region)| {
        let v: Value = serde_json::from_slice(&body).map_err(|_| invalid("BadRequestException", "Invalid JSON."))?;
        let mut d = read_v2(&v)?;
        d.details.insert("region".into(), region.into());
        d.details.insert("access_key_id".into(), key.into());
        Ok(vec![Ok(d)])
    });
    let outcome = submit(&state, Provider::Ses, "POST /v2/email/outbound-emails", items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(sent)) => (
                [("x-amzn-requestid", uuid::Uuid::new_v4().to_string())],
                Json(json!({ "MessageId": sent.message_id })),
            )
                .into_response(),
            Some(ItemResult::Failed(f)) => error_v2(f, &region),
            None => error_v2(&Failure::ServerError, &region),
        },
        Err(f) => error_v2(f, &region),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn error_v2(f: &Failure, region: &str) -> Response {
    let (status, kind, message) = match f {
        Failure::MissingAuth => (
            StatusCode::FORBIDDEN,
            "MissingAuthenticationTokenException".to_string(),
            "Missing Authentication Token".to_string(),
        ),
        Failure::BadAuth => (
            StatusCode::FORBIDDEN,
            "UnrecognizedClientException".into(),
            "The security token included in the request is invalid.".into(),
        ),
        Failure::Invalid(i) => {
            let kind = match i.code.as_deref() {
                // v1's generic validation error is v2's BadRequestException.
                None | Some("InvalidParameterValue") => "BadRequestException",
                Some(other) => other,
            };
            (StatusCode::BAD_REQUEST, kind.to_string(), i.message.clone())
        }
        Failure::Rejected(addr) => (StatusCode::BAD_REQUEST, "MessageRejected".into(), rejected_message(addr, region)),
        Failure::RateLimited => {
            (StatusCode::TOO_MANY_REQUESTS, "TooManyRequestsException".into(), "Maximum sending rate exceeded.".into())
        }
        Failure::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, "InternalFailure".into(), "An internal error occurred.".into())
        }
    };
    (
        status,
        [("x-amzn-errortype", kind.clone()), ("x-amzn-requestid", uuid::Uuid::new_v4().to_string())],
        Json(json!({ "message": message })),
    )
        .into_response()
}

fn str_list(v: Option<&Value>) -> Vec<Addr> {
    v.and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).flat_map(parse_list).collect())
        .unwrap_or_default()
}

fn read_v2(v: &Value) -> Result<Draft, Failure> {
    let content = v
        .get("Content")
        .filter(|c| c.is_object())
        .ok_or_else(|| invalid("BadRequestException", "Content is required."))?;
    let kinds: Vec<&str> =
        ["Simple", "Raw", "Template"].into_iter().filter(|k| content.get(*k).is_some_and(|c| !c.is_null())).collect();
    if kinds.len() != 1 {
        return Err(invalid(
            "BadRequestException",
            "Exactly one of Simple, Raw or Template content must be specified.",
        ));
    }
    let dest = v.get("Destination");
    let to = check(str_list(dest.and_then(|d| d.get("ToAddresses"))))?;
    let cc = check(str_list(dest.and_then(|d| d.get("CcAddresses"))))?;
    let bcc = check(str_list(dest.and_then(|d| d.get("BccAddresses"))))?;
    let from = str_field(v, "FromEmailAddress").map(parse_one);
    if from.as_ref().is_some_and(|a| !a.is_valid()) {
        return Err(invalid("InvalidParameterValue", "Missing final '@domain'"));
    }

    let mut draft = if let Some(raw) = content.pointer("/Raw/Data").and_then(Value::as_str) {
        let rcpts: Vec<Addr> = to.iter().chain(&cc).chain(&bcc).cloned().collect();
        let mut d = Draft::from_raw(decode_raw(raw)?, (!rcpts.is_empty()).then_some(rcpts));
        if from.is_some() {
            d.from = from;
        }
        if d.recipients().next().is_none() {
            return Err(invalid("MessageRejected", "No recipients were specified."));
        }
        d
    } else {
        let Some(from) = from else {
            return Err(invalid("BadRequestException", "FromEmailAddress is required."));
        };
        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err(invalid("BadRequestException", "Destination is required."));
        }
        let mut d = Draft { from: Some(from), to, cc, bcc, ..Default::default() };
        let part = if let Some(simple) = content.get("Simple").filter(|s| !s.is_null()) {
            d.subject = simple.pointer("/Subject/Data").and_then(Value::as_str).unwrap_or_default().into();
            d.text = simple.pointer("/Body/Text/Data").and_then(Value::as_str).map(String::from);
            d.html = simple.pointer("/Body/Html/Data").and_then(Value::as_str).map(String::from);
            if d.subject.is_empty() {
                return Err(invalid("BadRequestException", "Subject is required."));
            }
            simple
        } else {
            let t = &content["Template"];
            let data: Value = match str_field(t, "TemplateData") {
                Some(s) => serde_json::from_str(s)
                    .map_err(|_| invalid("BadRequestException", "TemplateData is not valid JSON."))?,
                None => Value::Object(Default::default()),
            };
            if let Some(tc) = t.get("TemplateContent").filter(|c| c.is_object()) {
                let render = |key: &str| str_field(tc, key).map(|s| crate::util::render_placeholders(s, &data));
                d.subject = render("Subject").unwrap_or_default();
                d.text = render("Text");
                d.html = render("Html");
                d.template =
                    Some(TemplateInfo { field: "TemplateContent".into(), id: "(inline)".into(), data, rendered: true });
            } else {
                let (field, id) = match (str_field(t, "TemplateName"), str_field(t, "TemplateArn")) {
                    (Some(n), _) => ("TemplateName", n),
                    (None, Some(a)) => ("TemplateArn", a),
                    (None, None) => {
                        return Err(invalid(
                            "BadRequestException",
                            "TemplateName, TemplateArn or TemplateContent is required.",
                        ));
                    }
                };
                d.template = Some(TemplateInfo { field: field.into(), id: id.into(), data, rendered: false });
            }
            t
        };
        if let Some(Value::Array(headers)) = part.get("Headers") {
            d.headers = headers
                .iter()
                .filter_map(|h| {
                    Some((str_field(h, "Name")?.to_string(), str_field(h, "Value").unwrap_or_default().to_string()))
                })
                .collect();
        }
        if let Some(Value::Array(list)) = part.get("Attachments") {
            for a in list {
                let name = str_field(a, "FileName").unwrap_or("attachment");
                let inline = str_field(a, "ContentDisposition").is_some_and(|c| c.eq_ignore_ascii_case("INLINE"));
                let att = str_field(a, "RawContent")
                    .and_then(|c| attachment(c, name, str_field(a, "ContentType"), str_field(a, "ContentId"), inline))
                    .ok_or_else(|| {
                        invalid("BadRequestException", format!("Attachment '{name}' has no valid RawContent."))
                    })?;
                d.attachments.push(att);
            }
        }
        d
    };
    too_many(&draft)?;
    if let Some(r) = v.get("ReplyToAddresses") {
        draft.reply_to = check(str_list(Some(r)))?;
    }
    if let Some(Value::Array(tags)) = v.get("EmailTags") {
        draft.tags = tags
            .iter()
            .filter_map(|t| Some(format!("{}={}", str_field(t, "Name")?, str_field(t, "Value").unwrap_or_default())))
            .collect();
    }
    super::keep(
        &mut draft,
        v,
        &["ConfigurationSetName", "FeedbackForwardingEmailAddress", "ListManagementOptions", "TenantName"],
    );
    draft.request = Some(crate::util::elide_long_strings(v));
    Ok(draft)
}

// ---------------------------------------------------------------------------
// v1 (awsQuery)
// ---------------------------------------------------------------------------

async fn query_v1(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let params = crate::util::parse_form(&body);
    let action = params.get("Action").cloned().unwrap_or_default();
    let cred = credential(&headers);
    let region = cred.as_ref().map(|c| c.1.clone()).unwrap_or_else(|| "us-east-1".into());
    let endpoint = format!("POST / ({})", if action.is_empty() { "no Action" } else { &action });
    let items = cred.ok_or(Failure::MissingAuth).and_then(|(key, region)| {
        let mut d = match action.as_str() {
            "SendEmail" => read_send_email(&params)?,
            "SendRawEmail" => read_send_raw(&params)?,
            "SendTemplatedEmail" => read_send_templated(&params)?,
            "" => return Err(invalid("MissingAction", "Could not find operation to perform.")),
            other => {
                return Err(invalid("InvalidAction", format!("The action {other} is not valid for this web service.")));
            }
        };
        too_many(&d)?;
        d.details.insert("region".into(), region.into());
        d.details.insert("access_key_id".into(), key.into());
        // The form, minus the long raw message.
        let shown: BTreeMap<&String, Value> =
            params.iter().map(|(k, v)| (k, crate::util::elide_long_strings(&Value::String(v.clone())))).collect();
        d.request = Some(json!(shown));
        Ok(vec![Ok(d)])
    });
    let outcome = submit(&state, Provider::Ses, &endpoint, items, false).await;
    let mut resp = match &outcome.result {
        Ok(results) => match results.first() {
            Some(ItemResult::Sent(sent)) => xml(
                StatusCode::OK,
                format!(
                    r#"<{action}Response xmlns="{XMLNS}">
  <{action}Result>
    <MessageId>{}</MessageId>
  </{action}Result>
  <ResponseMetadata>
    <RequestId>{}</RequestId>
  </ResponseMetadata>
</{action}Response>
"#,
                    esc(&sent.message_id),
                    uuid::Uuid::new_v4()
                ),
            ),
            Some(ItemResult::Failed(f)) => error_v1(f, &region),
            None => error_v1(&Failure::ServerError, &region),
        },
        Err(f) => error_v1(f, &region),
    };
    outcome.note.attach(&mut resp);
    resp
}

fn xml(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/xml")], body).into_response()
}

fn error_v1(f: &Failure, region: &str) -> Response {
    let (status, kind, code, message) = match f {
        Failure::MissingAuth => (
            StatusCode::FORBIDDEN,
            "Sender",
            "MissingAuthenticationToken".to_string(),
            "Request is missing Authentication Token".to_string(),
        ),
        Failure::BadAuth => (
            StatusCode::FORBIDDEN,
            "Sender",
            "InvalidClientTokenId".into(),
            "The security token included in the request is invalid.".into(),
        ),
        Failure::Invalid(i) => (
            StatusCode::BAD_REQUEST,
            "Sender",
            i.code.clone().unwrap_or_else(|| "InvalidParameterValue".into()),
            i.message.clone(),
        ),
        Failure::Rejected(addr) => {
            (StatusCode::BAD_REQUEST, "Sender", "MessageRejected".into(), rejected_message(addr, region))
        }
        Failure::RateLimited => {
            (StatusCode::BAD_REQUEST, "Sender", "Throttling".into(), "Maximum sending rate exceeded.".into())
        }
        Failure::ServerError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Receiver",
            "InternalFailure".into(),
            "An internal error occurred.".into(),
        ),
    };
    xml(
        status,
        format!(
            r#"<ErrorResponse xmlns="{XMLNS}">
  <Error>
    <Type>{kind}</Type>
    <Code>{}</Code>
    <Message>{}</Message>
  </Error>
  <RequestId>{}</RequestId>
</ErrorResponse>
"#,
            esc(&code),
            esc(&message),
            uuid::Uuid::new_v4()
        ),
    )
}

/// `prefix.member.1`, `prefix.member.2`, ... in order.
fn members(params: &HashMap<String, String>, prefix: &str) -> Vec<String> {
    let p = format!("{prefix}.member.");
    let mut found: Vec<(u32, &String)> =
        params.iter().filter_map(|(k, v)| Some((k.strip_prefix(&p)?.parse().ok()?, v))).collect();
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, v)| v.clone()).collect()
}

fn member_addrs(params: &HashMap<String, String>, prefix: &str) -> Result<Vec<Addr>, Failure> {
    check(members(params, prefix).iter().flat_map(|s| parse_list(s)).collect())
}

fn source(params: &HashMap<String, String>) -> Result<Addr, Failure> {
    let raw = params
        .get("Source")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("MissingParameter", "Source is required."))?;
    let a = parse_one(raw);
    if !a.is_valid() {
        return Err(invalid("InvalidParameterValue", "Missing final '@domain'"));
    }
    Ok(a)
}

/// Destination, ReplyToAddresses and Tags, shared by SendEmail and SendTemplatedEmail.
fn addressed(params: &HashMap<String, String>) -> Result<Draft, Failure> {
    let mut d = Draft {
        from: Some(source(params)?),
        to: member_addrs(params, "Destination.ToAddresses")?,
        cc: member_addrs(params, "Destination.CcAddresses")?,
        bcc: member_addrs(params, "Destination.BccAddresses")?,
        reply_to: member_addrs(params, "ReplyToAddresses")?,
        ..Default::default()
    };
    if d.recipients().next().is_none() {
        return Err(invalid("InvalidParameterValue", "Missing required header 'To'."));
    }
    for n in 1.. {
        let Some(name) = params.get(&format!("Tags.member.{n}.Name")) else { break };
        let value = params.get(&format!("Tags.member.{n}.Value")).cloned().unwrap_or_default();
        d.tags.push(format!("{name}={value}"));
    }
    for k in ["ConfigurationSetName", "ReturnPath", "SourceArn"] {
        if let Some(v) = params.get(k) {
            d.details.insert(k.into(), v.clone().into());
        }
    }
    Ok(d)
}

fn read_send_email(params: &HashMap<String, String>) -> Result<Draft, Failure> {
    let mut d = addressed(params)?;
    d.subject = params.get("Message.Subject.Data").cloned().unwrap_or_default();
    d.text = params.get("Message.Body.Text.Data").cloned();
    d.html = params.get("Message.Body.Html.Data").cloned();
    if d.text.is_none() && d.html.is_none() {
        return Err(invalid("InvalidParameterValue", "Message body is required."));
    }
    Ok(d)
}

fn read_send_templated(params: &HashMap<String, String>) -> Result<Draft, Failure> {
    let mut d = addressed(params)?;
    let template = params
        .get("Template")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| invalid("MissingParameter", "Template is required."))?;
    let data = params.get("TemplateData").ok_or_else(|| invalid("MissingParameter", "TemplateData is required."))?;
    let data: Value =
        serde_json::from_str(data).map_err(|_| invalid("InvalidParameterValue", "TemplateData is not valid JSON."))?;
    d.template = Some(TemplateInfo { field: "Template".into(), id: template.clone(), data, rendered: false });
    Ok(d)
}

fn read_send_raw(params: &HashMap<String, String>) -> Result<Draft, Failure> {
    let data = params
        .get("RawMessage.Data")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| invalid("MissingParameter", "RawMessage.Data is required."))?;
    let destinations = check(members(params, "Destinations").iter().flat_map(|s| parse_list(s)).collect())?;
    let mut d = Draft::from_raw(decode_raw(data)?, (!destinations.is_empty()).then_some(destinations));
    if let Some(s) = params.get("Source").filter(|s| !s.is_empty()) {
        d.from = Some(parse_one(s));
    }
    if d.recipients().next().is_none() {
        return Err(invalid("InvalidParameterValue", "Missing required header 'To'."));
    }
    Ok(d)
}
