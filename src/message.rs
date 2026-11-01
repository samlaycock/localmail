//! The canonical message. Every way in (SMTP and each provider API) produces a [`Draft`], which
//! becomes real MIME (if it isn't already) and is stored with what the provider was sent, so the
//! UI and the dev API treat every message the same way.

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::raw::Raw;
use mail_builder::mime::MimePart;
use mail_parser::MessageParser;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::address::{Addr, parse_list};
use crate::util::{flatten_json, random_from, random_hex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Smtp,
    Ses,
    Sendgrid,
    Postmark,
    Resend,
}

impl Provider {
    pub const ALL: [Provider; 5] =
        [Provider::Smtp, Provider::Ses, Provider::Sendgrid, Provider::Postmark, Provider::Resend];

    pub fn id(self) -> &'static str {
        match self {
            Provider::Smtp => "smtp",
            Provider::Ses => "ses",
            Provider::Sendgrid => "sendgrid",
            Provider::Postmark => "postmark",
            Provider::Resend => "resend",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Smtp => "SMTP",
            Provider::Ses => "Amazon SES",
            Provider::Sendgrid => "SendGrid",
            Provider::Postmark => "Postmark",
            Provider::Resend => "Resend",
        }
    }

    pub fn color(self) -> &'static str {
        match self {
            Provider::Smtp => "#64748b",
            Provider::Ses => "#ff9900",
            Provider::Sendgrid => "#1a82e2",
            Provider::Postmark => "#e4c200",
            // Resend's brand black disappears on a dark background.
            Provider::Resend => "#71717a",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id().eq_ignore_ascii_case(s))
    }

    /// A message ID in the provider's own format.
    pub fn new_message_id(self) -> String {
        match self {
            // e.g. 010001926d0c3d6e-1c2b0b0e-4d6f-4b5e-9a3c-0f3e5d1b2a4c-000000
            Provider::Ses => format!("0100{}-{}-000000", random_hex(6), uuid::Uuid::new_v4()),
            // 22 URL-safe base64 characters, like `XBg2anf2TqCy6WXKQFhieQ`.
            Provider::Sendgrid => random_from(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_", 22),
            Provider::Postmark | Provider::Resend => uuid::Uuid::new_v4().to_string(),
            Provider::Smtp => random_hex(8).to_ascii_uppercase(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    #[serde(skip)]
    pub content: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    #[serde(default)]
    pub inline: bool,
}

/// A send that used a template stored in the provider account, which localmail can't see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateInfo {
    /// What the provider calls the reference, e.g. `TemplateAlias` or `template_id`.
    pub field: String,
    pub id: String,
    #[serde(default)]
    pub data: Value,
    /// The template's content came with the request and was rendered (SES inline templates).
    #[serde(default)]
    pub rendered: bool,
}

/// One email as an API or SMTP client described it.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,
    pub attachments: Vec<Attachment>,
    /// Ready-made MIME (SMTP, SES raw sends); the fields above were read from its headers.
    pub raw: Option<Vec<u8>>,
    pub template: Option<TemplateInfo>,
    /// Tags and categories, as `name` or `name=value`.
    pub tags: Vec<String>,
    /// Anything else the provider was told: metadata, custom args, message stream, ...
    pub details: Map<String, Value>,
    /// The request (or the batch item) that described this message, for display.
    pub request: Option<Value>,
    pub envelope: Option<Envelope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Envelope {
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_user: Option<String>,
}

/// Header fields the builder writes itself; a request can't set them through custom headers.
const BUILDER_HEADERS: [&str; 10] = [
    "from",
    "to",
    "cc",
    "bcc",
    "reply-to",
    "subject",
    "date",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
];

impl Draft {
    /// A draft for ready-made MIME, with the addresses and subject read from its headers. Envelope
    /// recipients the headers don't name are Bcc recipients.
    pub fn from_raw(raw: Vec<u8>, envelope_recipients: Option<Vec<Addr>>) -> Draft {
        let (from, to, cc, subject, reply_to) = {
            let parsed = MessageParser::default().parse(&raw);
            let list = |l: Option<mail_parser::AddressList>| -> Vec<Addr> {
                l.map(|l| l.mailboxes().filter_map(|m| Some(Addr::new(m.name(), m.address()?))).collect())
                    .unwrap_or_default()
            };
            match &parsed {
                Some(m) => (
                    list(m.from()).into_iter().next(),
                    list(m.to()),
                    list(m.cc()),
                    m.subject().unwrap_or_default().to_string(),
                    list(m.reply_to()),
                ),
                None => (None, vec![], vec![], String::new(), vec![]),
            }
        };
        let (to, cc, bcc) = match envelope_recipients {
            Some(rcpts) => {
                let named = |a: &Addr| to.iter().chain(&cc).any(|x| x.key() == a.key());
                let bcc = rcpts.iter().filter(|a| !named(a)).cloned().collect();
                let in_envelope = |list: Vec<Addr>| -> Vec<Addr> {
                    list.into_iter().filter(|a| rcpts.iter().any(|r| r.key() == a.key())).collect()
                };
                (in_envelope(to), in_envelope(cc), bcc)
            }
            None => (to, cc, vec![]),
        };
        Draft { from, to, cc, bcc, reply_to, subject, raw: Some(raw), ..Default::default() }
    }

    /// Every recipient: To, Cc and Bcc.
    pub fn recipients(&self) -> impl Iterator<Item = &Addr> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }

    /// Text and HTML that describe a template send, since the template itself isn't available.
    fn template_bodies(&self, t: &TemplateInfo) -> (String, String) {
        let rows = flatten_json(&t.data);
        let mut text = format!(
            "localmail: this message was sent with the template {} = \"{}\", which is stored in the provider account, so it can't be shown here.\n\nTemplate data:\n\n",
            t.field, t.id
        );
        let mut html = format!(
            r#"<div style="font:14px/1.5 system-ui,sans-serif"><p style="color:#64748b">localmail: this message was sent with the template <code>{}</code> = <b>{}</b>, which is stored in the provider account, so it can't be shown here.</p><table style="border-collapse:collapse">"#,
            crate::html::esc(&t.field),
            crate::html::esc(&t.id)
        );
        if rows.is_empty() {
            text.push_str("(none)\n");
            html.push_str(r#"<tr><td style="color:#64748b">No template data.</td></tr>"#);
        }
        for (k, v) in rows {
            text.push_str(&format!("{k}: {v}\n"));
            let shown = if v.starts_with("http://") || v.starts_with("https://") {
                format!(r#"<a href="{0}">{0}</a>"#, crate::html::esc(&v))
            } else {
                crate::html::esc(&v)
            };
            html.push_str(&format!(
                r#"<tr><td style="padding:4px 16px 4px 0;color:#64748b;vertical-align:top"><code>{}</code></td><td style="padding:4px 0">{shown}</td></tr>"#,
                crate::html::esc(&k)
            ));
        }
        html.push_str("</table></div>");
        (text, html)
    }

    /// The message as MIME: the raw message as sent, or built from the fields.
    pub fn to_mime(&self, message_id: &str, date: i64) -> Vec<u8> {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }
        let addr = |a: &Addr| -> Address<'static> {
            match &a.name {
                Some(n) => (n.clone(), a.email.clone()).into(),
                None => a.email.clone().into(),
            }
        };
        let list = |l: &[Addr]| Address::new_list(l.iter().map(addr).collect());
        let mut b = MessageBuilder::new().subject(self.subject.clone()).date(date);
        if let Some(from) = &self.from {
            b = b.from(addr(from));
        }
        if !self.to.is_empty() {
            b = b.to(list(&self.to));
        }
        if !self.cc.is_empty() {
            b = b.cc(list(&self.cc));
        }
        if !self.reply_to.is_empty() {
            b = b.reply_to(list(&self.reply_to));
        }
        let mut has_message_id = false;
        for (name, value) in &self.headers {
            let lower = name.to_ascii_lowercase();
            if BUILDER_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            has_message_id |= lower == "message-id";
            b = b.header(name.clone(), Raw::new(value.clone()));
        }
        if !has_message_id {
            b = b.message_id(format!("{message_id}@localmail"));
        }
        let (text, html) = match (&self.template, &self.text, &self.html) {
            (Some(t), None, None) if !t.rendered => {
                let (text, html) = self.template_bodies(t);
                (Some(text), Some(html))
            }
            _ => (self.text.clone(), self.html.clone()),
        };
        b.body(self.mime_body(text, html)).write_to_vec().unwrap_or_default()
    }

    /// multipart/mixed [ multipart/related [ multipart/alternative [text, html], inline parts ],
    /// attachments ], leaving out the levels that would have one part.
    fn mime_body(&self, text: Option<String>, html: Option<String>) -> MimePart<'static> {
        let mut bodies: Vec<MimePart<'static>> = vec![];
        if let Some(t) = text {
            bodies.push(MimePart::new("text/plain", t));
        }
        if let Some(h) = html {
            bodies.push(MimePart::new("text/html", h));
        }
        if bodies.is_empty() {
            bodies.push(MimePart::new("text/plain", String::new()));
        }
        let mut body = wrap("multipart/alternative", bodies);
        let part = |a: &Attachment| {
            let ct = if a.content_type.is_empty() { "application/octet-stream".into() } else { a.content_type.clone() };
            MimePart::new(ct, a.content.clone())
        };
        let inline: Vec<_> = self
            .attachments
            .iter()
            .filter(|a| a.inline && a.content_id.is_some())
            .map(|a| part(a).inline().cid(a.content_id.clone().unwrap_or_default()))
            .collect();
        if !inline.is_empty() {
            body = wrap("multipart/related", std::iter::once(body).chain(inline).collect());
        }
        let attached: Vec<_> = self
            .attachments
            .iter()
            .filter(|a| !(a.inline && a.content_id.is_some()))
            .map(|a| {
                let p = part(a).attachment(a.filename.clone());
                match &a.content_id {
                    Some(cid) => p.cid(cid.clone()),
                    None => p,
                }
            })
            .collect();
        if !attached.is_empty() {
            body = wrap("multipart/mixed", std::iter::once(body).chain(attached).collect());
        }
        body
    }
}

