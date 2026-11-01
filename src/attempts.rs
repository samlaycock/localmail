//! The attempts log: every send, accepted or not, over SMTP and every provider API. A middleware
//! records the provider routes; their handlers attach an [`AttemptNote`] saying what the request
//! was (sender, recipients, the messages stored, why it failed). The SMTP server records its
//! transactions directly. It lives in memory only.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::message::Provider;
use crate::state::AppState;
use crate::util::now_ms;

/// How many attempts are kept.
pub const CAPACITY: usize = 1000;
/// Request bodies longer than this are cut short in the record.
const MAX_RECORDED_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub id: u64,
    /// Real time, in Unix milliseconds.
    pub at: i64,
    pub duration_ms: u64,
    pub provider: Provider,
    /// e.g. `POST /v3/mail/send` or `SMTP`.
    pub endpoint: String,
    /// HTTP status, or the SMTP reply code.
    pub status: u16,
    /// `accepted`, `partial` (some messages of a batch failed) or `failed`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub recipients: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Ids of the messages stored.
    pub messages: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The request body (shortened), or for SMTP a transcript of the envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// The response body (shortened).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

impl Attempt {
    pub fn failed(&self) -> bool {
        self.outcome != "accepted"
    }
}

/// Attached by provider handlers to their responses.
#[derive(Debug, Clone)]
pub struct AttemptNote {
    pub provider: Provider,
    pub endpoint: String,
    pub from: Option<String>,
    pub recipients: Vec<String>,
    pub subject: Option<String>,
    pub messages: Vec<i64>,
    pub scenario: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
    pub warnings: Vec<String>,
    /// Some messages of a batch were stored and some weren't.
    pub partial: bool,
}

impl AttemptNote {
    pub fn new(provider: Provider, endpoint: &str) -> Self {
        Self {
            provider,
            endpoint: endpoint.into(),
            from: None,
            recipients: vec![],
            subject: None,
            messages: vec![],
            scenario: None,
            error: None,
            error_description: None,
            warnings: vec![],
            partial: false,
        }
    }

    pub fn attach(self, response: &mut Response) {
        response.extensions_mut().insert(self);
    }
}

#[derive(Debug, Default)]
pub struct Attempts {
    inner: Mutex<Log>,
}

#[derive(Debug, Default)]
struct Log {
    next_id: u64,
    entries: VecDeque<Attempt>,
}

#[derive(Debug, Default)]
pub struct Filter {
    pub provider: Option<Provider>,
    /// Any recipient, case-insensitively.
    pub to: Option<String>,
    pub errors_only: bool,
    /// Only attempts newer than this id.
    pub after: Option<u64>,
    pub limit: Option<usize>,
}

impl Attempts {
    pub fn record(&self, mut attempt: Attempt) -> u64 {
        let mut log = self.inner.lock().unwrap();
        log.next_id += 1;
        attempt.id = log.next_id;
        log.entries.push_back(attempt);
        if log.entries.len() > CAPACITY {
            log.entries.pop_front();
        }
        log.next_id
    }

    /// Matching attempts, newest first.
    pub fn list(&self, filter: &Filter) -> Vec<Attempt> {
        let log = self.inner.lock().unwrap();
        log.entries
            .iter()
            .rev()
            .filter(|e| filter.after.is_none_or(|after| e.id > after))
            .filter(|e| filter.provider.is_none_or(|p| e.provider == p))
            .filter(|e| filter.to.as_ref().is_none_or(|to| e.recipients.iter().any(|r| r.eq_ignore_ascii_case(to))))
            .filter(|e| !filter.errors_only || e.failed())
            .take(filter.limit.unwrap_or(CAPACITY))
            .cloned()
            .collect()
    }

    /// The id of the newest attempt ever recorded (0 if none); clearing doesn't reset it.
    pub fn latest_id(&self) -> u64 {
        self.inner.lock().unwrap().next_id
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().entries.clear();
    }
}

fn shorten(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    // JSON keeps its shape with long strings (attachments) elided.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        let v = crate::util::elide_long_strings(&v);
        return Some(serde_json::to_string_pretty(&v).unwrap_or_default());
    }
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_RECORDED_BODY)]).into_owned();
    Some(if bytes.len() > MAX_RECORDED_BODY { format!("{text}… ({} bytes in all)", bytes.len()) } else { text })
}

/// Records every request whose handler attached an [`AttemptNote`].
pub async fn record(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let limit = state.body_limit();
    let bytes = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("localmail: request body over {limit} bytes (see max_message_size)"),
            )
                .into_response();
        }
    };
    let request = shorten(&bytes);
    let resp = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    let Some(note) = resp.extensions().get::<AttemptNote>().cloned() else { return resp };

    // Keep a copy of the (small) response body for the record.
    let (parts, body) = resp.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap_or_default();
    let status = parts.status.as_u16();
    let outcome = if status >= 400 {
        "failed"
    } else if note.partial {
        "partial"
    } else {
        "accepted"
    };
    let (error, error_description) = if status >= 400 && note.error.is_none() {
        (Some(format!("http_{status}")), None)
    } else {
        (note.error, note.error_description)
    };
    state.attempts.record(Attempt {
        id: 0,
        at: now_ms(),
        duration_ms: started.elapsed().as_millis() as u64,
        provider: note.provider,
        endpoint: note.endpoint,
        status,
        outcome,
        from: note.from,
        recipients: note.recipients,
        subject: note.subject,
        messages: note.messages,
        scenario: note.scenario,
        error,
        error_description,
        warnings: note.warnings,
        request,
        response: shorten(&body),
    });
    Response::from_parts(parts, Body::from(body))
}