fn wrap(content_type: &'static str, mut parts: Vec<MimePart<'static>>) -> MimePart<'static> {
    if parts.len() == 1 { parts.remove(0) } else { MimePart::new(content_type, parts) }
}

/// What's kept with a message besides its MIME: how it arrived, and what the provider was told.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Addr>,
    #[serde(default)]
    pub to: Vec<Addr>,
    #[serde(default)]
    pub cc: Vec<Addr>,
    #[serde(default)]
    pub bcc: Vec<Addr>,
    #[serde(default)]
    pub reply_to: Vec<Addr>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub details: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateInfo>,
    /// The scenario address that applied (a slow or flaky send that went through in the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Envelope>,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub has_attachments: bool,
}

/// A message ready to store.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub received_at: i64,
    pub provider: Provider,
    pub endpoint: String,
    pub provider_message_id: String,
    pub subject: String,
    pub raw: Vec<u8>,
    pub meta: Meta,
}

impl NewMessage {
    pub fn from_draft(
        draft: Draft,
        provider: Provider,
        endpoint: &str,
        provider_message_id: String,
        scenario: Option<String>,
    ) -> Self {
        let received_at = crate::util::now_ms();
        let raw = draft.to_mime(&provider_message_id, received_at / 1000);
        let parsed = MessageParser::default().parse(&raw);
        let preview = parsed.as_ref().map(preview).unwrap_or_default();
        let has_attachments = parsed.as_ref().is_some_and(|m| m.attachments().len() > 0);
        let reply_to = if draft.reply_to.is_empty() {
            parsed
                .as_ref()
                .and_then(|m| m.reply_to())
                .map(|l| l.mailboxes().filter_map(|m| Some(Addr::new(m.name(), m.address()?))).collect())
                .unwrap_or_default()
        } else {
            draft.reply_to
        };
        NewMessage {
            received_at,
            provider,
            endpoint: endpoint.into(),
            provider_message_id,
            subject: draft.subject,
            raw,
            meta: Meta {
                from: draft.from,
                to: draft.to,
                cc: draft.cc,
                bcc: draft.bcc,
                reply_to,
                tags: draft.tags,
                details: draft.details,
                template: draft.template,
                scenario,
                request: draft.request,
                envelope: draft.envelope,
                preview,
                has_attachments,
            },
        }
    }
}

/// A message as listed by the API and the inbox.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Also the cursor: ids only ever grow.
    pub id: i64,
    pub received_at: i64,
    pub date: String,
    pub provider: Provider,
    pub endpoint: String,
    pub provider_message_id: String,
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub subject: String,
    pub preview: String,
    pub size: usize,
    pub has_attachments: bool,
    pub read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateInfo>,
    pub tags: Vec<String>,
}

/// A stored message: its summary, MIME and the rest of what was kept.
#[derive(Debug, Clone)]
pub struct Stored {
    pub summary: Summary,
    pub raw: Vec<u8>,
    pub meta: Meta,
}

#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentInfo {
    pub index: usize,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    pub inline: bool,
}

/// Everything about a message, for `GET /dev/api/messages/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct Detail {
    #[serde(flatten)]
    pub summary: Summary,
    pub reply_to: Vec<Addr>,
    pub message_id: Option<String>,
    pub headers: Vec<Header>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
    pub details: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Envelope>,
}

impl Stored {
    pub fn detail(&self) -> Detail {
        let parsed = MessageParser::default().parse(&self.raw);
        let (headers, text, html, attachments, message_id) = match &parsed {
            Some(m) => (
                m.headers()
                    .iter()
                    .map(|h| Header {
                        name: h.raw_name().to_string(),
                        value: unfold(&String::from_utf8_lossy(h.raw_value())),
                    })
                    .collect(),
                text_part(m),
                html_part(m),
                attachments(m),
                m.message_id().map(String::from),
            ),
            None => (vec![], Some(String::from_utf8_lossy(&self.raw).into_owned()), None, vec![], None),
        };
        Detail {
            summary: self.summary.clone(),
            reply_to: self.meta.reply_to.clone(),
            message_id,
            headers,
            text,
            html,
            attachments,
            details: self.meta.details.clone(),
            request: self.meta.request.clone(),
            envelope: self.meta.envelope.clone(),
        }
    }

    /// Plain text to search for codes and links: the subject, the text body, the HTML body as
    /// text (a code can be in either) and, for template sends, the template data.
    pub fn searchable_text(&self) -> String {
        let parsed = MessageParser::default().parse(&self.raw);
        let mut out = self.summary.subject.clone();
        if let Some(m) = &parsed {
            for body in [text_part(m), html_part(m).map(|h| html_to_text(&h))].into_iter().flatten() {
                out.push('\n');
                out.push_str(&body);
            }
        }
        if let Some(t) = &self.meta.template {
            for (k, v) in flatten_json(&t.data) {
                out.push_str(&format!("\n{k}: {v}"));
            }
        }
        out
    }

    /// The HTML body, if there is one.
    pub fn html(&self) -> Option<String> {
        let parsed = MessageParser::default().parse(&self.raw)?;
        html_part(&parsed)
    }

    /// An attachment's (or inline part's) content type, filename and bytes.
    pub fn attachment(&self, index: usize) -> Option<(String, String, Vec<u8>)> {
        let parsed = MessageParser::default().parse(&self.raw)?;
        let part = parsed.attachments().nth(index)?;
        Some((content_type(&part), part_name(&part, index), part.decoded().into_owned()))
    }

    /// The index of the attachment with this Content-ID.
    pub fn attachment_by_cid(&self, cid: &str) -> Option<usize> {
        let parsed = MessageParser::default().parse(&self.raw)?;
        let cid = cid.trim_matches(['<', '>']);
        parsed.attachments().position(|p| p.content_id().is_some_and(|c| c.trim_matches(['<', '>']) == cid))
    }
}

/// The plain text body, if the message has one. (mail-parser's text body of an HTML-only message
/// is its HTML part, and its HTML body of a text-only message is its text part.)
fn text_part(m: &mail_parser::Message) -> Option<String> {
    m.text_body().find(|p| !p.is_content_type("text", "html")).and_then(|p| p.text()).map(|t| t.into_owned())
}

/// The start of the text body (or the HTML's text), on one line.
fn preview(m: &mail_parser::Message) -> String {
    let text = text_part(m).or_else(|| html_part(m).map(|h| html_to_text(&h))).unwrap_or_default();
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(240) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line,
    }
}

/// Readable text from HTML: block elements become line breaks, other tags spaces, so words and
/// table cells don't run together.
pub fn html_to_text(html: &str) -> String {
    static HIDDEN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BLOCK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let hidden = HIDDEN.get_or_init(|| {
        regex::Regex::new(r"(?is)<(style|script|head|title)\b.*?</(style|script|head|title)\s*>|<!--.*?-->").unwrap()
    });
    let block = BLOCK.get_or_init(|| {
        regex::Regex::new(
            r"(?i)<br\s*/?>|</?(p|div|tr|li|ul|ol|table|h[1-6]|blockquote|section|article|header|footer)\b[^>]*>",
        )
        .unwrap()
    });
    let tag = TAG.get_or_init(|| regex::Regex::new(r"(?s)<[^>]*>").unwrap());
    let text = hidden.replace_all(html, " ");
    let text = block.replace_all(&text, "\n");
    let text = tag.replace_all(&text, " ");
    let text = crate::extract::decode_entities(&text);
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn html_part(m: &mail_parser::Message) -> Option<String> {
    m.html_body().find(|p| p.is_content_type("text", "html")).and_then(|p| p.text()).map(|t| t.into_owned())
}

fn unfold(v: &str) -> String {
    v.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

fn content_type(part: &mail_parser::MessagePart) -> String {
    part.content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
        .to_ascii_lowercase()
}

fn part_name(part: &mail_parser::MessagePart, index: usize) -> String {
    part.attachment_name().map(String::from).unwrap_or_else(|| format!("attachment-{}", index + 1))
}

fn attachments(m: &mail_parser::Message) -> Vec<AttachmentInfo> {
    m.attachments()
        .enumerate()
        .map(|(index, p)| AttachmentInfo {
            index,
            filename: part_name(&p, index),
            content_type: content_type(&p),
            size: p.decoded_len(),
            content_id: p.content_id().map(|c| c.trim_matches(['<', '>']).to_string()),
            inline: p.content_id().is_some() && !p.content_disposition().is_some_and(|d| d.is_attachment()),
        })
        .collect()
}

/// Addresses from a provider field that's either a string (possibly comma-separated) or a list of
/// strings.
pub fn addrs_from_value(v: Option<&Value>) -> Vec<Addr> {
    match v {
        Some(Value::String(s)) => parse_list(s),
        Some(Value::Array(items)) => items
            .iter()
            .flat_map(|i| match i {
                Value::String(s) => parse_list(s),
                _ => vec![],
            })
            .collect(),
        _ => vec![],
    }
}
